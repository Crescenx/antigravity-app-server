// Node.js 24+. Explicit invocation performs model calls using configured credentials.
import {spawn} from 'node:child_process';
import {createInterface} from 'node:readline';
import path from 'node:path';
const cwd=process.argv[2],prompt=process.argv.slice(3).join(' ');
if(!cwd||!path.isAbsolute(cwd)||!prompt)throw Error('Usage: node examples/client.mjs /absolute/workspace "prompt"');
const binary=process.env.AGY_APP_SERVER_BINARY||path.resolve(import.meta.dirname,'../target/release/antigravity-app-server');
const child=spawn(binary,[],{stdio:['pipe','pipe','inherit']});
child.stdin.on('error',()=>{});
let serial=0,settled=false;
const pending=new Map();
const send=value=>child.stdin.write(JSON.stringify(value)+'\n');
const done=new Promise((resolve,reject)=>{
 child.on('error',reject);
 child.on('exit',code=>{if(!settled)reject(Error(`Server exited before completion (${code})`));});
 createInterface({input:child.stdout}).on('line',line=>{
  try{
   const m=JSON.parse(line);
   if(m.method&&m.id!==undefined){send({id:m.id,error:{code:-32000,message:'Example client does not grant approvals or answer questions'}});return;}
   if(m.id!==undefined){const p=pending.get(m.id);pending.delete(m.id);if(p)m.error?p.reject(Error(JSON.stringify(m.error))):p.resolve(m.result);return;}
   if(m.method==='item/completed'&&m.params.item.type==='agentMessage')console.log(m.params.item.text);
   if(m.method==='turn/completed'){settled=true;m.params.turn.status==='completed'?resolve():reject(Error(JSON.stringify(m.params.turn)));}
  }catch(error){reject(error);}
 });
});
const call=(method,params={})=>new Promise((resolve,reject)=>{const id=++serial;pending.set(id,{resolve,reject});send({id,method,params});});
const timeout=setTimeout(()=>{for(const p of pending.values())p.reject(Error('Timed out; no retry'));child.kill();},120000);
// Attach rejection handling before startup RPCs, including early child failures.
const run=async()=>{
 await call('initialize');
 const {thread}=await call('thread/start',{cwd,sandbox:'read-only',approvalPolicy:'never',approvalsReviewer:'user',budget:{maxModelCalls:4,maxTotalTokens:20000,scope:'BUDGET_SCOPE_LIFETIME'}});
 await call('turn/start',{threadId:thread.id,clientUserMessageId:crypto.randomUUID(),input:[{type:'text',text:prompt}]});
 await done;
};
try{await Promise.race([run(),done]);}finally{clearTimeout(timeout);child.stdin.end();const kill=setTimeout(()=>child.kill('SIGKILL'),3000);kill.unref();}
