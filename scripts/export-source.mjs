// Explicit public-source allowlist. Never copies .git or developer evidence.
import {mkdir, readdir, readFile, writeFile, lstat} from 'node:fs/promises';
import path from 'node:path';
const root=path.resolve(import.meta.dirname,'..');
const destination=path.join(root,'.tmp',`public-source-${Date.now()}`);
const entries=['Cargo.toml','Cargo.lock','LICENSE','NOTICE','README.md','SECURITY.md','CONTRIBUTING.md','.gitignore','src','examples','docs/guardian.md','docs/protocol.md','scripts/export-source.mjs','scripts/check-guardian.mjs','scripts/check-native-permissions.mjs','scripts/check-native-steer.mjs','scripts/check-models-goal.mjs','scripts/check-native-instructions.mjs','scripts/check-native-subagents.mjs','test/spec/rpc-client.mjs'];
const files=[];
async function collect(relative){
 const stat=await lstat(path.join(root,relative));
 if(stat.isSymbolicLink())throw Error(`Refusing symlink: ${relative}`);
 if(stat.isDirectory())for(const name of (await readdir(path.join(root,relative))).sort())await collect(path.join(relative,name));
 else if(stat.isFile())files.push(relative);
 else throw Error(`Unsupported file: ${relative}`);
}
for(const entry of entries)await collect(entry);
const content=new Map();
for(const file of files){
 const bytes=await readFile(path.join(root,file)),text=bytes.toString('utf8');
 if(/-----BEGIN (?:RSA |OPENSSH |EC )?PRIVATE KEY-----|\bAIza[0-9A-Za-z_-]{35}\b|\bsk-(?:proj-)?[A-Za-z0-9_-]{40,}|\/home\/andantex\/|\/mnt\/[a-z]\/Users\//.test(text))throw Error(`Review sensitive content before export: ${file}`);
 content.set(file,bytes);
}
await mkdir(path.dirname(destination),{recursive:true});
await mkdir(destination); // Never overwrite a previous export.
for(const [file,bytes] of content){const target=path.join(destination,file);await mkdir(path.dirname(target),{recursive:true});await writeFile(target,bytes,{flag:'wx'});}
console.log(JSON.stringify({directory:destination,files:files.length,gitHistoryIncluded:false,viewerIncluded:false},null,2));
