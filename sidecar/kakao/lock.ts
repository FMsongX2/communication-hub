import { mkdirSync, lstatSync, openSync, writeSync, fsyncSync, closeSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { homedir } from 'node:os';
import { positiveId } from './types';
/** Fixed per-OS-user/account lock, independent of configurable state and socket paths. */
export function accountLock(userId:string):()=>void {
  if(!positiveId(userId))throw new Error('invalid_account');
  const root=join(homedir(),'.local/state/communication-hub/kakao-locks');
  mkdirSync(root,{recursive:true,mode:0o700});
  const stat=lstatSync(root);
  if(stat.isSymbolicLink()||stat.uid!==process.getuid?.()||(stat.mode&0o077)!==0)throw new Error('lock_directory_not_private');
  const path=join(root,`${userId}.lock`);
  // Fail closed on stale locks too; explicit recovery avoids unlink races and PID reuse.
  const fd=openSync(path,'wx',0o600);writeSync(fd,JSON.stringify({pid:process.pid,started_at:new Date().toISOString()}));fsyncSync(fd);closeSync(fd);
  return ()=>rmSync(path);
}
