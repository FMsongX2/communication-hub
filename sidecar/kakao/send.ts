import { createHash } from 'node:crypto';
import { constants } from 'node:fs';
import { open, realpath } from 'node:fs/promises';
import { basename, relative, isAbsolute } from 'node:path';
import { Client, Config, MAX_ATTACHMENT, Receipt, SDKReceipt, SendParams, SendSchema, positiveId } from './types';
import { Store } from './store';
const sha=(value:string|Uint8Array)=>createHash('sha256').update(value).digest('hex');

async function attachment(config:Config,p:SendParams):Promise<{data:Buffer;name:string}|undefined> {
  if(!p.attachment_path)return;
  const path=await realpath(p.attachment_path);
  const roots=await Promise.all(config.attachment_roots.map(r=>realpath(r)));
  if(!roots.some(root=>{const rel=relative(root,path);return rel!==''&&!rel.startsWith('..')&&!isAbsolute(rel);}))throw new Error('attachment_outside_roots');
  const file=await open(path,constants.O_RDONLY|constants.O_NOFOLLOW);
  try {
    const stat=await file.stat();
    if(!stat.isFile()||stat.size<1||stat.size>MAX_ATTACHMENT)throw new Error('attachment_size');
    const data=await file.readFile();
    if(data.length!==stat.size||data.length>MAX_ATTACHMENT||sha(data)!==p.attachment_sha256)throw new Error('attachment_hash');
    return {data,name:basename(path)};
  } finally {await file.close();}
}
const validAck=(r:SDKReceipt,chat:string)=>r?.success===true&&r.status_code===0&&r.chat_id===chat&&positiveId(r.log_id)&&Number.isSafeInteger(r.sent_at)&&r.sent_at>0;

export class Sender {
  private tail:Promise<unknown>=Promise.resolve();
  constructor(readonly config:Config,readonly client:Client,readonly store:Store,readonly permitted:()=>boolean=()=>true,readonly authorize:(request:{delivery_id:string;user_id:string;chat_id:string;component:'text'|'attachment'})=>Promise<boolean>=async()=>false) {}
  send(raw:unknown):Promise<Receipt> {
    const job=this.tail.then(()=>this.execute(raw));this.tail=job.catch(()=>{});return job;
  }
  private async execute(raw:unknown):Promise<Receipt> {
    const p=SendSchema.parse(raw);
    const base:Receipt={status:'held',transport:'loco',user_id:this.config.expected_user_id,chat_id:p.chat_id,text_sent:false,attachment_sent:false,input_started:false,side_effects_started:false};
    const held=(reason:string):Receipt=>({...base,reason});
    if(p.expected_user_id!==this.config.expected_user_id||this.client.getCredentials().userId!==this.config.expected_user_id)return held('wrong_account');
    if(!this.config.allowed_chat_ids.includes(p.chat_id))return held('room_not_approved');
    const fingerprint=sha(JSON.stringify(p));const old=this.store.delivery(p.delivery_id);
    if(old)return old.fingerprint===fingerprint?old.result:held('delivery_id_conflict');
    if(this.config.mode!=='active')return held('shadow_mode');
    if(!this.permitted())return held('transport_not_ready');
    if(p.expires_at<=Date.now()/1000)return held('expired');
    let file;
    try {file=await attachment(this.config,p);}catch{return held('attachment_validation_failed');}
    if(p.expires_at<=Date.now()/1000)return held('expired');
    // Acquire before dispatch. Failures here are known not to have written anything.
    try {await this.client.acquireSession();}catch{return held('not_connected');}
    if(!this.permitted()||p.expires_at<=Date.now()/1000)return held('transport_not_ready_or_expired');
    const allowed=async(component:'text'|'attachment')=>{
      try{return await this.authorize({delivery_id:p.delivery_id,user_id:this.config.expected_user_id,chat_id:p.chat_id,component})===true;}catch{return false;}
    };
    if(!await allowed(p.reply?'text':'attachment'))return held('hub_authorization_denied_or_unavailable');
    if(!this.permitted()||p.expires_at<=Date.now()/1000)return held('transport_not_ready_or_expired');
    let result:Receipt={...base,status:'sending_uncertain',reason:'dispatch_in_flight',input_started:true,side_effects_started:true};
    this.store.begin(p.delivery_id,fingerprint,result); // fsynced before any write
    try {
      if(p.reply) {
        const ack=await this.client.sendMessage(p.chat_id,p.reply);
        if(!validAck(ack,p.chat_id))throw new Error('invalid_text_ack');
        result={...result,text_sent:true,text_log_id:ack.log_id};this.store.receipt(p.delivery_id,result);
      }
      if(file) {
        if(p.expires_at<=Date.now()/1000||!this.permitted()||(!!p.reply&&!await allowed('attachment'))||p.expires_at<=Date.now()/1000||!this.permitted()) {
          result={...result,status:result.text_sent?'partial_file_held':'held',reason:'attachment_not_dispatched'};
          this.store.receipt(p.delivery_id,result);return result;
        }
        const ack=await this.client.sendAttachment(p.chat_id,file.data,file.name);
        if(!validAck(ack,p.chat_id))throw new Error('invalid_attachment_ack');
        result={...result,attachment_sent:true,attachment_log_id:ack.log_id};this.store.receipt(p.delivery_id,result);
      }
      result={...result,status:'sent_verified',reason:undefined};
    } catch {
      // Attachment failures after dispatch are ambiguous even if text is verified.
      result={...result,status:'sending_uncertain',reason:result.text_sent?'attachment_dispatch_uncertain':'text_or_attachment_dispatch_uncertain'};
    }
    this.store.receipt(p.delivery_id,result);return result;
  }
}
