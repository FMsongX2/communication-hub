import { createConnection, createServer, type Server } from 'node:net';
import { chmodSync, lstatSync, mkdirSync, rmSync } from 'node:fs';
import { dirname } from 'node:path';
import { MAX_FRAME } from './types';

export const hubRequest=(path:string,event:unknown)=>hubCall(path,{method:'ingest_kakao_loco',event});
export function hubCall(path:string,request:unknown):Promise<any> {
  return new Promise((resolve,reject)=>{
    const socket=createConnection(path);let buffer=Buffer.alloc(0);let done=false;
    const finish=(error?:Error,result?:unknown)=>{if(done)return;done=true;socket.destroy();error?reject(error):resolve(result);};
    socket.setTimeout(10000,()=>finish(new Error('hub_timeout')));
    socket.on('error',()=>finish(new Error('hub_unavailable')));
    socket.on('end',()=>finish(new Error('hub_closed_without_ack')));
    socket.on('connect',()=>socket.write(JSON.stringify(request)+'\n'));
    socket.on('data',chunk=>{
      buffer=Buffer.concat([buffer,typeof chunk==='string'?Buffer.from(chunk):chunk]);if(buffer.length>MAX_FRAME)return finish(new Error('hub_frame_limit'));
      const end=buffer.indexOf(10);if(end<0)return;
      try {finish(undefined,JSON.parse(buffer.subarray(0,end).toString('utf8')));}catch{finish(new Error('hub_invalid_json'));}
    });
  });
}
export async function serve(path:string,handler:(method:string,params:unknown)=>Promise<unknown>):Promise<Server> {
  const dir=dirname(path);mkdirSync(dir,{recursive:true,mode:0o700});
  const stat=lstatSync(dir);if(stat.isSymbolicLink()||stat.uid!==process.getuid?.()||(stat.mode&0o077)!==0)throw new Error('socket_directory_not_private');
  // Never unlink an unknown/live listener. The operator must remove a stale socket.
  const server=createServer(socket=>{
    let buffer=Buffer.alloc(0),active=false;
    socket.setTimeout(15000,()=>socket.destroy());
    socket.on('error',()=>{});
    socket.on('data',chunk=>{
      if(active){socket.destroy();return;}
      buffer=Buffer.concat([buffer,typeof chunk==='string'?Buffer.from(chunk):chunk]);if(buffer.length>MAX_FRAME){socket.destroy();return;}
      const end=buffer.indexOf(10);if(end<0)return;
      if(buffer.subarray(end+1).length){socket.destroy();return;}
      active=true;socket.setTimeout(180000,()=>socket.destroy());
      let request:any;
      try {request=JSON.parse(buffer.subarray(0,end).toString('utf8'));
        if(typeof request?.id!=='string'||request.id.length>128||typeof request.method!=='string')throw new Error();
      }catch{socket.end(JSON.stringify({id:null,ok:false,error:{code:'invalid_request'}})+'\n');return;}
      void handler(request.method,request.params??{}).then(result=>socket.end(JSON.stringify({id:request.id,ok:true,result})+'\n'),()=>socket.end(JSON.stringify({id:request.id,ok:false,error:{code:'request_failed'}})+'\n'));
    });
  });
  server.maxConnections=32;
  await new Promise<void>((resolve,reject)=>{server.once('error',reject);server.listen(path,()=>{server.off('error',reject);chmodSync(path,0o600);resolve();});});
  server.on('close',()=>{try{rmSync(path);}catch{}});
  return server;
}
