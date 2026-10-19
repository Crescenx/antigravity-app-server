// Historical ACP SDK probe retained for evidence; incompatible with App core.
throw new Error("RETIRED: this test targets the ACP 1.4.0 WebSocket/local-provider SDK. It cannot run against App 2.24.0 language_server. Use scripts/check-app-core.mjs --live for bounded App core lifecycle coverage.");
// Real Rust stdio + pinned native runtime, with a held localhost model. No paid calls.
import http from 'node:http';
import path from 'node:path';
import {homedir} from 'node:os';
import {mkdir,mkdtemp,writeFile,readFile,access} from 'node:fs/promises';
import assert from 'node:assert/strict';
import {RpcClient} from '../test/spec/rpc-client.mjs';
const root=path.resolve(import.meta.dirname,'..');
await mkdir(path.join(root,'.tmp'),{recursive:true});const scratch=await mkdtemp(path.join(root,'.tmp/native-steer-contract-'));
const cwd=path.join(scratch,'workspace');await mkdir(cwd);
const requests=[],checks=[];let mode='normal',count=0,held;
const respond=(res,parts)=>{if(!res.destroyed)res.end('data: '+JSON.stringify({candidates:[{content:{role:'model',parts},finishReason:'STOP'}],usageMetadata:{promptTokenCount:1,candidatesTokenCount:1,totalTokenCount:2}})+'\n\n');};
const fixture=http.createServer(async(req,res)=>{let body='';for await(const chunk of req)body+=chunk;requests.push({mode,body:JSON.parse(body)});count++;res.setHeader('Content-Type','text/event-stream');
 if(count===1){held=res;return;}
 if(mode==='interrupt'&&count===2)respond(res,[{functionCall:{name:'run_command',args:{CommandLine:`printf forbidden > '${cwd}/after-interrupt'`,Cwd:cwd}}}]);
 else respond(res,[{text:'LOCAL_STEER_FINAL'}]);
});
await new Promise((resolve,reject)=>{fixture.once('error',reject);fixture.listen(0,'127.0.0.1',resolve);});
const env={...process.env,AGY_HARNESS_HOME:path.join(scratch,'state'),AGY_EXTERNAL_BINARY:process.env.AGY_EXTERNAL_BINARY||path.join(homedir(),'.local/opt/antigravity-acp/1.4.0/localharness_external'),GEMINI_API_KEY:'local-fixture-only',AGY_API_BASE_URL:`http://127.0.0.1:${fixture.address().port}`};
env.AGY_APP_SERVER_HOME=env.AGY_HARNESS_HOME;
delete env.AGY_PERSONAL_CREDENTIALS;delete env.GOOGLE_API_KEY;delete env.GOOGLE_APPLICATION_CREDENTIALS;
const client=new RpcClient(process.env.HARNESS_BINARY||process.env.AGY_APP_SERVER_BINARY || path.join(root,'target/debug/antigravity-app-server'),[],{cwd:root,env});
const ok=r=>{assert.ok(!r.error,JSON.stringify(r.error));return r.result;};
const sleep=ms=>new Promise(r=>setTimeout(r,ms));
async function until(fn,label){const deadline=Date.now()+15000;while(Date.now()<deadline){const value=fn();if(value)return value;await sleep(10);}throw Error(`timeout ${label}; stderr=${client.stderr}`);}
async function check(name,fn){await fn();checks.push(name);console.log('PASS '+name);}
let error;
try{
 ok(await client.call('initialize',{}));const thread=ok(await client.call('thread/start',{cwd,approvalPolicy:'on-request',sandbox:'workspace-write'})).thread.id;
 const first=ok(await client.call('turn/start',{threadId:thread,clientUserMessageId:'initial',input:[{type:'text',text:'INITIAL'}]})).turn.id;
 await until(()=>held,'held first request');
 const steering={threadId:thread,expectedTurnId:first,clientUserMessageId:'steer-one',input:[{type:'text',text:'STEER_ONE'}],additionalContext:{fixture:{kind:'application',value:'STEER_CONTEXT'}}};
 await check('reject stale turn before native dispatch',async()=>assert.ok((await client.call('turn/steer',{...steering,expectedTurnId:'stale'})).error));
 await check('queued receipt is durable and same request deduplicates',async()=>{assert.equal(ok(await client.call('turn/steer',steering)).deliveryStatus,'queued');assert.equal(ok(await client.call('turn/steer',steering)).deliveryStatus,'queued');const persisted=JSON.parse(await readFile(path.join(scratch,'state','threads',thread+'.json'),'utf8'));assert.equal(persisted.steerReceipts['steer-one'].state,'queued');assert.equal(count,1);});
 await check('changed context with same client ID is rejected',async()=>assert.ok((await client.call('turn/steer',{...steering,additionalContext:{fixture:{kind:'application',value:'changed'}}})).error));
 ok(await client.call('turn/steer',{threadId:thread,expectedTurnId:first,clientUserMessageId:'steer-two',input:[{type:'text',text:'STEER_TWO'}]}));
 respond(held,[{text:'ORIGINAL_RESPONSE'}]);
 await until(()=>client.events.some(e=>e.method==='turn/completed'&&e.params.turn.id===first),'first completion');
 await check('both queued inputs reach next model call once in FIFO order',()=>{const second=requests.filter(r=>r.mode==='normal')[1];const body=JSON.stringify(second.body);assert.ok(body.includes('STEER_CONTEXT'));assert.ok(body.indexOf('STEER_ONE')<body.indexOf('STEER_TWO'));assert.equal(requests.filter(r=>r.mode==='normal').length,2);});
 await check('standard user message client IDs are emitted once',()=>{const users=client.events.filter(e=>e.method==='item/completed'&&e.params.item.type==='userMessage').map(e=>e.params.item);assert.deepEqual(users.map(x=>x.clientId),['initial','steer-one','steer-two']);assert.deepEqual(users.map(x=>x.content[0].text),['INITIAL','STEER_ONE','STEER_TWO']);});
 await check('completed receipt retry does not start another turn',async()=>{assert.equal(ok(await client.call('turn/steer',steering)).deliveryStatus,'consumed');assert.equal(count,2);});
 mode='interrupt';count=0;held=undefined;
 const second=ok(await client.call('turn/start',{threadId:thread,clientUserMessageId:'interrupt-initial',input:[{type:'text',text:'INTERRUPT_INITIAL'}]})).turn.id;
 await until(()=>held,'held interrupted request');
 ok(await client.call('turn/steer',{threadId:thread,expectedTurnId:second,clientUserMessageId:'interrupt-steer',input:[{type:'text',text:'INTERRUPT_STEER'}]}));
 ok(await client.call('turn/interrupt',{threadId:thread,turnId:second}));
 const done=await until(()=>client.events.find(e=>e.method==='turn/completed'&&e.params.turn.id===second),'interrupted completion');
 await check('queued steering cannot execute a tool after interrupt',async()=>{assert.equal(done.params.turn.status,'interrupted');await assert.rejects(access(path.join(cwd,'after-interrupt')));const persisted=JSON.parse(await readFile(path.join(scratch,'state','threads',thread+'.json'),'utf8'));assert.notEqual(persisted.steerReceipts['interrupt-steer'].state,'queued');});
 mode='resumed';count=0;held=undefined;
 const third=ok(await client.call('turn/start',{threadId:thread,input:[{type:'text',text:'RESUMED'}]})).turn.id;
 await until(()=>held,'held resumed request');respond(held,[{text:'RESUMED_OK'}]);
 await until(()=>client.events.some(e=>e.method==='turn/completed'&&e.params.turn.id===third),'resumed completion');
 await check('normal turn starts after interrupt without replaying old input',()=>{assert.equal(requests.filter(r=>r.mode==='resumed').length,1);const newUser=client.events.filter(e=>e.method==='item/completed'&&e.params.turnId===third&&e.params.item.type==='userMessage');assert.equal(newUser.length,1);assert.equal(newUser[0].params.item.content[0].text,'RESUMED');});
}catch(e){error=e.stack;console.error(error);}finally{
 await client.close();fixture.closeAllConnections();fixture.close();
 await writeFile(path.join(scratch,'evidence.json'),JSON.stringify({requests,events:client.events,stderr:client.stderr},null,2));
 await writeFile(path.join(scratch,'report.json'),JSON.stringify({paidCalls:0,checks,error},null,2));console.log(JSON.stringify({scratch,pass:checks.length,error}));
}
process.exitCode=error?1:0;
