// Native runtime + local model metadata/generation fixture. No paid inference.
import assert from 'node:assert/strict';
import http from 'node:http';
import {mkdir,mkdtemp} from 'node:fs/promises';
import {homedir} from 'node:os';
import path from 'node:path';
import {RpcClient} from '../test/spec/rpc-client.mjs';
const root=path.resolve(import.meta.dirname,'..');
await mkdir(path.join(root,'.tmp'),{recursive:true});
const scratch=await mkdtemp(path.join(root,'.tmp/models-goal-'));
const requests=[];
const fixture=http.createServer(async(req,res)=>{
 let body='';for await(const chunk of req)body+=chunk;
 if(req.method==='GET'&&req.url==='/v1beta/models'){
  res.setHeader('content-type','application/json');
  return res.end(JSON.stringify({models:['claude-sonnet-5-5-low','claude-sonnet-5-5-high'].map(id=>({name:'models/'+id,supportedGenerationMethods:['generateContent']}))}));
 }
 requests.push({url:req.url,body:JSON.parse(body)});
 res.setHeader('content-type','text/event-stream');
 res.end('data: '+JSON.stringify({candidates:[{content:{role:'model',parts:[{text:'Fixture task completed.'}]},finishReason:'STOP'}],usageMetadata:{promptTokenCount:1,candidatesTokenCount:1,totalTokenCount:2}})+'\n\n');
});
await new Promise(resolve=>fixture.listen(0,'127.0.0.1',resolve));
const env={...process.env,AGY_APP_SERVER_HOME:path.join(scratch,'state'),AGY_EXTERNAL_BINARY:process.env.AGY_EXTERNAL_BINARY||path.join(homedir(),'.local/opt/antigravity-acp/1.4.0/localharness_external'),AGY_API_BASE_URL:`http://127.0.0.1:${fixture.address().port}`,GEMINI_API_KEY:'fixture-only'};
delete env.AGY_PERSONAL_CREDENTIALS;
const client=new RpcClient(process.env.AGY_APP_SERVER_BINARY||path.join(root,'target/debug/antigravity-app-server'),[],{env,cwd:root});
const ok=r=>{assert.ok(!r.error,JSON.stringify(r.error));return r.result;};
try{
 ok(await client.call('initialize'));
 const catalog=ok(await client.call('model/list'));
 assert.deepEqual(catalog.data[0].supportedReasoningEfforts.map(e=>e.reasoningEffort),['low','high']);
 const started=ok(await client.call('thread/start',{cwd:scratch,model:'claude-sonnet-5-5',reasoningEffort:'high',sandbox:'read-only',approvalPolicy:'never',approvalsReviewer:'user',budget:{maxModelCalls:1,scope:'BUDGET_SCOPE_LIFETIME'}}));
 assert.equal(started.model,'claude-sonnet-5-5');assert.equal(started.reasoningEffort,'high');
 const threadId=started.thread.id;
 ok(await client.call('thread/inject_items',{threadId,items:[{type:'message',role:'developer',content:[{type:'input_text',text:'previous-runtime-reference-458'}]}]}));
 assert.equal(requests.length,0);
 ok(await client.call('thread/goal/set',{threadId,objective:'finish the authorized fixture task',tokenBudget:1}));
 const deadline=Date.now()+15000;
 while(!client.events.some(e=>e.method==='turn/completed')&&Date.now()<deadline)await new Promise(r=>setTimeout(r,25));
 assert.ok(client.events.some(e=>e.method==='turn/completed'),'native Goal did not complete');
 assert.equal(requests.length,1);
 assert.match(requests[0].url,/claude-sonnet-5-5-high/);
 assert.match(JSON.stringify(requests[0].body),/previous-runtime-reference-458/);
 assert.match(JSON.stringify(requests[0].body),/finish the authorized fixture task/);
 const goal=ok(await client.call('thread/goal/get',{threadId})).goal;
 assert.equal(goal.tokensUsed,2);assert.equal(goal.status,'budgetLimited');
 console.log(JSON.stringify({passed:true,modelCallsToRealProvider:0,checks:['catalog groups native variants','reasoning effort round trip','native request uses exact high variant','history injection makes no model call','first Goal turn receives history and counts budget'],scratch},null,2));
}finally{await client.close();await new Promise(resolve=>fixture.close(resolve));}
