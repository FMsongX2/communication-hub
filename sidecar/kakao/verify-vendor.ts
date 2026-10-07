import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
const root=resolve(import.meta.dir,'../../third_party/agent-messenger');
const pin=JSON.parse(readFileSync(resolve(root,'UPSTREAM.json'),'utf8'));
if(pin.commit!=='f0a441dfb8f1865d54de26eaf3055de2df64a936'||pin.version!=='2.39.0')throw new Error('upstream_pin_mismatch');
for(const [file,expected] of Object.entries(pin.patched_sha256)) {
 const actual=createHash('sha256').update(readFileSync(resolve(root,file))).digest('hex');
 if(actual!==expected)throw new Error(`vendor_checksum_mismatch: ${file}`);
}
console.log(JSON.stringify({status:'verified',commit:pin.commit,files:Object.keys(pin.patched_sha256).length}));
