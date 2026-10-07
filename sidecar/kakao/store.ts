import { Database } from 'bun:sqlite';
import { chmodSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import type { Receipt } from './types';

/** The transport journal contains only approved-scope metadata, never credentials. */
export class Store {
  readonly db: Database;
  constructor(stateDir: string, userId: string) {
    mkdirSync(stateDir, {recursive:true,mode:0o700}); chmodSync(stateDir,0o700);
    const path=join(stateDir,'transport.sqlite');
    this.db=new Database(path,{create:true,strict:true}); chmodSync(path,0o600);
    this.db.exec(`PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;
      CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS deliveries (id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, result TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS cursors (chat_id TEXT PRIMARY KEY, log_id TEXT NOT NULL, bootstrap_at TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS approval_epochs (chat_id TEXT PRIMARY KEY, epoch TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS outgoing (chat_id TEXT NOT NULL, log_id TEXT NOT NULL, PRIMARY KEY(chat_id,log_id));`);
    const row=this.db.query('SELECT value FROM metadata WHERE key=?').get('user_id') as {value:string}|null;
    if(row && row.value!==userId) {this.db.close();throw new Error('state_account_mismatch');}
    this.db.query('INSERT OR IGNORE INTO metadata VALUES (?,?)').run('user_id',userId);
  }
  delivery(id:string):{fingerprint:string;result:Receipt}|null {
    const row=this.db.query('SELECT fingerprint,result FROM deliveries WHERE id=?').get(id) as {fingerprint:string;result:string}|null;
    return row ? {fingerprint:row.fingerprint,result:JSON.parse(row.result)}:null;
  }
  begin(id:string,fingerprint:string,result:Receipt) {
    this.db.query('INSERT INTO deliveries VALUES (?,?,?)').run(id,fingerprint,JSON.stringify(result));
  }
  receipt(id:string,result:Receipt) {
    this.db.transaction(()=>{
      this.db.query('UPDATE deliveries SET result=? WHERE id=?').run(JSON.stringify(result),id);
      for(const log of [result.text_log_id,result.attachment_log_id]) if(log)
        this.db.query('INSERT OR IGNORE INTO outgoing VALUES (?,?)').run(result.chat_id,log);
    })();
  }
  isOutgoing(chat:string,log:string) {return !!this.db.query('SELECT 1 FROM outgoing WHERE chat_id=? AND log_id=?').get(chat,log);}
  cursor(chat:string):string|null {return (this.db.query('SELECT log_id FROM cursors WHERE chat_id=?').get(chat) as {log_id:string}|null)?.log_id??null;}
  bootstrap(chat:string,log:string) {this.db.query('INSERT OR IGNORE INTO cursors VALUES (?,?,?)').run(chat,log,new Date().toISOString());}
  advance(chat:string,log:string) {
    const previous=this.cursor(chat);
    if(previous===null)throw new Error('cursor_not_bootstrapped');
    if(BigInt(log)>BigInt(previous))this.db.query('UPDATE cursors SET log_id=? WHERE chat_id=?').run(log,chat);
  }
  /** Persist only valid Hub policy. Temporary Hub failure must not erase receive history. */
  applyApprovalEpochs(epochs:ReadonlyMap<string,string>,initialStaticIds:ReadonlySet<string>=new Set()) {
    this.db.transaction(()=>{
      const initialized=!!this.db.query("SELECT 1 FROM metadata WHERE key='dynamic_policy_initialized'").get();
      const rows=this.db.query('SELECT chat_id,epoch FROM approval_epochs').all() as Array<{chat_id:string;epoch:string}>;
      for(const row of rows)if(epochs.get(row.chat_id)!==row.epoch) {
        this.db.query('DELETE FROM cursors WHERE chat_id=?').run(row.chat_id);
        this.db.query('DELETE FROM approval_epochs WHERE chat_id=?').run(row.chat_id);
      }
      for(const [chat,epoch] of epochs) {
        const old=rows.find(row=>row.chat_id===chat);
        if(old?.epoch!==epoch) {
          // One-time static upgrade may adopt only an unchanged initial Hub grant.
          const adopt=!initialized&&!old&&epoch==='1'&&initialStaticIds.has(chat);
          if(!adopt)this.db.query('DELETE FROM cursors WHERE chat_id=?').run(chat);
          this.db.query('INSERT OR REPLACE INTO approval_epochs VALUES (?,?)').run(chat,epoch);
        }
      }
      this.db.query("INSERT OR REPLACE INTO metadata VALUES ('dynamic_policy_initialized','1')").run();
      // Migrating from static config must not retain revoked static cursors.
      for(const row of this.db.query('SELECT chat_id FROM cursors').all() as Array<{chat_id:string}>) {
        if(!epochs.has(row.chat_id))this.db.query('DELETE FROM cursors WHERE chat_id=?').run(row.chat_id);
      }
    })();
  }
  close() {this.db.close();}
}
