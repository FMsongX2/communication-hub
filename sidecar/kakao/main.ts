import { readFileSync } from 'node:fs';
import { startSidecar } from './runtime';
async function main() {
  if(process.argv.length!==4||process.argv[2]!=='--config')throw new Error('usage: bun main.ts --config /absolute/config.json');
  const app=await startSidecar(JSON.parse(readFileSync(process.argv[3],'utf8')));
  console.log(JSON.stringify({status:'listening',mode:app.status().mode}));
}
if(import.meta.main)void main().catch(()=>{console.error(JSON.stringify({error:'sidecar_start_failed',action:'check_config_keychain_and_account_lock'}));process.exitCode=1;});
