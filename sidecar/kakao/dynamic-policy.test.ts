import { afterEach, describe, expect, test } from 'bun:test';
import { createServer } from 'node:net';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Database } from 'bun:sqlite';
import { startSidecar, type Listener } from './runtime';
import { hubCall } from './ipc';
import { Store } from './store';
import type { Config, Client, HubEvent, Message } from './types';
const cleanup:Array<()=>void>=[];
afterEach(()=>{for(const stop of cleanup.splice(0).reverse())stop();});
const sleep=(ms:number)=>new Promise(resolve=>setTimeout(resolve,ms));
async function until(fn:()=>boolean){const end=Date.now()+3000;while(!fn()){if(Date.now()>end)throw new Error('condition_timeout');await sleep(5);}}
function deferred(){let release!:()=>void;const promise=new Promise<void>(resolve=>{release=resolve;});return {promise,release};}
const room=(chat_id:string,approval_epoch='1')=>({room_id:`kakao:${chat_id}`,chat_id,name:'Same display name',approval_epoch});
async function fixture(initial=[room('501')]) {
  const dir=mkdtempSync(join(tmpdir(),'hub-dynamic-'));cleanup.push(()=>rmSync(dir,{recursive:true,force:true}));
  const config:Config={socket_path:join(dir,'sidecar.sock'),hub_socket:join(dir,'hub.sock'),state_dir:dir,expected_user_id:'41',credential_service:'fake',credential_account:'fake',allowed_chat_ids:['900'],dynamic_room_policy:true,mode:'active',attachment_roots:[]};
  let policy:any={status:'ready',user_id:'41',mode:'active',revision:'1',rooms:initial};
  let unavailable=false;let eventGate:ReturnType<typeof deferred>|undefined;
  const accepted:HubEvent[]=[],incoming:HubEvent[]=[],authorized:any[]=[];
  const hub=createServer(socket=>{let buffer='';socket.on('error',()=>{});socket.on('data',chunk=>{
    buffer+=chunk.toString();if(!buffer.includes('\n'))return;
    const request=JSON.parse(buffer.split('\n')[0]);
    void (async()=>{
      let response:any;
      if(request.method==='loco_room_policy')response=unavailable?{ok:false}:{ok:true,result:policy};
      else if(request.method==='authorize_kakao_loco') {
        authorized.push(request.params);
        const allowed=policy.rooms.some((r:any)=>r.chat_id===request.params.chat_id&&r.approval_epoch===request.params.approval_epoch);
        response={ok:true,result:{status:allowed&&!unavailable?'authorized':'held'}};
      } else if(request.method==='ingest_kakao_loco') {
        incoming.push(request.event);await eventGate?.promise;
        const allowed=policy.rooms.some((r:any)=>r.chat_id===request.event.chat_id&&r.approval_epoch===request.event.approval_epoch);
        if(allowed&&!unavailable)accepted.push(request.event);
        response={ok:true,result:allowed?{durable:true,status:'queued'}:{status:'rejected',reason:'loco_approval_epoch_changed'}};
      } else throw new Error('unexpected_hub_call');
      socket.end(JSON.stringify(response)+'\n');
    })().catch(()=>socket.destroy());
  });});
  await new Promise<void>(resolve=>hub.listen(config.hub_socket,resolve));cleanup.push(()=>hub.close());
  const queries:string[]=[],watermarks:string[]=[],writes:string[]=[];
  let clients=0,catalogCalls=0,connected=true;let push:(m:Message)=>void=()=>{};
  const latest=new Map([['501','10'],['502','20']]);
  const history=new Map<string,Array<Omit<Message,'chat_id'>>>();
  let catalog=[{chat_id:'501',title:'Same display name',display_name:'ignore',type:'MultiChat',active_members:2},{chat_id:'502',title:'Same display name',display_name:'ignore',type:'MultiChat',active_members:3}];
  const client:Client={
    getCredentials:()=>({userId:'41'}),isConnected:()=>connected,acquireSession:async()=>{},close:()=>{connected=false;},
    getChats:async()=>{catalogCalls++;return catalog.map(r=>({...r,last_message:'MUST NEVER LEAK'}));},
    getLatestLogId:async chat=>{watermarks.push(chat);return latest.get(chat)??'0';},
    getMessagePage:async(chat,{from})=>{queries.push(chat);return {messages:(history.get(chat)??[]).filter(m=>BigInt(m.log_id)>BigInt(from)),next_cursor:null,complete:true};},
    sendMessage:async(chat)=>{writes.push(chat);return {success:true,status_code:0,chat_id:chat,log_id:String(800+writes.length),sent_at:1};},
    sendAttachment:async()=>{throw new Error('unexpected_attachment');},onSessionEvent:()=>()=>{},
  };
  const listener:Listener={start:async()=>{},stop:()=>{},on:(event:string,handler:any)=>{if(event==='message')push=handler;}};
  const deps={credentials:async()=>({oauthToken:'fake',userId:'41',deviceUuid:'synthetic',deviceType:'tablet' as const}),createClient:async()=>{clients++;connected=true;return client;},createListener:()=>listener,lock:()=>()=>{}};
  const start=async()=>{const app=await startSidecar(config,deps);cleanup.push(app.close);return app;};
  const rpc=(method:string,params:unknown={})=>hubCall(config.socket_path,{id:'synthetic',method,params});
  const message=(chat:string,log:string)=>({chat_id:chat,log_id:log,author_id:'43',message:'[유이] synthetic request',sent_at:1});
  const addMessage=(chat:string,log:string)=>{const m=message(chat,log);history.set(chat,[...(history.get(chat)??[]),m]);latest.set(chat,log);push(m);};
  const update=(rooms:ReturnType<typeof room>[])=>{policy={...policy,revision:String(Number(policy.revision)+1),rooms};};
  const cursor=(chat:string)=>{const db=new Database(join(dir,'transport.sqlite'),{readonly:true});try{return (db.query('SELECT log_id FROM cursors WHERE chat_id=?').get(chat) as {log_id:string}|null)?.log_id??null;}finally{db.close();}};
  const sendParams=(chat='501',epoch='1',id='delivery')=>({delivery_id:id,expected_user_id:'41',chat_id:chat,reply:'synthetic reply',expires_at:Date.now()/1000+60,approval_epoch:epoch});
  return {config,start,rpc,client,queries,watermarks,writes,accepted,incoming,authorized,latest,history,update,addMessage,cursor,sendParams,
    clients:()=>clients,catalogCalls:()=>catalogCalls,setUnavailable:(v:boolean)=>{unavailable=v;},setPolicy:(v:any)=>{policy=v;},getPolicy:()=>policy,
    setEventGate:(g:ReturnType<typeof deferred>)=>{eventGate=g;},setCatalog:(c:typeof catalog)=>{catalog=c;}};
}

