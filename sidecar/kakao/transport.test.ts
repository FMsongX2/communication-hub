import { afterEach, describe, expect, test } from 'bun:test';
import { mkdtempSync, rmSync, writeFileSync, readFileSync, statSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { createHash } from 'node:crypto';
import { createConnection, createServer } from 'node:net';
import { KakaoTalkClient } from '@communication-hub/agent-messenger-kakao/client';
import { Store } from './store';
import { Sender } from './send';
import { Receiver } from './receive';
import { serve, hubRequest } from './ipc';
import { Config, Client, Message, SDKReceipt, MAX_FRAME } from './types';
const dirs:string[]=[];const stores:Store[]=[];
afterEach(()=>{for(const s of stores.splice(0))try{s.close();}catch{};for(const d of dirs.splice(0))rmSync(d,{recursive:true,force:true});});
function setup(mode:'shadow'|'active'='active') {
 const dir=mkdtempSync(join(tmpdir(),'hub-loco-test-'));dirs.push(dir);
 const config:Config={socket_path:join(dir,'loco.sock'),hub_socket:join(dir,'hub.sock'),state_dir:dir,expected_user_id:'42',credential_service:'test',credential_account:'42',allowed_chat_ids:['100'],mode,attachment_roots:[dir]};
 const store=new Store(dir,'42');stores.push(store);let writes=0;
 const ack:SDKReceipt={success:true,status_code:0,chat_id:'100',log_id:'200',sent_at:1000};
 const client:Client={getCredentials:()=>({userId:'42'}),isConnected:()=>true,acquireSession:async()=>({}),close:()=>{},getChats:async()=>[],getLatestLogId:async()=>'10',getMessagePage:async()=>({messages:[],next_cursor:null,complete:true}),sendMessage:async()=>{writes++;return ack;},sendAttachment:async()=>{writes++;return {...ack,log_id:'201'};},onSessionEvent:()=>()=>{}};
 const p={delivery_id:'delivery-1',expected_user_id:'42',chat_id:'100',reply:'[System-유이] : test',expires_at:Date.now()/1000+120};
 return {dir,config,store,client,p,writes:()=>writes,ack};
}
function msg(log:string,body='@[유이] hello',author:number|string=42):Omit<Message,'chat_id'> {return {log_id:log,author_id:author,message:body,sent_at:1000};}

describe('durable send boundary',()=>{
 test('same ID never dispatches twice, including after restart',async()=>{
  const s=setup();const sender=new Sender(s.config,s.client,s.store,()=>true,async()=>true);
  expect((await sender.send(s.p)).status).toBe('sent_verified');
  expect((await sender.send(s.p)).text_log_id).toBe('200');
  s.store.close();const restored=new Store(s.dir,'42');stores.push(restored);
  expect((await new Sender(s.config,s.client,restored,()=>true,async()=>true).send(s.p)).status).toBe('sent_verified');expect(s.writes()).toBe(1);
 });
 test('concurrent duplicate requests share journal outcome',async()=>{
  const s=setup();const sender=new Sender(s.config,s.client,s.store,()=>true,async()=>true);const result=await Promise.all([sender.send(s.p),sender.send(s.p)]);
  expect(result.map(r=>r.status)).toEqual(['sent_verified','sent_verified']);expect(s.writes()).toBe(1);
 });
 test('wrong account, unapproved room, shadow and expiry are held before writes',async()=>{
  const s=setup();const sender=new Sender(s.config,s.client,s.store,()=>true,async()=>true);
  expect((await sender.send({...s.p,expected_user_id:'43'})).reason).toBe('wrong_account');
  expect((await sender.send({...s.p,chat_id:'101'})).reason).toBe('room_not_approved');
  expect((await sender.send({...s.p,expires_at:1})).reason).toBe('expired');
  expect((await new Sender({...s.config,mode:'shadow'},s.client,s.store,()=>true,async()=>true).send(s.p)).reason).toBe('shadow_mode');expect(s.writes()).toBe(0);
 });
 test('malformed ACK quarantines delivery with no replay',async()=>{
  const s=setup();let attempts=0;s.client.sendMessage=async()=>{attempts++;return {...s.ack,log_id:'0'};};
  const sender=new Sender(s.config,s.client,s.store,()=>true,async()=>true);expect((await sender.send(s.p)).status).toBe('sending_uncertain');
  expect((await sender.send(s.p)).status).toBe('sending_uncertain');expect(attempts).toBe(1);
 });
 test('pre-dispatch record is already durable when SDK starts and crash recovery cannot replay',async()=>{
  const s=setup();s.client.sendMessage=async()=>{expect(s.store.delivery(s.p.delivery_id)?.result.status).toBe('sending_uncertain');throw new Error('disconnect');};
  const result=await new Sender(s.config,s.client,s.store,()=>true,async()=>true).send(s.p);expect(result.side_effects_started).toBe(true);
  s.store.close();const restored=new Store(s.dir,'42');stores.push(restored);
  expect((await new Sender(s.config,s.client,restored,()=>true,async()=>true).send(s.p)).status).toBe('sending_uncertain');expect(s.writes()).toBe(0);
 });
 test('payload conflict including changed deadline never replays',async()=>{
  const s=setup();const sender=new Sender(s.config,s.client,s.store,()=>true,async()=>true);await sender.send(s.p);
  expect((await sender.send({...s.p,expires_at:s.p.expires_at+1})).reason).toBe('delivery_id_conflict');expect(s.writes()).toBe(1);
 });
 test('null optional attachment fields accepted',async()=>{
  const s=setup();expect((await new Sender(s.config,s.client,s.store,()=>true,async()=>true).send({...s.p,attachment_path:null,attachment_sha256:null})).status).toBe('sent_verified');
 });
 test('attachment hash mismatch prevents even text send',async()=>{
  const s=setup();const path=join(s.dir,'test.pdf');writeFileSync(path,'pdf');
  const result=await new Sender(s.config,s.client,s.store,()=>true,async()=>true).send({...s.p,attachment_path:path,attachment_sha256:'0'.repeat(64)});
  expect(result.reason).toBe('attachment_validation_failed');expect(s.writes()).toBe(0);
 });
 test('text receipt survives ambiguous file dispatch and replay',async()=>{
  const s=setup();const path=join(s.dir,'test.pdf');writeFileSync(path,'pdf');let files=0;
  s.client.sendAttachment=async()=>{files++;expect(s.store.delivery(s.p.delivery_id)?.result.text_log_id).toBe('200');throw new Error('no ack');};
  const p={...s.p,attachment_path:path,attachment_sha256:createHash('sha256').update('pdf').digest('hex')};
  const sender=new Sender(s.config,s.client,s.store,()=>true,async()=>true);const result=await sender.send(p);
  expect(result.status).toBe('sending_uncertain');expect(result.text_sent).toBe(true);expect(result.attachment_sent).toBe(false);
  expect((await sender.send(p)).text_log_id).toBe('200');expect(files).toBe(1);expect(s.writes()).toBe(1);
 });
 test('permission revoked between components produces partial_file_held',async()=>{
  const s=setup();const path=join(s.dir,'test.zip');writeFileSync(path,'zip');let permitted=true;
  s.client.sendMessage=async()=>{permitted=false;return s.ack;};
  const result=await new Sender(s.config,s.client,s.store,()=>permitted,async()=>true).send({...s.p,attachment_path:path,attachment_sha256:createHash('sha256').update('zip').digest('hex')});
  expect(result.status).toBe('partial_file_held');expect(result.text_sent).toBe(true);expect(result.attachment_sent).toBe(false);expect(s.writes()).toBe(0);
 });
 test('both server receipts persist separately',async()=>{
  const s=setup();const path=join(s.dir,'test.zip');writeFileSync(path,'zip');
  const result=await new Sender(s.config,s.client,s.store,()=>true,async()=>true).send({...s.p,attachment_path:path,attachment_sha256:createHash('sha256').update('zip').digest('hex')});
  expect(result.status).toBe('sent_verified');expect(result.text_log_id).toBe('200');expect(result.attachment_log_id).toBe('201');expect(s.store.isOutgoing('100','201')).toBe(true);
 });
});

describe('upstream no-replay patch, real SDK with fake transport',()=>{
 test('write exception after session invalidation invokes transport exactly once',async()=>{
  const s=setup();const client:any=new KakaoTalkClient(s.dir);let writes=0,reconnects=0;
  client.state={session:{sendMessage:async()=>{writes++;client.state=null;throw new Error('accepted then disconnected');}},loginResult:{}};
  client.ensureSession=async()=>{reconnects++;return client.state;};
  await expect(client.sendMessage('100','test')).rejects.toThrow();expect(writes).toBe(1);expect(reconnects).toBe(1);
 });
 test('media SHIP failure after invalidation is not replayed',async()=>{
  const s=setup();const client:any=new KakaoTalkClient(s.dir);await client.login({oauthToken:"fake-test-token",userId:"42",deviceUuid:"test-device"});let ships=0;
  client.state={session:{shipMedia:async()=>{ships++;client.state=null;throw new Error('transport failure');}},loginResult:{}};
  client.ensureSession=async()=>client.state;
  await expect(client.sendAttachment('100',Buffer.from('test'),'report.pdf')).rejects.toThrow();expect(ships).toBe(1);
 });
 test('explicit credentials only; no fallback account/cache import',async()=>{
  const s=setup();const client=new KakaoTalkClient(s.dir);await expect(client.login()).rejects.toThrow('Explicit credentials');
 });
});

describe('ordered receive and bootstrap',()=>{
 test('fresh cursor starts at snapshot, without historical message flood',async()=>{
  const s=setup();const emitted:string[]=[];let from='';s.client.getMessagePage=async(_,{from:f})=>{from=f;return {messages:[],complete:true,next_cursor:null};};
  const receiver=new Receiver(s.config,s.client,s.store,async e=>{emitted.push(e.log_id);return {ok:true,result:{durable:true,status:'queued'}};});
  await receiver.sync('100');expect(from).toBe('10');expect(emitted).toEqual([]);expect(s.store.cursor('100')).toBe('10');
 });
 test('push received during snapshot remains eligible, including self-authored call',async()=>{
  const s=setup();let receiver:Receiver;const emitted:string[]=[];
  s.client.getLatestLogId=async()=>{receiver.notify({chat_id:'100',log_id:'11'});return '12';};
  s.client.getMessagePage=async(_,{from})=>{expect(from).toBe('10');return {messages:[msg('12'),msg('11')],complete:true,next_cursor:'12'};};
  receiver=new Receiver(s.config,s.client,s.store,async e=>{emitted.push(e.log_id);return {ok:true,result:{durable:true,status:'queued'}};});
  await receiver.sync('100');expect(emitted).toEqual(['11','12']);expect(s.store.cursor('100')).toBe('12');
 });
 test('malformed/failed Hub ACK leaves cursor unchanged and replay is safely retried',async()=>{
  const s=setup();s.store.bootstrap('100','10');s.client.getMessagePage=async()=>({messages:[msg('11')],complete:true,next_cursor:'11'});
  let calls=0;const receiver=new Receiver(s.config,s.client,s.store,async()=>{calls++;return calls===1?{ok:false}:{ok:true,result:{durable:true,status:'duplicate'}};});
  await expect(receiver.sync('100')).rejects.toThrow('hub_ack_invalid');expect(s.store.cursor('100')).toBe('10');
  await receiver.sync('100');expect(calls).toBe(2);expect(s.store.cursor('100')).toBe('11');
 });
 test('unjournaled rejection preserves cursor until corrected Hub accepts the same message',async()=>{
  const s=setup();s.store.bootstrap('100','10');
  s.client.getMessagePage=async(_,{from})=>({messages:BigInt(from)<11n?[msg('11')]:[],complete:true,next_cursor:'11'});
  let mismatch=true,calls=0,processed=0;
  const receiver=new Receiver(s.config,s.client,s.store,async()=>{calls++;if(mismatch)return {ok:true,result:{status:'rejected',reason:'loco_mode_mismatch'}};processed++;return {ok:true,result:{durable:true,status:'queued'}};});
  await expect(receiver.sync('100')).rejects.toThrow('hub_ack_invalid');expect(s.store.cursor('100')).toBe('10');
  mismatch=false;await receiver.sync('100');expect(s.store.cursor('100')).toBe('11');expect(calls).toBe(2);expect(processed).toBe(1);
 });
 test('only journaled policy rejection is terminal',async()=>{
  const s=setup();s.store.bootstrap('100','10');s.client.getMessagePage=async()=>({messages:[msg('11')],complete:true,next_cursor:'11'});
  let durable=false;const receiver=new Receiver(s.config,s.client,s.store,async()=>({ok:true,result:{durable,status:'rejected'}}));
  await expect(receiver.sync('100')).rejects.toThrow('hub_ack_invalid');expect(s.store.cursor('100')).toBe('10');
  durable=true;await receiver.sync('100');expect(s.store.cursor('100')).toBe('11');
 });
 test('mixed multi-agent ACK is invalid; all definitive ACKs advance',async()=>{
  const s=setup();s.store.bootstrap('100','10');s.client.getMessagePage=async()=>({messages:[msg('11')],complete:true,next_cursor:'11'});
  let ok=false;const receiver=new Receiver(s.config,s.client,s.store,async()=>({ok:true,result:{durable:true,events:[{status:'queued'},{status:ok?'ignored':'unknown'}]}}));
  await expect(receiver.sync('100')).rejects.toThrow();expect(s.store.cursor('100')).toBe('10');ok=true;await receiver.sync('100');expect(s.store.cursor('100')).toBe('11');
 });
 test('ordinary disconnect backfills gap and duplicate pages do not redispatch',async()=>{
  const s=setup();s.store.bootstrap('100','10');let fail=true;const emitted:string[]=[];
  s.client.getMessagePage=async()=>{if(fail){fail=false;throw new Error('network');}return {messages:[msg('11'),msg('12')],complete:true,next_cursor:'12'};};
  const receiver=new Receiver(s.config,s.client,s.store,async e=>{emitted.push(e.log_id);return {ok:true,result:{durable:true,status:'shadow'}};});
  await expect(receiver.sync('100')).rejects.toThrow('network');await receiver.sync('100');await receiver.sync('100');expect(emitted).toEqual(['11','12']);
 });
 test('unapproved chat never persists; own system echo skipped but human system quote preserved',async()=>{
  const s=setup();const emitted:string[]=[];s.store.bootstrap('100','10');
  s.client.getMessagePage=async()=>({messages:[msg('11','[System-유이] : echo'),msg('12','[System-유이] : user quote',43),msg('13')],complete:true,next_cursor:'13'});
  const receiver=new Receiver(s.config,s.client,s.store,async e=>{emitted.push(e.log_id);return {ok:true,result:{durable:true,status:'queued'}};});
  receiver.notify({chat_id:'999',log_id:'11'});await receiver.sync('999');expect(s.store.cursor('999')).toBeNull();
  await receiver.sync('100');expect(emitted).toEqual(['12','13']);
 });
 test('bounded catch-up reports incomplete and does not skip unseen logs',async()=>{
  const s=setup();s.store.bootstrap('100','10');let pages=0;
  s.client.getMessagePage=async(_,{from})=>{pages++;const log=(BigInt(from)+1n).toString();return {messages:[msg(log)],complete:false,next_cursor:log};};
  const receiver=new Receiver(s.config,s.client,s.store,async()=>({ok:true,result:{durable:true,status:'queued'}}));
  await expect(receiver.sync('100')).rejects.toThrow('catchup_limit');expect(pages).toBe(10);expect(s.store.cursor('100')).toBe('20');
  await expect(receiver.sync('100')).rejects.toThrow('catchup_limit');expect(pages).toBe(10);
 });
});

function request(path:string,value:unknown):Promise<any> {return new Promise((resolve,reject)=>{const socket=createConnection(path);let text='';socket.on('error',reject);socket.on('connect',()=>socket.write(JSON.stringify(value)+'\n'));socket.on('data',chunk=>text+=chunk);socket.on('end',()=>{try{resolve(JSON.parse(text));}catch(e){reject(e);}});});}
describe('local persistent IPC',()=>{
 test('socket stays alive for separate requests and permissions are private',async()=>{
  const s=setup();let count=0;const server=await serve(s.config.socket_path,async()=>({count:++count}));
  try{expect(statSync(s.config.socket_path).mode&0o777).toBe(0o600);expect((await request(s.config.socket_path,{id:'1',method:'status'})).result.count).toBe(1);expect((await request(s.config.socket_path,{id:'2',method:'status'})).result.count).toBe(2);expect(server.listening).toBe(true);}finally{await new Promise<void>(r=>server.close(()=>r()));}
 });
 test('oversized frame closes without invoking handler',async()=>{
  const s=setup();let calls=0;const server=await serve(s.config.socket_path,async()=>{calls++;return {};});
  try{await new Promise<void>((resolve,reject)=>{const socket=createConnection(s.config.socket_path);socket.on('connect',()=>socket.write('x'.repeat(MAX_FRAME+1)));socket.on('close',()=>resolve());socket.on('error',()=>{});});expect(calls).toBe(0);}finally{await new Promise<void>(r=>server.close(()=>r()));}
 });
 test('hub request waits for full newline ACK',async()=>{
  const s=setup();const server=createServer(socket=>{socket.on('data',()=>{socket.write('{"ok":true,');setTimeout(()=>socket.end('"result":{"durable":true,"status":"queued"}}\n'),5);});});
  await new Promise<void>(r=>server.listen(s.config.hub_socket,r));
  try{expect(await hubRequest(s.config.hub_socket,{})).toEqual({ok:true,result:{durable:true,status:'queued'}});}finally{await new Promise<void>(r=>server.close(()=>r()));}
 });
});

describe('runtime lifecycle with injected credentials and SDK',()=>{
 test('one persistent client serves multiple connections and KICKOUT is terminal',async()=>{
  const {startSidecar}=await import('./runtime');const s=setup('shadow');s.config.allowed_chat_ids=[];
  let creations=0,acquires=0,closed=0;let event:(event:{type:'connected'|'disconnected'|'kicked';reason?:string})=>void=()=>{};
  s.client.onSessionEvent=handler=>{event=handler;return ()=>{};};s.client.acquireSession=async()=>{acquires++;return {};};s.client.close=()=>{closed++;};
  const app=await startSidecar(s.config,{credentials:async()=>({oauthToken:'fake',userId:'42',deviceUuid:'test',deviceType:'tablet'}),createClient:async()=>{creations++;return s.client;},createListener:()=>({start:async()=>{},stop:()=>{},on:()=>{}}),lock:()=>()=>{}});
  try {
    expect((await request(s.config.socket_path,{id:'1',method:'status'})).result.status).toBe('ready');
    expect((await request(s.config.socket_path,{id:'2',method:'list_rooms'})).result.rooms).toEqual([]);
    expect(creations).toBe(1);expect(acquires).toBe(1);event({type:'kicked',reason:'device takeover'});
    expect((await request(s.config.socket_path,{id:'3',method:'status'})).result.reason).toBe('kicked');
    expect((await request(s.config.socket_path,{id:'4',method:'send',params:s.p})).result.status).toBe('held');expect(s.writes()).toBe(0);expect(closed).toBe(1);
  }finally{app.close();}
 });
 test('ordinary session disconnection reconnects the same client',async()=>{
  const {startSidecar}=await import('./runtime');const s=setup('shadow');s.config.allowed_chat_ids=[];let acquires=0;
  let event:(event:{type:'connected'|'disconnected'|'kicked'})=>void=()=>{};
  s.client.onSessionEvent=handler=>{event=handler;return ()=>{};};s.client.acquireSession=async()=>{acquires++;return {};};
  const app=await startSidecar(s.config,{credentials:async()=>({oauthToken:'fake',userId:'42',deviceUuid:'test',deviceType:'tablet'}),createClient:async()=>s.client,createListener:()=>({start:async()=>{},stop:()=>{},on:()=>{}}),lock:()=>()=>{}});
  try {event({type:'disconnected'});expect(app.status().status).toBe('held');await Bun.sleep(1100);expect(acquires).toBe(2);expect(app.status().status).toBe('ready');}finally{app.close();}
 });
 test('expired token stays auth_required and never reconnects automatically',async()=>{
  const {startSidecar}=await import('./runtime');const s=setup('shadow');s.config.allowed_chat_ids=[];let acquires=0;
  s.client.acquireSession=async()=>{acquires++;throw Object.assign(new Error('expired'),{code:'invalid_access_token',serverStatus:-950});};
  const app=await startSidecar(s.config,{credentials:async()=>({oauthToken:'fake',userId:'42',deviceUuid:'test',deviceType:'tablet'}),createClient:async()=>s.client,createListener:()=>({start:async()=>{},stop:()=>{},on:()=>{}}),lock:()=>()=>{}});
  try{expect((await request(s.config.socket_path,{id:'1',method:'status'})).result.reason).toBe('auth_required');await Bun.sleep(1100);expect(acquires).toBe(1);}finally{app.close();}
 });
});

describe('dispatch-time Hub authorization',()=>{
 test('pause while session is acquired is checked before text dispatch',async()=>{
  const s=setup();let paused=false,authorizations=0;
  s.client.acquireSession=async()=>{paused=true;return {};};
  const sender=new Sender(s.config,s.client,s.store,()=>true,async request=>{authorizations++;expect(request.component).toBe('text');return !paused;});
  const result=await sender.send(s.p);
  expect(result.status).toBe('held');expect(result.side_effects_started).toBe(false);expect(authorizations).toBe(1);expect(s.writes()).toBe(0);expect(s.store.delivery(s.p.delivery_id)).toBeNull();
 });
 test('approval revoked after text ACK blocks file and persists partial receipt',async()=>{
  const s=setup();const path=join(s.dir,'test.pdf');writeFileSync(path,'pdf');const components:string[]=[];
  const sender=new Sender(s.config,s.client,s.store,()=>true,async request=>{components.push(request.component);return request.component==='text';});
  const params={...s.p,attachment_path:path,attachment_sha256:createHash('sha256').update('pdf').digest('hex')};
  const result=await sender.send(params);
  expect(components).toEqual(['text','attachment']);expect(result.status).toBe('partial_file_held');expect(result.text_log_id).toBe('200');expect(result.attachment_sent).toBe(false);expect(s.writes()).toBe(1);
  expect((await sender.send(params)).status).toBe('partial_file_held');expect(s.writes()).toBe(1);
 });
 test('missing authorization provider and failed Hub RPC fail closed',async()=>{
  const s=setup();expect((await new Sender(s.config,s.client,s.store).send(s.p)).status).toBe('held');
  const sender=new Sender(s.config,s.client,s.store,()=>true,async()=>{throw new Error('Hub down');});
  expect((await sender.send(s.p)).status).toBe('held');expect(s.writes()).toBe(0);
 });
});
