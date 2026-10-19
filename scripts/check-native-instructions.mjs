// Historical ACP SDK probe retained for evidence; incompatible with App core.
throw new Error("RETIRED: this test targets the ACP 1.4.0 WebSocket/local-provider SDK. It cannot run against App 2.24.0 language_server. Use scripts/check-app-core.mjs --live for bounded App core lifecycle coverage.");
// Real native runtime and localhost inference fixture; no paid model requests.
import http from 'node:http';
import path from 'node:path';
import {homedir} from 'node:os';
import {mkdir,mkdtemp,writeFile} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {RpcClient} from '../test/spec/rpc-client.mjs';
const root=path.resolve(import.meta.dirname,'..');
await mkdir(path.join(root,'.tmp'),{recursive:true});
const scratch=await mkdtemp(path.join(root,'.tmp/native-instructions-'));
const home=path.join(scratch,'home'),repo=path.join(scratch,'repo'),cwd=path.join(repo,'sub');
await mkdir(home);await mkdir(cwd,{recursive:true});
for(const [file,text] of [
 [path.join(home,'AGENTS.md'),'GLOBAL_SHADOWED_7A9'],
 [path.join(home,'AGENTS.override.md'),'GLOBAL_RULE_7A9'],
 [path.join(scratch,'AGENTS.md'),'OUTSIDE_RULE_7A9'],
 [path.join(repo,'.git'),'gitdir: fixture'],
 [path.join(repo,'AGENTS.md'),'ROOT_RULE_7A9'],
 [path.join(cwd,'AGENTS.md'),'LEAF_SHADOWED_7A9'],
 [path.join(cwd,'AGENTS.override.md'),'LEAF_RULE_7A9'],
])await writeFile(file,text);
const requests=[];
const fixture=http.createServer(async(req,res)=>{
 let body='';for await(const chunk of req)body+=chunk;
 requests.push(JSON.parse(body));
 res.setHeader('content-type','text/event-stream');
 res.end('data: '+JSON.stringify({candidates:[{content:{role:'model',parts:[{text:'FIXTURE_OK'}]},finishReason:'STOP'}],usageMetadata:{promptTokenCount:1,candidatesTokenCount:1,totalTokenCount:2}})+'\n\n');
});
await new Promise(r=>fixture.listen(0,'127.0.0.1',r));
const env={...process.env,CODEX_HOME:home,AGY_APP_SERVER_HOME:path.join(scratch,'state'),AGY_HARNESS_HOME:path.join(scratch,'state'),AGY_EXTERNAL_BINARY:process.env.AGY_EXTERNAL_BINARY||path.join(homedir(),'.local/opt/antigravity-acp/1.4.0/localharness_external'),GEMINI_API_KEY:'local-fixture-only',AGY_API_BASE_URL:`http://127.0.0.1:${fixture.address().port}`};
delete env.AGY_PERSONAL_CREDENTIALS;delete env.GOOGLE_API_KEY;delete env.GOOGLE_APPLICATION_CREDENTIALS;
const client=new RpcClient(process.env.AGY_APP_SERVER_BINARY||path.join(root,'target/debug/antigravity-app-server'),[],{cwd:root,env});
const ok=r=>{assert.ok(!r.error,JSON.stringify(r.error));return r.result;};
async function turn(threadId){
 const id=ok(await client.call('turn/start',{threadId,input:[{type:'text',text:'Reply OK.'}]})).turn.id;
 const deadline=Date.now()+20000;
 while(Date.now()<deadline){
  const done=client.events.find(e=>e.method==='turn/completed'&&e.params.turn.id===id);
  if(done){assert.equal(done.params.turn.status,'completed',JSON.stringify(done));return requests.at(-1);}
  await new Promise(r=>setTimeout(r,20));
 }
 throw Error('turn timeout: '+client.stderr);
}
const prompt=request=>JSON.stringify(request.systemInstruction??request.system_instruction);
try{
 ok(await client.call('initialize',{}));
 const thread=ok(await client.call('thread/start',{cwd,config:{mcp_servers:{},tools_disabled:true}})).thread.id;
 const first=prompt(await turn(thread));
 for(const marker of ['GLOBAL_RULE_7A9','ROOT_RULE_7A9','LEAF_RULE_7A9'])assert.equal(first.split(marker).length-1,1,marker);
 assert.ok(first.indexOf('GLOBAL_RULE_7A9')<first.indexOf('ROOT_RULE_7A9'));
 assert.ok(first.indexOf('ROOT_RULE_7A9')<first.indexOf('LEAF_RULE_7A9'));
 for(const marker of ['OUTSIDE_RULE_7A9','GLOBAL_SHADOWED_7A9','LEAF_SHADOWED_7A9'])assert.ok(!first.includes(marker),marker);
 assert.ok(first.length>3000,'native base instructions retained');
 await writeFile(path.join(cwd,'AGENTS.override.md'),'REFRESHED_RULE_7A9');
 ok(await client.call('thread/unload',{threadId:thread}));
 ok(await client.call('thread/resume',{threadId:thread}));
 const refreshed=prompt(await turn(thread));
 assert.equal(refreshed.split('REFRESHED_RULE_7A9').length-1,1);
 assert.ok(!refreshed.includes('LEAF_RULE_7A9'));
 // A per-thread budget override persists through unload/resume, and keeps globals.
 const limited=ok(await client.call('thread/start',{cwd,config:{mcp_servers:{},tools_disabled:true,project_doc_max_bytes:0}})).thread.id;
 ok(await client.call('thread/unload',{threadId:limited}));
 ok(await client.call('thread/resume',{threadId:limited}));
 const zero=prompt(await turn(limited));
 assert.ok(zero.includes('GLOBAL_RULE_7A9'));
 assert.ok(!zero.includes('ROOT_RULE_7A9')&&!zero.includes('REFRESHED_RULE_7A9'));
 console.log(JSON.stringify({pass:true,paidCalls:0,requests:requests.length,scratch}));
}finally{
 await client.close();fixture.closeAllConnections();fixture.close();
 await writeFile(path.join(scratch,'evidence.json'),JSON.stringify({requests,stderr:client.stderr},null,2));
}
