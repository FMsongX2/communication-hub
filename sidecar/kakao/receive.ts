import { Client, Config, HubEvent, Message, positiveId } from './types';
import { Store } from './store';
export type Ack = {ok:boolean;result?:{durable?:boolean;status?:string;events?:Array<{status?:string}>}};
export type Ingest = (event:HubEvent)=>Promise<Ack>;
const ACCEPTED=new Set(['queued','duplicate','ignored','shadow','rejected']);

/** Pushes wake forward pagination; page order, not delivery order, drives the durable cursor. */
export class Receiver {
  readonly issues=new Map<string,string>();
  private running=new Map<string,Promise<void>>();
  private pending=new Map<string,{min:bigint;max:bigint}>();
  private titles=new Map<string,string>();
  private scopes=new Map<string,{epoch:string;token:string;cancel:()=>void;cancelled:Promise<void>}>();
  private generation=0;
  private synced=new Set<string>();
  constructor(readonly config:Config,readonly client:Client,readonly store:Store,readonly ingest:Ingest) {
    if(!config.dynamic_room_policy)for(const chat of config.allowed_chat_ids)this.addScope(chat,'static');
  }
  private addScope(chat:string,epoch:string) {
    let cancel!:()=>void;const cancelled=new Promise<void>(resolve=>{cancel=resolve;});
    this.scopes.set(chat,{epoch,token:String(++this.generation),cancel,cancelled});
  }
  scope(chat:string) {return this.scopes.get(chat)?.token??null;}
  approvalEpoch(chat:string) {return this.config.dynamic_room_policy?this.scopes.get(chat)?.epoch:undefined;}
  roomReady(chat:string) {return this.scopes.has(chat)&&this.synced.has(chat)&&!this.issues.has(chat);}
  applyPolicy(epochs:ReadonlyMap<string,string>,persist=true,initialStaticIds:ReadonlySet<string>=new Set()) {
    if(persist)this.store.applyApprovalEpochs(epochs,initialStaticIds);
    for(const [chat,value] of this.scopes)if(epochs.get(chat)!==value.epoch) {
      value.cancel();this.scopes.delete(chat);this.pending.delete(chat);this.issues.delete(chat);this.titles.delete(chat);this.synced.delete(chat);
    }
    for(const [chat,epoch] of epochs)if(!this.scopes.has(chat))this.addScope(chat,epoch);
  }
  private assertScope(chat:string,token:string) {if(this.scope(chat)!==token)throw new Error('room_policy_changed');}
  setTitle(chat:string,title:string) {if(this.config.allowed_chat_ids.includes(chat))this.titles.set(chat,title);}
  notify(message:Pick<Message,'chat_id'|'log_id'>) {
    if(!this.scope(message.chat_id)||!positiveId(message.log_id))return;
    const log=BigInt(message.log_id);const previous=this.pending.get(message.chat_id);
    this.pending.set(message.chat_id,{min:previous&&previous.min<log?previous.min:log,max:previous&&previous.max>log?previous.max:log});
  }
  async sync(chat:string):Promise<void> {
    const token=this.scope(chat);if(!token)return;
    const key=`${chat}:${token}`;const running=this.running.get(key);if(running)return running;
    const work=Promise.race([this.catchUp(chat,token),this.scopes.get(chat)!.cancelled]).catch(error=>{if(this.scope(chat)===token)this.issues.set(chat,error instanceof Error?error.message:'receive_failed');throw error;});
    this.running.set(key,work);
    try {await work;}finally{this.running.delete(key);}
  }
  private async catchUp(chat:string,token:string) {
    this.assertScope(chat,token);
    if(this.issues.get(chat)==='catchup_limit')throw new Error('catchup_limit');
    if(this.store.cursor(chat)===null) {
      const latest=await this.client.getLatestLogId(chat);
      this.assertScope(chat,token);
      if(latest!=='0'&&!positiveId(latest))throw new Error('invalid_bootstrap_watermark');
      let baseline=BigInt(latest);
      // Static startup preserves pushes during CHATINFO. New dynamic approvals
      // begin at this watermark and cannot revive calls from an unapproved gap.
      const observed=this.pending.get(chat);
      if(!this.config.dynamic_room_policy&&observed&&observed.min<=baseline)baseline=observed.min-1n;
      if(this.config.dynamic_room_policy&&observed&&observed.max<=baseline)this.pending.delete(chat);
      this.store.bootstrap(chat,baseline.toString());
    }
    for(let pageNumber=0;pageNumber<10;pageNumber++) {
      this.assertScope(chat,token);
      const from=this.store.cursor(chat)!;
      const page=await this.client.getMessagePage(chat,{count:100,from});
      this.assertScope(chat,token);
      if(!page||!Array.isArray(page.messages)||page.messages.length>100||typeof page.complete!=='boolean')throw new Error('invalid_message_page');
      // Validate all IDs before ordering and committing any member of this page.
      if(page.messages.some(m=>!positiveId(m.log_id)))throw new Error('invalid_message_id');
      const ordered=[...page.messages].sort((a,b)=>BigInt(a.log_id)<BigInt(b.log_id)?-1:BigInt(a.log_id)>BigInt(b.log_id)?1:0);
      for(const message of ordered) {
        this.assertScope(chat,token);
        if(BigInt(message.log_id)<=BigInt(this.store.cursor(chat)!))continue;
        const author=typeof message.author_id==='number'&&Number.isSafeInteger(message.author_id)?String(message.author_id):message.author_id;
        if(!positiveId(author)||typeof message.message!=='string'||Buffer.byteLength(message.message)>65536||!Number.isSafeInteger(message.sent_at)||message.sent_at<=0)throw new Error('invalid_message');
        const echo=this.store.isOutgoing(chat,message.log_id)||(author===this.config.expected_user_id&&(message.message.startsWith('[System-유이] : ')||message.message.startsWith('[System-유미] : ')));
        if(!echo) {
          const ack=await this.ingest({mode:this.config.mode,user_id:this.config.expected_user_id,chat_id:chat,log_id:message.log_id,author_id:author,body:message.message,sent_at:message.sent_at,title:this.titles.get(chat)??chat,...(this.config.dynamic_room_policy?{approval_epoch:this.approvalEpoch(chat)}:{})});
          this.assertScope(chat,token);
          const statuses=ack.result?.events?.map(e=>e.status)??[ack.result?.status];
          if(ack?.ok!==true||ack.result?.durable!==true||statuses.length===0||!statuses.every(s=>ACCEPTED.has(s??'')))throw new Error('hub_ack_invalid');
        }
        // Ack is durable in Hub before this FULL synchronous SQLite commit.
        this.store.advance(chat,message.log_id);
      }
      const cursor=this.store.cursor(chat)!;
      const waiting=this.pending.get(chat);
      if(waiting&&BigInt(cursor)>=waiting.max)this.pending.delete(chat);
      if(page.complete) {
        if(this.pending.has(chat))throw new Error('push_not_yet_in_history');
        this.issues.delete(chat);this.synced.add(chat);return;
      }
      if(cursor===from||page.next_cursor!==cursor)throw new Error('pagination_no_progress');
    }
    throw new Error('catchup_limit');
  }
  async syncAll() {for(const chat of this.config.allowed_chat_ids)await this.sync(chat);}
}
