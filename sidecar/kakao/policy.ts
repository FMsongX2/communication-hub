import { z } from 'zod';
import { Config, positiveId } from './types';
import { Receiver } from './receive';
import { hubCall } from './ipc';
const snapshotSchema=z.object({
  status:z.literal('ready'),user_id:z.string().refine(positiveId),mode:z.enum(['active','shadow']),
  revision:z.string().min(1).max(128),
  rooms:z.array(z.object({room_id:z.string().min(1).max(256),chat_id:z.string().refine(positiveId),name:z.string().max(1024),approval_epoch:z.string().min(1).max(128)})).max(100),
});
/** Hub SQLite is authoritative. Config IDs never act as fallback in dynamic mode. */
export class RoomPolicy {
  healthy=false;
  revision:string|null=null;
  private tail:Promise<unknown>=Promise.resolve();
  private stopped=false;
  constructor(readonly config:Config,readonly receiver:Receiver,readonly approved:Set<string>,readonly changed:(rooms:string[],healthy:boolean)=>void,readonly initialStaticIds:ReadonlySet<string>=new Set()) {}
  close(){this.stopped=true;this.healthy=false;this.receiver.applyPolicy(new Map(),false);}
  refresh():Promise<void> {
    const next=this.tail.then(()=>this.pull());this.tail=next.catch(()=>{});return next;
  }
  private async pull() {
    if(this.stopped)throw new Error('sidecar_stopped');
    try {
      const ack=await hubCall(this.config.hub_socket,{method:'loco_room_policy',params:{user_id:this.config.expected_user_id}});
      if(this.stopped)throw new Error('sidecar_stopped');
      if(ack?.ok!==true)throw new Error('policy_unavailable');
      const policy=snapshotSchema.parse(ack.result);
      if(policy.user_id!==this.config.expected_user_id||policy.mode!==this.config.mode)throw new Error('policy_identity_mismatch');
      if(new Set(policy.rooms.map(r=>r.chat_id)).size!==policy.rooms.length||new Set(policy.rooms.map(r=>r.room_id)).size!==policy.rooms.length)throw new Error('policy_duplicate_binding');
      const changed=policy.rooms.filter(r=>!this.approved.has(r.chat_id)||this.receiver.approvalEpoch(r.chat_id)!==r.approval_epoch).map(r=>r.chat_id);
      this.receiver.applyPolicy(new Map(policy.rooms.map(r=>[r.chat_id,r.approval_epoch])),true,this.initialStaticIds);
      this.config.allowed_chat_ids=policy.rooms.map(r=>r.chat_id);
      this.approved.clear();for(const room of policy.rooms){this.approved.add(room.chat_id);this.receiver.setTitle(room.chat_id,room.name);}
      this.revision=policy.revision;this.healthy=true;this.changed(changed,true);
    } catch(error) {
      this.healthy=false;
      if(!this.stopped){this.config.allowed_chat_ids=[];this.approved.clear();this.receiver.applyPolicy(new Map(),false);this.changed([],false);}
      throw new Error('policy_unavailable');
    }
  }
}
