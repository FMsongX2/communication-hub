import { describe, expect, test } from 'bun:test';
import { KakaoTalkClient } from '../../third_party/agent-messenger/src/platforms/kakaotalk/client';
const chat=(id:number,name=`room-${id}`)=>({c:id,t:'MultiChat',k:[name],o:id});
const packet=(chatDatas:unknown[],eof:boolean,token=0,id=0)=>({statusCode:0,body:{chatDatas,eof,lastTokenId:token,lastChatId:id}});
type Page={statusCode:number;body:Record<string,unknown>}|Error;
/** Real SDK and reconnect wrapper; all session operations are offline mocks. */
function fixture(pages:Page[],loginChats=[chat(71),chat(72),chat(73)]) {
  const calls:string[][]=[];
  const session={getChatList:async(token:{toString():string},id:{toString():string})=>{
    calls.push([token.toString(),id.toString()]);const next=pages[calls.length-1];
    if(!next)throw new Error('unexpected catalog request');if(next instanceof Error)throw next;return next;
  }};
  const state={session,loginResult:{chatDatas:loginChats,eof:true,lastTokenId:9876,lastChatId:4321}};
  const client=new KakaoTalkClient('/unused-offline-catalog-fixture');
  Object.assign(client,{state,ensureSession:async()=>state});return {client,calls};
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
});
