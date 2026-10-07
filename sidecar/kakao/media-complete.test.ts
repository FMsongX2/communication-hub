import { describe, expect, test } from 'bun:test';
import { resolve } from 'node:path';

/** Run mocks in a fresh process so upstream module mocks cannot leak into other suites. */
async function probe(scenario: string) {
  const clientPath=resolve(import.meta.dir,'../../third_party/agent-messenger/src/platforms/kakaotalk/client.ts');
  const uploaderPath=resolve(import.meta.dir,'../../third_party/agent-messenger/src/platforms/kakaotalk/protocol/media-uploader.ts');
  const source=`
    import { mock } from 'bun:test';
    const scenario=${JSON.stringify(scenario)};
    let uploads=0,forwards=0,ships=0;
    const complete={statusCode:scenario==='header-error'?-321:0,body:{status:scenario==='body-error'?-322:0,chatLog:{chatId:scenario==='wrong-chat'?'101':'100',logId:'999',sendAt:1000}}};
    const upload=async()=>{uploads++;return {completePacket:scenario==='missing-complete'?null:complete,postStatusCode:scenario==='post-error'?-323:0,postOffset:0};};
    mock.module(${JSON.stringify(uploaderPath)},()=>({uploadMediaToLoco:upload,uploadMultiMediaEntry:upload}));
    const {KakaoTalkClient}=await import(${JSON.stringify(clientPath)});
    const client=new KakaoTalkClient('/unused-offline-fixture');
    await client.login({oauthToken:'fixture',userId:'42',deviceUuid:'fixture',deviceType:'tablet'});
    client.ensureSession=async()=>({session:{
      shipMedia:async()=>{ships++;return {statusCode:0,body:{k:'fixture',vh:'fixture.invalid',p:1}};},
      shipMultiMedia:async()=>{ships++;return {statusCode:0,body:{kl:['a','b'],vhl:['fixture.invalid','fixture.invalid'],pl:[1,1]}};},
      forwardChat:async()=>{forwards++;return {statusCode:0,body:{status:scenario==='forward-body-error'?-324:0,logId:'999',sendAt:1000}};}
    }});
    if(scenario==='without-chat-id')delete complete.body.chatLog.chatId;
    if(scenario==='multi-body-error')complete.body.status=-322;
    const png=Buffer.alloc(24);Buffer.from([137,80,78,71,13,10,26,10]).copy(png);png.writeUInt32BE(1,16);png.writeUInt32BE(1,20);
    try {
      const receipt=scenario.startsWith('multi-')||scenario==='forward-body-error'
        ?await client.sendMultiPhoto('100',[{data:png},{data:png}])
        :await client.sendAttachment('100',Buffer.from('fixture bytes'),'fixture.pdf');
      console.log(JSON.stringify({receipt,uploads,ships,forwards}));
    }catch(error){console.log(JSON.stringify({rejected:true,uploads,ships,forwards}));}
  `;
  const child=Bun.spawn([process.execPath,'--eval',source],{stdout:'pipe',stderr:'pipe',cwd:import.meta.dir});
  const stdout=await new Response(child.stdout).text();const stderr=await new Response(child.stderr).text();
  expect(await child.exited).toBe(0);if(stderr)throw new Error(stderr);
  return JSON.parse(stdout);
}

describe('real SDK media COMPLETE validation',()=>{
  for(const scenario of ['header-error','body-error','wrong-chat','missing-complete','post-error']) {
    test(`${scenario} cannot become a successful media receipt or replay`,async()=>{
      const result=await probe(scenario);expect(result.rejected).toBe(true);expect(result.ships).toBe(1);expect(result.uploads).toBe(1);expect(result.receipt).toBeUndefined();
    });
  }
  for(const scenario of ['success','without-chat-id']) {
    test(`${scenario} retains the valid server receipt`,async()=>{
      const result=await probe(scenario);expect(result.receipt).toEqual({success:true,status_code:0,chat_id:'100',log_id:'999',sent_at:1000});expect(result.uploads).toBe(1);
    });
  }
  test('multi-photo COMPLETE rejection blocks final FORWARD',async()=>{
    const result=await probe('multi-body-error');expect(result.rejected).toBe(true);expect(result.uploads).toBe(2);expect(result.forwards).toBe(0);expect(result.ships).toBe(1);
  });
  test('multi-photo FORWARD body rejection cannot be reported as success',async()=>{
    const result=await probe('forward-body-error');expect(result.rejected).toBe(true);expect(result.forwards).toBe(1);
  });
  test('multi-photo valid COMPLETEs permit one final FORWARD',async()=>{
    const result=await probe('multi-success');expect(result.receipt.success).toBe(true);expect(result.forwards).toBe(1);expect(result.uploads).toBe(2);
  });
});
