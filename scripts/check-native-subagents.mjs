// Historical ACP SDK probe retained for evidence; incompatible with App core.
throw new Error("RETIRED: this test targets the ACP 1.4.0 WebSocket/local-provider SDK. It cannot run against App 2.24.0 language_server. Use scripts/check-app-core.mjs --live for bounded App core lifecycle coverage.");
// Native runtime + host sandbox + deterministic model fixture; no paid inference.
import http from 'node:http';
import path from 'node:path';
import {homedir} from 'node:os';
import {mkdir,mkdtemp,writeFile,readFile,access} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {RpcClient} from '../test/spec/rpc-client.mjs';
const root=path.resolve(import.meta.dirname,'..');
await mkdir(path.join(root,'.tmp'),{recursive:true});
const scratch=await mkdtemp(path.join(root,'.tmp/managed-native-agents-'));
const cwd=path.join(scratch,'workspace'),home=path.join(scratch,'home'),skills=path.join(scratch,'skills');
for(const dir of [cwd,home,skills])await mkdir(dir);
await writeFile(path.join(cwd,'.git'),'gitdir: fixture');
await writeFile(path.join(cwd,'AGENTS.md'),'PROJECT_RULE_FOR_WORKERS');
await writeFile(path.join(home,'AGENTS.md'),'GLOBAL_RULE_FOR_WORKERS');
const requests=[],checks=[];let active,client,workerId;
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
async function until(fn,label,ms=30000){const end=Date.now()+ms;while(Date.now()<end){const result=fn();if(result)return result;await sleep(20);}throw Error('Timeout '+label+'; '+client?.stderr);}
const call=(name,args)=>[{functionCall:{name,args}}],text=value=>[{text:value}];
const fixture=http.createServer(async(req,res)=>{
 try{
  let raw='';for await(const b of req)raw+=b;
  const body=JSON.parse(raw),worker=JSON.stringify(body.systemInstruction).includes('Your agent name is /root/worker.');
  requests.push({scenario:active?.name,worker,body});
  let parts;
  if(worker){
   const n=++active.childCalls;
   if(active.name==='spawn'&&n===1)parts=call('run_command',{CommandLine:`printf 'inside' > '${cwd}/inside'; printf forbidden > '${scratch}/outside'`,Cwd:cwd,WaitMsBeforeAsync:10000,toolSummary:'Sandbox check',toolAction:'Testing workspace boundary'});
   else if(['interrupt','root_interrupt'].includes(active.name)&&n===1)parts=call('run_command',{CommandLine:`sleep 4; printf forbidden > '${cwd}/late-${active.name}'`,Cwd:cwd,WaitMsBeforeAsync:10000,toolSummary:'Cancellation check',toolAction:'Waiting for cancellation'});
   else parts=text('WORKER_RESULT_'+active.name);
  }else{
   const n=++active.rootCalls;
   if(n===1){
    if(active.name==='spawn')parts=call('spawn_agent',{task_name:'worker',message:'WORKER_INITIAL'});
    else if(active.name==='message')parts=call('send_message',{target:'worker',message:'QUEUED_MESSAGE_MARKER'});
    else parts=call('followup_task',{target:'worker',message:'TASK_'+active.name});
   }else if(active.name==='interrupt'&&n===2){
    await until(()=>client.events.slice(active.eventStart).some(e=>e.method==='item/started'&&e.params.threadId===workerId&&e.params.item.type==='commandExecution'),'worker command starts');
    parts=call('interrupt_agent',{target:'worker'});
   }else if((n===2&&active.name!=='message')||(n===3&&active.name==='interrupt'))parts=call('wait_agent',{timeout_ms:10000});
   else parts=text('ROOT_DONE_'+active.name);
  }
  res.setHeader('content-type','text/event-stream');
  res.end('data: '+JSON.stringify({candidates:[{content:{role:'model',parts},finishReason:'STOP'}],usageMetadata:{promptTokenCount:1,candidatesTokenCount:1,totalTokenCount:2}})+'\n\n');
 }catch(error){res.writeHead(500);res.end(String(error));}
});
await new Promise(r=>fixture.listen(0,'127.0.0.1',r));
const env={...process.env,CODEX_HOME:home,AGY_SKILLS_ROOT:skills,AGY_APP_SERVER_HOME:path.join(scratch,'state'),AGY_HARNESS_HOME:path.join(scratch,'state'),AGY_EXTERNAL_BINARY:process.env.AGY_EXTERNAL_BINARY||path.join(homedir(),'.local/opt/antigravity-acp/1.4.0/localharness_external'),GEMINI_API_KEY:'local-fixture-only',AGY_API_BASE_URL:`http://127.0.0.1:${fixture.address().port}`};
delete env.AGY_PERSONAL_CREDENTIALS;delete env.GOOGLE_API_KEY;delete env.GOOGLE_APPLICATION_CREDENTIALS;
client=new RpcClient(process.env.AGY_APP_SERVER_BINARY||path.join(root,'target/debug/antigravity-app-server'),[],{cwd:root,env});
const ok=r=>{assert.ok(!r.error,JSON.stringify(r.error));return r.result;};
const exists=async file=>{try{await access(file);return true;}catch{return false;}};
async function run(name,threadId){
 active={name,rootCalls:0,childCalls:0,eventStart:client.events.length};
 const turn=ok(await client.call('turn/start',{threadId,input:[{type:'text',text:name==='spawn'?'spawn PARENT_CONTEXT_MARKER':name}]})).turn.id;
 const event=await until(()=>client.events.find(e=>e.method==='turn/completed'&&e.params.turn.id===turn),'turn '+name);
 assert.equal(event.params.turn.status,'completed');
 return event.params.turn;
}
async function check(label,fn){await fn();checks.push(label);console.log('PASS '+label);}
let failure;
try{
 ok(await client.call('initialize',{}));
 const thread=ok(await client.call('thread/start',{cwd,approvalPolicy:'never',approvalsReviewer:'user',sandbox:'workspace-write',developerInstructions:'APPLICATION_RULE_FOR_WORKERS',config:{mcp_servers:{}}})).thread.id;
 const first=await run('spawn',thread);
 const children=ok(await client.call('thread/list',{ancestorThreadId:thread})).data;
 assert.equal(children.length,1);workerId=children[0].id;
 await check('native child has a standard thread identity and inherits the workspace sandbox',async()=>{
  assert.equal(await readFile(path.join(cwd,'inside'),'utf8'),'inside');assert.equal(await exists(path.join(scratch,'outside')),false);
  assert.equal(children[0].source.subAgent.thread_spawn.parent_thread_id,thread);
  assert.equal(children[0].source.subAgent.thread_spawn.agent_path,'/root/worker');
  assert.equal(active.childCalls,2);
  assert.ok(JSON.stringify(requests.find(r=>r.worker).body.contents).includes('PARENT_CONTEXT_MARKER'));
  assert.ok(first.items.some(i=>i.type==='collabAgentToolCall'&&i.tool==='spawnAgent'&&i.status==='completed'));
 });
 await check('model sees Codex-style tools and child retains inherited rules',()=>{
  for(const {body,worker}of requests){
   const names=body.tools.flatMap(t=>t.functionDeclarations??[]).map(t=>t.name);
   assert.ok(names.includes('followup_task'));assert.ok(names.includes('interrupt_agent'));assert.ok(!names.includes('invoke_subagent'));
   if(worker){assert.ok(!names.includes('spawn_agent'));for(const marker of ['GLOBAL_RULE_FOR_WORKERS','PROJECT_RULE_FOR_WORKERS','APPLICATION_RULE_FOR_WORKERS'])assert.ok(JSON.stringify(body.systemInstruction).includes(marker));}
  }
 });
 await run('message',thread);await sleep(500);
 await check('send_message queues without waking an idle child',()=>assert.equal(active.childCalls,0));
 await run('followup',thread);
 await check('followup wakes same child and supplies queued message plus previous context',()=>{
  assert.equal(active.childCalls,1);
  const content=JSON.stringify(requests.find(r=>r.scenario==='followup'&&r.worker).body.contents);
  for(const marker of ['QUEUED_MESSAGE_MARKER','WORKER_INITIAL','TASK_followup'])assert.ok(content.includes(marker),marker);
 });
 await run('interrupt',thread);await sleep(4200);
 await check('targeted interruption stops pending work while preserving the child thread',async()=>{
  assert.equal(await exists(path.join(cwd,'late-interrupt')),false);
  const child=ok(await client.call('thread/read',{threadId:workerId})).thread;
  assert.equal(child.turns.at(-1).status,'interrupted');
 });
 ok(await client.call('thread/unload',{threadId:workerId}));
 await run('resume',thread);
 await check('followup cold-resumes interrupted native session with same identity and history',()=>{
  assert.equal(active.childCalls,1);
  assert.ok(JSON.stringify(requests.find(r=>r.scenario==='resume'&&r.worker).body.contents).includes('WORKER_INITIAL'));
 });
 active={name:'root_interrupt',rootCalls:0,childCalls:0,eventStart:client.events.length};
 const rootTurn=ok(await client.call('turn/start',{threadId:thread,input:[{type:'text',text:'interrupt whole tree'}]})).turn.id;
 await until(()=>client.events.slice(active.eventStart).some(e=>e.method==='item/started'&&e.params.threadId===workerId&&e.params.item.type==='commandExecution'),'child running before root interrupt');
 ok(await client.call('turn/interrupt',{threadId:thread,turnId:rootTurn}));
 await until(()=>client.events.slice(active.eventStart).some(e=>e.method==='turn/completed'&&e.params.threadId===workerId),'child stopped by root interrupt');
 await until(()=>client.events.find(e=>e.method==='turn/completed'&&e.params.turn.id===rootTurn),'root interrupted');await sleep(4200);
 await check('root interrupt also stops child execution and keeps both identities recoverable',async()=>{
  assert.equal(await exists(path.join(cwd,'late-root_interrupt')),false);
  assert.equal(ok(await client.call('thread/read',{threadId:workerId})).thread.turns.at(-1).status,'interrupted');
 });
 // No inference is triggered by loading the parent or child.
 ok(await client.call('thread/unload',{threadId:workerId}));ok(await client.call('thread/unload',{threadId:thread}));
 const before=requests.length;ok(await client.call('thread/resume',{threadId:thread}));await sleep(100);assert.equal(requests.length,before);
 await run('cold_parent',thread);
 await check('parent unload/resume preserves agent registry and can restart child without replay',()=>assert.equal(active.childCalls,1));
}catch(error){failure=error.stack;console.error(failure);}finally{
 await client.close();fixture.closeAllConnections();fixture.close();
 await writeFile(path.join(scratch,'evidence.json'),JSON.stringify({requests,events:client.events,stderr:client.stderr},null,2));
 await writeFile(path.join(scratch,'report.json'),JSON.stringify({checks,failure,paidCalls:0},null,2));
 console.log(JSON.stringify({scratch,pass:checks.length,failure}));
}
process.exitCode=failure?1:0;
