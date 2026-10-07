import { describe, expect, test } from 'bun:test';
import { resolve } from 'node:path';
import { KakaoTalkClient } from '../../third_party/agent-messenger/src/platforms/kakaotalk/client';
const chat=(id:number,name=`room-${id}`)=>({c:id,t:'MultiChat',k:[name],o:id});
const packet=(chatDatas:unknown[],eof:boolean,token=0,id=0)=>({statusCode:0,body:{chatDatas,eof,lastTokenId:token,lastChatId:id}});
type Page={statusCode:number;body:Record<string,unknown>}|Error;
/** Real SDK and reconnect wrapper; all session operations are offline mocks. */
function fixture(pages:Page[],loginChats=[chat(71),chat(72),chat(73)]) {
  const calls:string[][]=[];
  const session={
    getChannelInfo:async(id:{toString():string}):Promise<{statusCode:number;body:Record<string,unknown>}>=>({statusCode:0,body:{chatInfo:{chatId:Number(id.toString()),left:true}}}),
    getAllMembers:async()=>({statusCode:0,body:{members:[{userId:41,nickName:'self'},{userId:43,nickName:'peer'}]}}),
    getChatList:async(token:{toString():string},id:{toString():string})=>{
    calls.push([token.toString(),id.toString()]);const next=pages[calls.length-1];
    if(!next)throw new Error('unexpected catalog request');if(next instanceof Error)throw next;return next;
  }};
  const state={session,loginResult:{chatDatas:loginChats,eof:true,lastTokenId:9876,lastChatId:4321,delChatIds:[] as number[]},syncedChatIds:[] as string[]};
  const client=new KakaoTalkClient('/unused-offline-catalog-fixture');
  Object.assign(client,{state,userId:'41',ensureSession:async()=>state});return {client,calls,session,state};
}
describe('canonical full catalog independent of LOGINLIST delta',()=>{
  test('persisted login delta of three rooms with EOF returns all 73 synthetic canonical rooms',async()=>{
    const {client,calls}=fixture([packet(Array.from({length:73},(_,i)=>chat(i+1)),true)]);
    const result=await client.getChats({all:true});expect(result).toHaveLength(73);
    expect(new Set(result.map(c=>c.chat_id)).size).toBe(73);expect(calls).toEqual([['0','0']]);
  });
  test('canonical records supersede login metadata and exclude stale login rooms',async()=>{
    const {client}=fixture([packet([chat(1,'canonical')],true)],[chat(1,'stale'),chat(999,'left')]);
    const result=await client.getChats({all:true});expect(result.map(c=>c.chat_id)).toEqual(['1']);
    expect(result[0].display_name).toContain('canonical');
  });
  test('each full request starts at zero on the same client',async()=>{
    const {client,calls}=fixture([packet([chat(1)],true),packet([chat(2)],true)]);
    expect((await client.getChats({all:true})).map(c=>c.chat_id)).toEqual(['1']);
    expect((await client.getChats({all:true})).map(c=>c.chat_id)).toEqual(['2']);
    expect(calls).toEqual([['0','0'],['0','0']]);
  });
  test('pages follow numeric and BSON Long cursors and deduplicate overlaps',async()=>{
    const pages=[packet([chat(1),chat(2)],false,23,2),packet([chat(2),chat(3)],false),packet([chat(4)],true)];
    Object.assign(pages[1].body,{lastTokenId:{low:4,high:1},lastChatId:{low:3,high:0}});
    const {client,calls}=fixture(pages);
    expect((await client.getChats({all:true})).map(c=>c.chat_id)).toEqual(['4','3','2','1']);
    expect(calls).toEqual([['0','0'],['23','2'],['4294967300','3']]);
  });
  test('search including explicit empty string scans canonical pages before filtering',async()=>{
    for(const search of ['needle','']) {
      const {client,calls}=fixture([packet([chat(1)],false,2,1),packet([chat(2,'needle')],true)]);
      expect((await client.getChats({search})).map(c=>c.chat_id)).toEqual(search?['2']:['2','1']);expect(calls).toHaveLength(2);
    }
  });
  test('default nonempty login view remains partial without a network scan',async()=>{
    const {client,calls}=fixture([]);expect(await client.getChats()).toHaveLength(3);expect(calls).toHaveLength(0);
  });
  test('default empty login view fetches canonical rooms from zero',async()=>{
    const {client,calls}=fixture([packet([chat(1)],true)],[]);expect(await client.getChats()).toHaveLength(1);expect(calls).toEqual([['0','0']]);
  });
  test('canonical empty EOF is a valid empty catalog regardless of login entries',async()=>{
    const {client}=fixture([packet([],true)]);expect(await client.getChats({all:true})).toEqual([]);
  });
  const incomplete:Array<[string,Page[],string,number]>=[
    ['unchanged cursor',[packet([chat(1)],false)],'repeated cursor',1],
    ['cursor cycle',[packet([chat(1)],false,2,1),packet([chat(2)],false)],'repeated cursor',2],
    ['duplicate-only non-EOF page',[packet([chat(1)],false,2,1),packet([chat(1)],false,3,1)],'no new chats',2],
    ['empty non-EOF',[packet([],false)],'empty non-EOF',1],
    ['missing EOF',[{statusCode:0,body:{chatDatas:[chat(1)],lastTokenId:2,lastChatId:1}}],'EOF unavailable',1],
    ['missing cursor',[{statusCode:0,body:{chatDatas:[chat(1)],eof:false}}],'cursor unavailable',1],
    ['missing chatDatas',[{statusCode:0,body:{eof:true}}],'chatDatas unavailable',1],
    ['packet failure',[{statusCode:-321,body:{chatDatas:[],eof:true}}],'statusCode=-321',1],
    ['body failure',[{statusCode:0,body:{status:-322,chatDatas:[],eof:true}}],'body.status=-322',1],
    ['network failure',[new Error('fixture network failure')],'fixture network failure',1],
    ['page bound',Array.from({length:50},(_,i)=>packet([chat(i+1)],false,i+1,i+1)),'after 50 pages',50],
  ];
  for(const [name,pages,message,count] of incomplete)for(const options of [{all:true},{search:'needle'}]) {
    test(`${name} rejects incomplete ${options.all?'full':'search'} catalog`,async()=>{
      const {client,calls}=fixture(pages);await expect(client.getChats(options)).rejects.toThrow(message);expect(calls).toHaveLength(count);
    });
  }
  const current=(id:number,members=[{userId:41,nickName:'self'},{userId:43,nickName:'peer'}])=>({statusCode:0,body:{chatInfo:{chatId:id,type:'DirectChat',activeMembersCount:members.length,newMessageCount:0,displayMembers:members,chatMetas:[]}}});
  test('reconstructs current LOGINLIST first-page rooms omitted from zero-cursor continuation',async()=>{
    const {client,session}=fixture([packet(Array.from({length:70},(_,i)=>chat(i+1)),true)]);
    session.getChannelInfo=async id=>current(Number(id.toString()));
    const result=await client.getChats({all:true});expect(result).toHaveLength(73);
    expect(client.getCatalogDiagnostics()).toEqual({synced_chat_ids:[],validated_sync_only_chat_ids:[],excluded_sync_only_chat_ids:[],login_chat_ids:['71','72','73'],continuation_chat_ids:Array.from({length:70},(_,i)=>String(i+1)),validated_login_only_chat_ids:['71','72','73'],excluded_login_only_chat_ids:[],tombstone_chat_ids:[],returned_count:73,complete:true});
    expect(JSON.stringify(client.getCatalogDiagnostics())).not.toContain('chatInfo');
  });
  test('login-only room requires current authenticated-account membership',async()=>{
    const {client,session}=fixture([packet([chat(1)],true)],[chat(2)]);
    const others=[{userId:42,nickName:'other'},{userId:43,nickName:'peer'}];
    session.getChannelInfo=async id=>current(Number(id.toString()),others);
    session.getAllMembers=async()=>({statusCode:0,body:{members:others}});
    expect((await client.getChats({all:true})).map(c=>c.chat_id)).toEqual(['1']);
    expect(client.getCatalogDiagnostics()?.excluded_login_only_chat_ids).toEqual(['2']);
  });
  test('current tombstones remove continuation entries and skip login-only lookup',async()=>{
    const page=packet([chat(1),chat(2)],true);Object.assign(page.body,{delChatIds:[2,3]});
    const {client,session}=fixture([page],[chat(3)]);let reads=0;
    session.getChannelInfo=async()=>{reads++;throw new Error('unexpected lookup');};
    expect((await client.getChats({all:true})).map(c=>c.chat_id)).toEqual(['1']);expect(reads).toBe(0);
  });
  test('current login tombstone wins over stale first-page candidate',async()=>{
    const {client,state,session}=fixture([packet([chat(1)],true)],[chat(2)]);state.loginResult.delChatIds=[2];
    session.getChannelInfo=async()=>{throw new Error('unexpected lookup');};
    expect((await client.getChats({all:true})).map(c=>c.chat_id)).toEqual(['1']);
  });
  test('transient current-membership validation failure does not silently publish incomplete catalog',async()=>{
    const {client,session}=fixture([packet([chat(1)],true)],[chat(2)]);
    session.getChannelInfo=async()=>{throw new Error('offline transient failure');};
    await expect(client.getChats({all:true})).rejects.toThrow('offline transient failure');expect(client.getCatalogDiagnostics()).toBeNull();
  });
  test('login-only search uses current metadata after stable membership verification',async()=>{
    const {client,session}=fixture([packet([chat(1)],true)],[chat(2,'old label')]);
    session.getChannelInfo=async id=>current(Number(id.toString()),[{userId:41,nickName:'self'},{userId:43,nickName:'needle'}]);
    expect((await client.getChats({search:'needle'})).map(c=>c.chat_id)).toEqual(['2']);
  });

  test('empty login delta retains negotiated materialized IDs but hydrates missing rooms before inclusion',async()=>{
    const {client,session,state}=fixture([packet([chat(1),chat(2)],true)],[]);state.syncedChatIds=['1','2','3'];
    session.getChannelInfo=async id=>current(Number(id.toString()));
    expect((await client.getChats({all:true})).map(c=>c.chat_id).sort()).toEqual(['1','2','3']);
    expect(client.getCatalogDiagnostics()?.validated_sync_only_chat_ids).toEqual(['3']);
    expect(client.getCatalogDiagnostics()?.login_chat_ids).toEqual([]);
  });
  test('materialized missing IDs do not resurrect a current tombstone or left room',async()=>{
    const page=packet([chat(1)],true);Object.assign(page.body,{delChatIds:[2]});
    const {client,session,state}=fixture([page],[]);state.syncedChatIds=['1','2','3'];const queried:string[]=[];
    session.getChannelInfo=async id=>{queried.push(id.toString());return {statusCode:0,body:{chatInfo:{chatId:Number(id.toString()),left:true}}};};
    expect((await client.getChats({all:true})).map(c=>c.chat_id)).toEqual(['1']);expect(queried).toEqual(['3']);
    expect(client.getCatalogDiagnostics()?.excluded_sync_only_chat_ids).toEqual(['2','3']);
  });
  test('materialized missing ID without self membership cannot enter the catalog',async()=>{
    const {client,session,state}=fixture([packet([],true)],[]);state.syncedChatIds=['3'];
    const others=[{userId:42,nickName:'other'},{userId:43,nickName:'peer'}];
    session.getChannelInfo=async id=>current(Number(id.toString()),others);session.getAllMembers=async()=>({statusCode:0,body:{members:others}});
    expect(await client.getChats({all:true})).toEqual([]);
  });

  test('real connect retains only IDs negotiated by this session after delta and tombstones',async()=>{
    const root=resolve(import.meta.dir,'../../third_party/agent-messenger/src/platforms/kakaotalk');
    const source=`
      import {mock} from 'bun:test';
      import {mkdtempSync,rmSync} from 'node:fs';import {tmpdir} from 'node:os';import {join} from 'node:path';
      const negotiated=[];
      mock.module(${JSON.stringify(root+'/protocol/session.ts')},()=>({LocoSession:class{
        onPush(){} onClose(){} close(){}
        async login(token,user,device,state){negotiated.push(...state.chatIds.map(id=>id.low));return {chatDatas:[{c:4,t:'DirectChat',a:2,n:0}],delChatIds:[2],revision:2,lastTokenId:{low:9,high:0},lbk:0,eof:true};}
        async getChatList(){return {statusCode:0,body:{chatDatas:[{c:1,t:'DirectChat',a:2,n:0}],eof:true}};}
        async getChannelInfo(id){return {statusCode:0,body:{chatInfo:{chatId:Number(id.toString()),type:'DirectChat',activeMembersCount:2,newMessageCount:0,displayMembers:[{userId:41,nickName:'self'},{userId:43,nickName:'peer'}]}}};}
        async getAllMembers(){return {statusCode:0,body:{members:[{userId:41,nickName:'self'},{userId:43,nickName:'peer'}]}};}
      }}));
      const {KakaoSyncStateStore}=await import(${JSON.stringify(root+'/sync-state-store.ts')});
      const {KakaoTalkClient}=await import(${JSON.stringify(root+'/client.ts')});
      const dir=mkdtempSync(join(tmpdir(),'catalog-connect-'));
      try {
        await new KakaoSyncStateStore(dir).save('synthetic',{version:2,revision:1,chatIds:[1,2,3].map(low=>({low,high:0})),maxIds:[1,1,1].map(low=>({low,high:0})),lastTokenId:{low:1,high:0},lbk:0});
        const client=await new KakaoTalkClient(dir).login({oauthToken:'fake',userId:'41',deviceUuid:'synthetic'});
        const rooms=await client.getChats({all:true});
        console.log(JSON.stringify({negotiated,ids:rooms.map(room=>room.chat_id).sort(),diagnostics:client.getCatalogDiagnostics()}));client.close();
      }finally{rmSync(dir,{recursive:true,force:true});}
    `;
    const child=Bun.spawn([process.execPath,'--eval',source],{stdout:'pipe',stderr:'pipe',cwd:import.meta.dir});
    const stdout=await new Response(child.stdout).text();const stderr=await new Response(child.stderr).text();
    expect(await child.exited).toBe(0);expect(stderr).toBe('');const result=JSON.parse(stdout);
    expect(result.negotiated).toEqual([1,2,3]);expect(result.ids).toEqual(['1','3','4']);
    expect(result.diagnostics.synced_chat_ids).toEqual(['1','3','4']);
    expect(result.diagnostics.validated_sync_only_chat_ids).toEqual(['3']);
    expect(result.diagnostics.validated_login_only_chat_ids).toEqual(['4']);
  });

});
