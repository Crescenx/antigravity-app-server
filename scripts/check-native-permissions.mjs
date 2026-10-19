// Real pinned native runtime + scripted localhost model. No provider credentials or paid calls.
import http from 'node:http';
import {mkdir,mkdtemp,writeFile,readFile,access} from 'node:fs/promises';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {homedir} from 'node:os';
import assert from 'node:assert/strict';
import {RpcClient} from '../test/spec/rpc-client.mjs';
const root=path.dirname(path.dirname(fileURLToPath(import.meta.url)));
await mkdir(path.join(root,'.tmp'),{recursive:true});
const scratch=await mkdtemp(path.join(root,'.tmp/native-permission-contract-'));
const cwd=path.join(scratch,'workspace');await mkdir(path.join(cwd,'.git'),{recursive:true});
const requests=[],checks=[],approvals=[];let active={name:'none',steps:[],index:0};
const fixture=http.createServer(async(req,res)=>{
 let body='';for await(const chunk of req)body+=chunk;
 try {
  const parsed=JSON.parse(body);requests.push({scenario:active.name,url:req.url,body:parsed});
  const next=active.steps[active.index++];
  const response={candidates:[{content:{role:'model',parts:next?[{functionCall:next}]:[{text:'Local permission fixture finished.'}]},finishReason:'STOP'}],usageMetadata:{promptTokenCount:1,cachedContentTokenCount:1,candidatesTokenCount:1,totalTokenCount:2}};
  res.setHeader('Content-Type','text/event-stream');res.end('data: '+JSON.stringify(response)+'\n\n');
 } catch(error){res.writeHead(500);res.end(error.message);}
});
await new Promise((resolve,reject)=>{fixture.once('error',reject);fixture.listen(0,'127.0.0.1',resolve)});
const netServer=http.createServer((req,res)=>res.end('fixture-network-ok'));
await new Promise((resolve,reject)=>{netServer.once('error',reject);netServer.listen(0,'127.0.0.1',resolve)});
const binary=process.env.HARNESS_BINARY||process.env.AGY_APP_SERVER_BINARY || path.join(root,'target/debug/antigravity-app-server');
const env={...process.env,AGY_HARNESS_HOME:path.join(scratch,'state'),AGY_EXTERNAL_BINARY:process.env.AGY_EXTERNAL_BINARY||path.join(homedir(),'.local/opt/antigravity-acp/1.4.0/localharness_external'),GEMINI_API_KEY:'local-fixture-no-real-credentials',AGY_API_BASE_URL:`http://127.0.0.1:${fixture.address().port}`};
env.AGY_APP_SERVER_HOME=env.AGY_HARNESS_HOME;
delete env.AGY_PERSONAL_CREDENTIALS;delete env.GOOGLE_API_KEY;delete env.GOOGLE_APPLICATION_CREDENTIALS;
const client=new RpcClient(binary,[],{cwd:root,env});
const seenApprovals=new Set();
const responder=setInterval(()=>{for(const e of client.events){if(e.id&&e.method?.endsWith('/requestApproval')&&!seenApprovals.has(e.id)){seenApprovals.add(e.id);const serialized=JSON.stringify(e.params);const decision=serialized.includes('DENY_FIXTURE')?'decline':serialized.includes('SESSION_FIXTURE')?'acceptForSession':'accept';approvals.push({request:e,decision});client.child.stdin.write(JSON.stringify({jsonrpc:'2.0',id:e.id,result:{decision}})+'\n');}}},10);
const ok=r=>{assert.ok(!r.error,JSON.stringify(r.error));return r.result;};
const shell=(command,bypass=false,workingDirectory=cwd)=>({name:'run_command',args:{CommandLine:command,Cwd:workingDirectory,BypassSandbox:bypass,WaitMsBeforeAsync:10000,toolSummary:'Permission fixture',toolAction:'Testing permissions'}});
const file=(target,content,bypass=false)=>({name:'write_to_file',args:{TargetFile:target,CodeContent:content,BypassSandbox:bypass}});
const nativeFile=(target,content)=>({name:'write_to_file',args:{TargetFile:target,CodeContent:content,Description:'Write temporary fixture',Overwrite:true,toolSummary:'Permission fixture',toolAction:'Writing fixture'}});
const nativeEdit=(target,before,after)=>({name:'replace_file_content',args:{TargetFile:target,TargetContent:before,ReplacementContent:after,StartLine:1,EndLine:1,AllowMultiple:false,Instruction:'Modify fixture',Description:'Modify fixture',toolSummary:'Permission fixture',toolAction:'Editing fixture'}});
const declarationsFor=name=>requests.find(r=>r.scenario===name).body.tools.flatMap(t=>t.functionDeclarations||[]);
const propertiesOf=tool=>(tool.parametersJsonSchema||tool.parameters).properties;
async function exists(p){try{await access(p);return true;}catch{return false;}}
async function check(name,fn){try{await fn();checks.push({name,status:'pass'});}catch(e){checks.push({name,status:'fail',error:e.message});}console.log(`${checks.at(-1).status.toUpperCase()} ${name}`);}
async function turn(threadId,name,steps,extra={}){
 active={name,steps,index:0};const before=client.events.length;
 const response=ok(await client.call('turn/start',{threadId,input:[{type:'text',text:name}],collaborationMode:{mode:'default',settings:{model:'gemini-2.5-flash',reasoning_effort:null,developer_instructions:null}},...extra}));
 const id=response.turn.id;const deadline=Date.now()+25000;
 while(Date.now()<deadline){const done=client.events.slice(before).find(e=>e.method==='turn/completed'&&e.params?.turn?.id===id);if(done)return {events:client.events.slice(before),turn:done.params.turn};await new Promise(r=>setTimeout(r,20));}
 throw Error(`turn ${name} did not complete; stderr=${client.stderr}`);
}
let failure;
try {
 ok(await client.call('initialize',{clientInfo:{name:'native-permission-contract',version:'1'},capabilities:{experimentalApi:true}}));
 const started=ok(await client.call('thread/start',{approvalsReviewer:'user',cwd,model:'gemini-2.5-flash',approvalPolicy:'on-request',sandboxPolicy:{type:'workspaceWrite',writableRoots:[cwd],networkAccess:false,excludeTmpdirEnvVar:true,excludeSlashTmp:true}}));
 const id=started.thread.id;
 const steps=[file(path.join(cwd,'file.txt'),'alpha'),{name:'replace_file_content',args:{TargetFile:path.join(cwd,'file.txt'),TargetContent:'alpha',ReplacementContent:'beta'}},shell(`cat '${cwd}/file.txt'`),shell(`printf blocked > '${scratch}/sibling'`),shell(`printf blocked > '${cwd}/.git/forbidden'`),shell(`curl --noproxy '*' --max-time 2 http://127.0.0.1:${netServer.address().port}`),shell(`printf allowed > '${scratch}/approved'`,true),shell(`printf DENY_FIXTURE > '${scratch}/denied'`,true)];
 const first=await turn(id,'permission-matrix',steps,{additionalContext:{probe:{kind:'application',value:'context-round-one'}}});
 await check('native command schema with external file tools and disabled task input',()=>{const declarations=declarationsFor('permission-matrix');for(const name of ['run_command','write_to_file','replace_file_content'])assert.equal(declarations.filter(t=>t.name===name).length,1,name);const command=declarations.find(t=>t.name==='run_command');assert.ok(propertiesOf(command).RunPersistent);assert.ok(propertiesOf(command).WaitMsBeforeAsync);assert.ok(propertiesOf(command).BypassSandbox);assert.ok(!declarations.some(t=>t.name==='manage_task'));assert.ok(propertiesOf(declarations.find(t=>t.name==='write_to_file')).BypassSandbox);});
 await check('workspace file write, replacement and command read',async()=>assert.equal(await readFile(path.join(cwd,'file.txt'),'utf8'),'beta'));
 await check('filesystem denies sibling and protected git',async()=>{assert.equal(await exists(path.join(scratch,'sibling')),false);assert.equal(await exists(path.join(cwd,'.git/forbidden')),false);});
 await check('explicit bypass accept and decline',async()=>{assert.equal(await readFile(path.join(scratch,'approved'),'utf8'),'allowed');assert.equal(await exists(path.join(scratch,'denied')),false);assert.equal(approvals.length,2);assert.deepEqual(approvals.map(x=>x.decision),['accept','decline']);});
 await check('standard command and file completed items',()=>{const items=first.events.filter(e=>e.method==='item/completed').map(e=>e.params.item);assert.ok(items.some(i=>i.type==='commandExecution'&&i.aggregatedOutput?.includes('beta')));assert.ok(items.some(i=>i.type==='fileChange'&&i.changes?.some(c=>c.diff?.includes('+alpha'))));assert.ok(items.some(i=>i.type==='commandExecution'&&i.command?.includes('curl')&&i.status==='failed'));});
 await check('Viewer commands and approval prompts contain only original command',()=>{for(const e of first.events){if(e.params?.item?.type==='commandExecution'){const item=e.params.item;assert.ok(steps.some(s=>s.name==='run_command'&&s.args.CommandLine===item.command),JSON.stringify(item));assert.doesNotMatch(item.command,/sandbox-state-json|permission-tool-helper|native-command-helper/);}}for(const a of approvals)assert.doesNotMatch(JSON.stringify(a.request),/sandbox-state-json|native-command-helper/);});
 await check('standard usage projects synthetic cache counters',()=>{const usage=first.events.filter(e=>e.method==='thread/tokenUsage/updated').at(-1);assert.ok(usage);assert.ok(JSON.stringify(usage).includes('cachedInputTokens'));assert.ok(usage.params.tokenUsage.total.cachedInputTokens>0);});
 const second=await turn(id,'context-second',[],{additionalContext:{probe:{kind:'application',value:'context-round-two'}}});
 await check('additional context reaches subsequent model request',()=>{const request=requests.find(r=>r.scenario==='context-second');assert.ok(JSON.stringify(request.body).includes('context-round-two'));assert.equal(second.turn.status,'completed');});
 const fileApprovalBefore=approvals.length;
 await turn(id,'outside-file-bypass',[file(path.join(scratch,'outside-file'),'allowed-file',true)]);
 await check('outside file bypass reaches wrapper approval',async()=>{assert.equal(await readFile(path.join(scratch,'outside-file'),'utf8'),'allowed-file');assert.equal(approvals.length,fileApprovalBefore+1);});
 const sessionBefore=approvals.length;const repeated=`printf x >> '${scratch}/session-grant' # SESSION_FIXTURE`;
 await turn(id,'session-grant',[shell(repeated,true),shell(repeated,true),shell(`printf y >> '${scratch}/session-grant' # SESSION_FIXTURE`,true)]);
 await check('session grant matches exact arguments and prompts on changes',async()=>{assert.equal(await readFile(path.join(scratch,'session-grant'),'utf8'),'xxy');assert.equal(approvals.length,sessionBefore+2);assert.ok(approvals.slice(sessionBefore).every(a=>a.decision==='acceptForSession'));});
 await check('interrupt stops actual subprocess and prevents late writes',async()=>{
   active={name:'interrupt-fixture',steps:[shell(`sleep 3; printf forbidden > '${cwd}/interrupt-late'`)],index:0};
   const before=client.events.length;const started=ok(await client.call('turn/start',{threadId:id,input:[{type:'text',text:'interrupt-fixture'}]}));
   const deadline=Date.now()+12000;let launched=false;
   while(Date.now()<deadline){if(client.events.slice(before).some(e=>e.method==='item/started'&&e.params.item?.type==='commandExecution')){launched=true;break;}await new Promise(r=>setTimeout(r,20));}
   assert.ok(launched,'command did not start');await new Promise(r=>setTimeout(r,150));
   ok(await client.call('turn/interrupt',{threadId:id,turnId:started.turn.id}));
   let completed;
   while(Date.now()<deadline){completed=client.events.slice(before).find(e=>e.method==='turn/completed'&&e.params.turn.id===started.turn.id);if(completed)break;await new Promise(r=>setTimeout(r,20));}
   assert.ok(completed,'interrupted turn did not complete');assert.equal(completed.params.turn.status,'interrupted');
   await new Promise(r=>setTimeout(r,3200));assert.equal(await exists(path.join(cwd,'interrupt-late')),false);
   assert.ok(completed.params.turn.items.filter(i=>i.type==='commandExecution').every(i=>i.status!=='inProgress'),'interrupted command item remained in progress');
   assert.ok(client.events.slice(before).some(e=>e.method==='item/completed'&&e.params.item?.type==='commandExecution'),'interrupted command missing terminal item notification');
   const persisted=JSON.parse(await readFile(path.join(scratch,'state','threads',`${id}.json`),'utf8'));
   const receipt=Object.values(persisted.nativeCommandReceipts||{}).find(r=>r.originalCall?.argumentsJson?.includes('interrupt-late'));
   assert.equal(receipt?.state,'unknown');assert.equal(receipt?.result,undefined);
 });
 const never=ok(await client.call('thread/start',{approvalsReviewer:'user',cwd,approvalPolicy:'never',sandbox:'workspace-write'}));const approvalCount=approvals.length;
 await turn(never.thread.id,'never-bypass',[shell(`printf forbidden > '${scratch}/never'`,true)]);
 await check('never approval policy rejects requested bypass',async()=>{assert.equal(await exists(path.join(scratch,'never')),false);assert.equal(approvals.length,approvalCount);});
 const ro=ok(await client.call('thread/start',{approvalsReviewer:'user',cwd,approvalPolicy:'never',sandbox:'read-only'}));
 await turn(ro.thread.id,'readonly-file',[file(path.join(cwd,'readonly-forbidden'),'blocked')]);
 await check('read-only policy reaches actual file executor',async()=>assert.equal(await exists(path.join(cwd,'readonly-forbidden')),false));
 const net=ok(await client.call('thread/start',{approvalsReviewer:'user',cwd,sandboxPolicy:{type:'workspaceWrite',networkAccess:true,excludeTmpdirEnvVar:true,excludeSlashTmp:true}}));
 const netTurn=await turn(net.thread.id,'network-enabled',[shell(`curl --noproxy '*' --max-time 2 http://127.0.0.1:${netServer.address().port}`)]);
 await check('network-enabled policy reaches same listener',()=>assert.ok(netTurn.events.some(e=>e.method==='item/completed'&&e.params.item.aggregatedOutput?.includes('fixture-network-ok'))));
 const extraDirectory=path.join(scratch,'second-workspace');await mkdir(extraDirectory);
 const multi=ok(await client.call('thread/start',{approvalsReviewer:'user',cwd,additionalDirectories:[extraDirectory],approvalPolicy:'on-request',sandbox:'workspace-write'}));
 await turn(multi.thread.id,'two-workspaces',[shell(`printf primary > '${cwd}/multi-primary'`),shell(`printf extra > '${extraDirectory}/multi-extra'`,false,extraDirectory),shell(`printf blocked > '${scratch}/multi-outside'`,false,extraDirectory)]);
 await check('two workspaces execute with second Cwd while sibling remains restricted',async()=>{assert.equal(await readFile(path.join(cwd,'multi-primary'),'utf8'),'primary');assert.equal(await readFile(path.join(extraDirectory,'multi-extra'),'utf8'),'extra');assert.equal(await exists(path.join(scratch,'multi-outside')),false);});
 const beforeFullApprovals=approvals.length;
 await turn(multi.thread.id,'switch-full-native',[nativeFile(path.join(scratch,'full-native.txt'),'alpha'),nativeEdit(path.join(scratch,'full-native.txt'),'alpha','beta'),shell(`printf full > '${extraDirectory}/full-command'`,false,extraDirectory)],{sandboxPolicy:{type:'dangerFullAccess'},approvalPolicy:'never'});
 await check('full access restores native file tools and task tool on same session',async()=>{assert.equal(await readFile(path.join(scratch,'full-native.txt'),'utf8'),'beta\n');assert.equal(await readFile(path.join(extraDirectory,'full-command'),'utf8'),'full');assert.equal(approvals.length,beforeFullApprovals);const declarations=declarationsFor('switch-full-native');assert.ok(propertiesOf(declarations.find(t=>t.name==='write_to_file')).Overwrite);assert.ok(propertiesOf(declarations.find(t=>t.name==='write_to_file')).Description);assert.ok(!propertiesOf(declarations.find(t=>t.name==='write_to_file')).BypassSandbox);assert.ok(propertiesOf(declarations.find(t=>t.name==='replace_file_content')).StartLine);assert.ok(declarations.some(t=>t.name==='manage_task'));});
 await turn(multi.thread.id,'switch-back-restricted',[file(path.join(scratch,'restricted-file-again'),'blocked'),shell(`printf blocked > '${scratch}/restricted-command-again'`),shell(`printf restored > '${extraDirectory}/restricted-restored'`,false,extraDirectory)],{sandboxPolicy:{type:'workspaceWrite',writableRoots:[cwd,extraDirectory],networkAccess:false,excludeTmpdirEnvVar:true,excludeSlashTmp:true},approvalPolicy:'on-request'});
 await check('same session restores file and command restrictions after full access',async()=>{assert.equal(await exists(path.join(scratch,'restricted-file-again')),false);assert.equal(await exists(path.join(scratch,'restricted-command-again')),false);assert.equal(await readFile(path.join(extraDirectory,'restricted-restored'),'utf8'),'restored');assert.equal(approvals.length,beforeFullApprovals);const declarations=declarationsFor('switch-back-restricted');assert.ok(propertiesOf(declarations.find(t=>t.name==='write_to_file')).BypassSandbox);assert.ok(!declarations.some(t=>t.name==='manage_task'));const persisted=JSON.parse(await readFile(path.join(scratch,'state','threads',`${multi.thread.id}.json`),'utf8'));assert.equal(persisted.id,multi.thread.id);assert.equal(persisted.permissionPolicy.mode,'workspaceWrite');});
 const persistent=shell(`printf forbidden > '${cwd}/persistent-forbidden'`);persistent.args.RunPersistent=true;
 const reused=shell(`printf forbidden > '${cwd}/reused-terminal-forbidden'`);reused.args.RequestedTerminalID='fixture-untrusted-terminal';
 const taskDenied=await turn(multi.thread.id,'restricted-task-adversarial',[persistent,reused,{name:'manage_task',args:{Action:'send_input',TaskId:'fixture-untrusted-task',Input:`printf forbidden > '${cwd}/task-input-forbidden'\n`,toolSummary:'Fixture input',toolAction:'Testing task input'}}]);
 ok(await client.call('thread/resume',{threadId:multi.thread.id}));
 await turn(multi.thread.id,'continue-after-task-denial',[shell(`printf continued > '${cwd}/after-task-denial'`)]);
 await check('restricted session rejects persistent terminals and hard-injected task input',async()=>{for(const name of ['persistent-forbidden','reused-terminal-forbidden','task-input-forbidden'])assert.equal(await exists(path.join(cwd,name)),false,name);assert.equal(await readFile(path.join(cwd,'after-task-denial'),'utf8'),'continued');assert.match(taskDenied.turn.error?.message||'',/unknown_tool/);assert.equal(approvals.length,beforeFullApprovals);});
 const planMode={mode:'plan',settings:{model:'gemini-2.5-flash',reasoning_effort:null,developer_instructions:null}};
 const plan=ok(await client.call('thread/start',{approvalsReviewer:'user',cwd,sandbox:'danger-full-access',collaborationMode:planMode}));
 const planApprovals=approvals.length;
 const planned=await turn(plan.thread.id,'plan-adversarial',[file(path.join(cwd,'plan-file'),'blocked'),shell(`printf blocked > '${cwd}/plan-shell'`),shell(`printf blocked > '${cwd}/plan-bypass'`,true),shell(`curl --noproxy '*' --max-time 2 http://127.0.0.1:${netServer.address().port}`),shell(`cat '${cwd}/file.txt'`)],{collaborationMode:planMode});
 await check('Plan overrides full access and denies file, shell write, bypass and network',async()=>{
   assert.deepEqual(plan.sandbox,{type:'readOnly',networkAccess:false});
   for(const name of ['plan-file','plan-shell','plan-bypass'])assert.equal(await exists(path.join(cwd,name)),false);
   assert.equal(approvals.length,planApprovals);
   const items=planned.events.filter(e=>e.method==='item/completed').map(e=>e.params.item);
   assert.ok(items.some(i=>i.command?.includes('curl')&&i.status==='failed'));
   assert.ok(items.some(i=>i.aggregatedOutput?.includes('beta')));
   const request=requests.find(r=>r.scenario==='plan-adversarial').body;
   assert.match(JSON.stringify(request.systemInstruction),/You are in Plan mode/);
   const tools=request.tools.flatMap(t=>t.functionDeclarations||[]);
   assert.ok(!tools.some(t=>/invoke_subagent|goal_update/.test(t.name)));
   const goal=await client.call('thread/goal/set',{threadId:plan.thread.id,objective:'write files'});assert.ok(goal.error);
 });
 await turn(plan.thread.id,'plan-default-restore',[nativeFile(path.join(cwd,'plan-restored'),'restored')]);
 await check('Default restores requested full access on same native session',async()=>{
   assert.equal(await readFile(path.join(cwd,'plan-restored'),'utf8'),'restored\n');
   const persisted=JSON.parse(await readFile(path.join(scratch,'state','threads',`${plan.thread.id}.json`),'utf8'));
   assert.equal(persisted.collaborationMode,'default');assert.equal(persisted.permissionPolicy.mode,'dangerFullAccess');
 });
 await check('unknown reviewer is rejected',async()=>{const r=await client.call('thread/start',{approvalsReviewer:'user',cwd,approvalsReviewer:'unknown-reviewer'});assert.ok(r.error);});
 await check('missing sandbox helper fails closed before a model call',async()=>{
   const missingHome=path.join(scratch,'missing-helper-state');
   const missing=new RpcClient(binary,[],{cwd:root,env:{...env,AGY_HARNESS_HOME:missingHome,AGY_APP_SERVER_HOME:missingHome,AGY_CODEX_SANDBOX_BINARY:path.join(scratch,'missing-helper')}});
   try{ok(await missing.call('initialize',{}));const before=requests.length;const result=await missing.call('thread/start',{approvalsReviewer:'user',cwd,sandbox:'workspace-write'});assert.ok(result.error);assert.equal(requests.length,before);}finally{await missing.close();}
 });
 await check('model history preserves original commands without execution wrappers',()=>{for(const request of requests)assert.doesNotMatch(JSON.stringify(request.body.contents),/--sandbox-state-json| # agy-native-command-/);const calls=requests.flatMap(r=>r.body.contents||[]).flatMap(c=>c.parts||[]).map(p=>p.functionCall).filter(c=>c?.name==='run_command');for(const step of steps.filter(s=>s.name==='run_command'))assert.ok(calls.some(c=>c.args.CommandLine===step.args.CommandLine),step.args.CommandLine);});
}catch(e){failure=e;checks.push({name:'fixture execution',status:'fail',error:e.stack});console.error(e.message);}
finally{
 clearInterval(responder);await client.close();fixture.close();netServer.close();
 await writeFile(path.join(scratch,'requests.json'),JSON.stringify(requests,null,2));await writeFile(path.join(scratch,'events.json'),JSON.stringify(client.events,null,2));await writeFile(path.join(scratch,'stderr.txt'),client.stderr);
 const report={createdAt:new Date().toISOString(),binary,scratch,paidCalls:0,scope:'Rust stdio + pinned native + scripted localhost model + Codex sandbox helper',checks,approvals};
 await writeFile(path.join(scratch,'report.json'),JSON.stringify(report,null,2));console.log(JSON.stringify({scratch,pass:checks.filter(c=>c.status==='pass').length,fail:checks.filter(c=>c.status==='fail').length}));
}
process.exitCode=failure||checks.some(c=>c.status==='fail')?1:0;