describe('dynamic Hub-owned room policy over actual Unix IPC',()=>{
  test('startup ignores static fallback; add unseen room without restart then receive',async()=>{
    const f=await fixture();const app=await f.start();expect(f.queries).toEqual(['501']);expect(app.status().rooms?.[0].status).toBe('ready');
    f.update([room('501'),room('502')]);const refresh=await f.rpc('refresh_policy');expect(refresh.result.status).toBe('ready');
    await until(()=>app.status().rooms?.find(r=>r.chat_id==='502')?.status==='ready');
    f.addMessage('502','21');await until(()=>f.accepted.length===1);
    expect(f.accepted[0].approval_epoch).toBe('1');expect(f.cursor('502')).toBe('21');expect(f.clients()).toBe(1);expect(f.catalogCalls()).toBe(1);
  });
  test('delete blocks queued sends and diagnostic queries without reviving static config',async()=>{
    const f=await fixture();await f.start();const gate=deferred();let acquired=0;
    f.client.acquireSession=async()=>{acquired++;await gate.promise;};
    const a=f.rpc('send',f.sendParams());await until(()=>acquired===1);
    const b=f.rpc('send',f.sendParams('501','1','second'));await sleep(20);
    f.update([]);expect((await f.rpc('refresh_policy')).ok).toBe(true);gate.release();
    expect((await a).result.status).toBe('held');expect((await b).result.status).toBe('held');expect(f.writes).toEqual([]);
    expect((await f.rpc('message_page',{chat_id:'501',from:'0',count:10})).ok).toBe(false);
    f.addMessage('501','11');await sleep(80);expect(f.accepted).toEqual([]);expect(f.cursor('501')).toBeNull();
  });
  test('re-add creates a new watermark and never replays unapproved gap calls',async()=>{
    const f=await fixture();const app=await f.start();f.update([]);await f.rpc('refresh_policy');
    f.addMessage('501','11');f.update([room('501','2')]);await f.rpc('refresh_policy');
    await until(()=>app.status().rooms?.[0]?.status==='ready');expect(f.cursor('501')).toBe('11');expect(f.accepted).toEqual([]);
    expect((await f.rpc('send',f.sendParams('501','1'))).result.status).toBe('held');
    f.addMessage('501','12');await until(()=>f.accepted.length===1);expect(f.accepted[0].log_id).toBe('12');expect(f.accepted[0].approval_epoch).toBe('2');
  });
  test('revocation during page fetch rejects stale page before ingest or cursor advance',async()=>{
    const f=await fixture();await f.start();const gate=deferred();let fetching=false;
    const original=f.client.getMessagePage;f.client.getMessagePage=async(...args)=>{fetching=true;await gate.promise;return original(...args);};
    f.addMessage('501','11');await until(()=>fetching);f.update([]);await f.rpc('refresh_policy');gate.release();await sleep(80);
    expect(f.accepted).toEqual([]);expect(f.incoming).toEqual([]);expect(f.cursor('501')).toBeNull();
  });
  test('revocation during Hub ACK cannot advance cursor or dispatch the old epoch',async()=>{
    const f=await fixture();await f.start();const gate=deferred();f.setEventGate(gate);
    f.addMessage('501','11');await until(()=>f.incoming.length===1);f.update([]);await f.rpc('refresh_policy');gate.release();await sleep(80);
    expect(f.accepted).toEqual([]);expect(f.cursor('501')).toBeNull();
  });
  test('restart applies current Hub revocation and preserves completed delivery journal',async()=>{
    const f=await fixture();const first=await f.start();expect((await f.rpc('send',f.sendParams())).result.status).toBe('sent_verified');
    first.close();await sleep(25);f.update([]);const second=await f.start();
    expect(second.status().rooms).toEqual([]);expect(f.cursor('501')).toBeNull();
    const db=new Database(join(f.config.state_dir,'transport.sqlite'),{readonly:true});
    expect((db.query('SELECT COUNT(*) AS n FROM deliveries').get() as {n:number}).n).toBe(1);db.close();
    expect((await f.rpc('send',f.sendParams())).result.status).toBe('held');expect(f.writes).toHaveLength(1);
  });
  test('unchanged epoch restart retains cursor and backfills approved downtime',async()=>{
    const f=await fixture();const first=await f.start();first.close();await sleep(25);
    f.addMessage('501','11');const second=await f.start();expect(second.status().status).toBe('ready');
    expect(f.accepted.map(e=>e.log_id)).toEqual(['11']);expect(f.watermarks).toEqual(['501']);
  });
  test('Hub unavailable suspends all room access without static fallback; recovery retains cursor',async()=>{
    const f=await fixture();const app=await f.start();f.setUnavailable(true);expect((await f.rpc('refresh_policy')).ok).toBe(false);
    expect(app.status().policy_status).toBe('unavailable');expect(app.status().rooms).toEqual([]);
    expect((await f.rpc('send',f.sendParams())).result.status).toBe('held');expect(f.cursor('501')).toBe('10');
    f.addMessage('501','11');f.setUnavailable(false);await f.rpc('refresh_policy');await until(()=>f.accepted.length===1);
    expect(f.watermarks).toEqual(['501']);
  });
  for(const kind of ['foreign-account','wrong-mode','duplicate-id'])test(`${kind} policy fails closed`,async()=>{
    const f=await fixture();const app=await f.start();const p=f.getPolicy();
    f.setPolicy(kind==='foreign-account'?{...p,user_id:'77'}:kind==='wrong-mode'?{...p,mode:'shadow'}:{...p,rooms:[room('501'),room('501')]});
    expect((await f.rpc('refresh_policy')).ok).toBe(false);expect(app.status().rooms).toEqual([]);
    expect((await f.rpc('send',f.sendParams())).result.status).toBe('held');
  });
  test('catalog returns distinct IDs despite duplicate names, omits messages and rejects unknown/stale selections',async()=>{
    const f=await fixture();await f.start();const before=f.queries.length;
    const result=(await f.rpc('list_chats')).result;expect(result.rooms.map((r:any)=>r.chat_id)).toEqual(['501','502']);
    expect(result.rooms[0].name).toBe(result.rooms[1].name);expect(JSON.stringify(result)).not.toContain('MUST NEVER LEAK');
    expect((await f.rpc('resolve_chat',{chat_id:'502'})).result.chat_id).toBe('502');
    expect((await f.rpc('resolve_chat',{chat_id:'999'})).ok).toBe(false);
    f.setCatalog([{chat_id:'501',title:'Same display name',display_name:'ignore',type:'MultiChat',active_members:2}]);
    expect((await f.rpc('resolve_chat',{chat_id:'502'})).ok).toBe(false);
    expect(f.queries).toHaveLength(before);expect(f.writes).toEqual([]);expect(f.clients()).toBe(1);
  });
  test('one room failure leaves other rooms ready and receiving',async()=>{
    const f=await fixture([room('501'),room('502')]);const original=f.client.getLatestLogId;
    f.client.getLatestLogId=async chat=>{if(chat==='502')throw new Error('synthetic_room_failure');return original(chat);};
    const app=await f.start();expect(app.status().status).toBe('ready');expect(app.status().rooms?.find(r=>r.chat_id==='502')?.status).toBe('held');
    f.addMessage('501','11');await until(()=>f.accepted.length===1);expect(f.accepted[0].chat_id).toBe('501');
    f.update([room('501')]);await f.rpc('refresh_policy');expect(app.status().receive_issues).toEqual({});
  });
  test('deleting a room cancels its stalled sync wait so other rooms can keep receiving',async()=>{
    const f=await fixture([room('501'),room('502')]);const app=await f.start();const gate=deferred();cleanup.push(gate.release);
    const original=f.client.getMessagePage;let stalled=false;
    f.client.getMessagePage=async(...args)=>{if(args[0]==='502'){stalled=true;await gate.promise;}return original(...args);};
    f.addMessage('502','21');await until(()=>stalled);f.update([room('501')]);await f.rpc('refresh_policy');
    f.addMessage('501','11');await until(()=>f.accepted.length===1);
    expect(f.accepted[0].chat_id).toBe('501');expect(app.status().rooms?.map(r=>r.chat_id)).toEqual(['501']);
    gate.release();await sleep(20);expect(f.cursor('502')).toBeNull();
  });
  test('new approval reports sync_pending while its watermark is still being read',async()=>{
    const f=await fixture();const app=await f.start();const gate=deferred();cleanup.push(gate.release);
    const original=f.client.getLatestLogId;let started=false;
    f.client.getLatestLogId=async chat=>{if(chat==='502'){started=true;await gate.promise;}return original(chat);};
    f.update([room('501'),room('502')]);await f.rpc('refresh_policy');await until(()=>started);
    expect(app.status().rooms?.find(r=>r.chat_id==='502')?.status).toBe('sync_pending');
    expect(app.status().rooms?.find(r=>r.chat_id==='501')?.status).toBe('ready');
    gate.release();await until(()=>app.status().rooms?.find(r=>r.chat_id==='502')?.status==='ready');
  });
  test('startup with unreachable policy never queries static fallback rooms',async()=>{
    const f=await fixture();f.setUnavailable(true);const app=await f.start();
    expect(app.status().status).toBe('held');expect(f.queries).toEqual([]);expect(f.watermarks).toEqual([]);expect(f.catalogCalls()).toBe(0);
  });
  test('revocation during diagnostic message fetch discards returned data',async()=>{
    const f=await fixture();await f.start();const gate=deferred();cleanup.push(gate.release);let started=false;
    const original=f.client.getMessagePage;
    f.client.getMessagePage=async(...args)=>{started=true;await gate.promise;return original(...args);};
    const read=f.rpc('message_page',{chat_id:'501',from:'0',count:10});await until(()=>started);
    f.update([]);await f.rpc('refresh_policy');gate.release();expect((await read).ok).toBe(false);
  });
  test('uncertain delivery journal survives revocation and reapproval',async()=>{
    const f=await fixture();const app=await f.start();f.client.sendMessage=async()=>{throw new Error('synthetic uncertain write');};
    expect((await f.rpc('send',f.sendParams())).result.status).toBe('sending_uncertain');
    f.update([]);await f.rpc('refresh_policy');f.update([room('501','2')]);await f.rpc('refresh_policy');
    await until(()=>app.status().rooms?.[0]?.status==='ready');
    const db=new Database(join(f.config.state_dir,'transport.sqlite'),{readonly:true});
    expect(JSON.parse((db.query('SELECT result FROM deliveries WHERE id=?').get('delivery') as {result:string}).result).status).toBe('sending_uncertain');db.close();
  });
  test('five-second local policy repair applies deletion without catalog rescans',async()=>{
    const f=await fixture();const app=await f.start();f.update([]);
    const end=Date.now()+7000;while(app.status().rooms?.length){if(Date.now()>end)throw new Error('repair_timeout');await sleep(25);}
    expect(app.status().rooms).toEqual([]);expect(f.catalogCalls()).toBe(1);expect(f.queries).toEqual(['501']);
  },10000);

  test('first static-to-dynamic upgrade adopts only unchanged epoch-1 approved cursors',async()=>{
    const f=await fixture();f.config.allowed_chat_ids=['501','900'];
    const legacy=new Store(f.config.state_dir,'41');legacy.bootstrap('501','8');legacy.bootstrap('900','3');legacy.close();
    f.history.set('501',[{log_id:'9',author_id:'43',message:'request during upgrade',sent_at:1}]);
    await f.start();expect(f.accepted.map(e=>e.log_id)).toEqual(['9']);expect(f.watermarks).toEqual([]);
    expect(f.cursor('501')).toBe('9');expect(f.cursor('900')).toBeNull();
  });
  test('upgrade never adopts a cursor for a changed epoch or a non-static room',async()=>{
    const f=await fixture([room('501','2'),room('502')]);f.config.allowed_chat_ids=['501'];
    const legacy=new Store(f.config.state_dir,'41');legacy.bootstrap('501','8');legacy.bootstrap('502','8');legacy.close();
    f.history.set('501',[{log_id:'9',author_id:'43',message:'unapproved gap',sent_at:1}]);
    await f.start();expect(f.accepted).toEqual([]);expect(f.watermarks).toEqual(['501','502']);
    expect(f.cursor('501')).toBe('10');expect(f.cursor('502')).toBe('20');
  });
  test('Hub outage does not consume the one-time upgrade cursor adoption',async()=>{
    const f=await fixture();f.config.allowed_chat_ids=['501'];
    const legacy=new Store(f.config.state_dir,'41');legacy.bootstrap('501','8');legacy.close();
    f.setUnavailable(true);await f.start();expect(f.cursor('501')).toBe('8');
    f.history.set('501',[{log_id:'9',author_id:'43',message:'during Hub outage',sent_at:1}]);
    f.setUnavailable(false);await f.rpc('refresh_policy');await until(()=>f.accepted.length===1);
    expect(f.accepted[0].log_id).toBe('9');expect(f.watermarks).toEqual([]);
  });

  test('Hub revocation before local refresh rejects old ingress without advancing its cursor',async()=>{
    const f=await fixture();const app=await f.start();f.update([]);
    f.addMessage('501','11');await until(()=>app.status().receive_issues['501']==='hub_ack_invalid');
    expect(f.accepted).toEqual([]);expect(f.cursor('501')).toBe('10');
  });

});
