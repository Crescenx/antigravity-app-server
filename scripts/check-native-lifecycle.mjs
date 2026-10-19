import {appCoreEnv} from './app-core-env.mjs';
// No model calls: isolated state; explicit native account is required for model catalog access.
import assert from 'node:assert/strict';
import {mkdir,mkdtemp} from 'node:fs/promises';
import path from 'node:path';
import {RpcClient} from '../test/spec/rpc-client.mjs';
const root=path.resolve(import.meta.dirname,'..');
await mkdir(path.join(root,'.tmp'),{recursive:true});
const scratch=await mkdtemp(path.join(root,'.tmp/native-lifecycle-'));
const cwd=path.join(scratch,'workspace');await mkdir(cwd);
const env={...appCoreEnv(),AGY_HARNESS_HOME:path.join(scratch,'state')};

const binary=process.env.HARNESS_BINARY||path.join(root,'target/debug/antigravity-app-server');
const ok=response=>{assert.ok(!response.error,JSON.stringify(response.error));return response.result;};
let id;
const first=new RpcClient(binary,[],{env,cwd:root});
try{
  ok(await first.call('initialize',{},false));
  const started=ok(await first.call('thread/start',{cwd},false));
  id=started.thread.id;assert.equal(started.native.cascadeId,id);
  assert.ok((await first.call('thread/configure',{threadId:id,expectedRevision:999})).error);
  const updated=ok(await first.call('thread/configure',{threadId:id,expectedRevision:0,additionalDirectories:[cwd]}));
  assert.equal(updated.thread.revision,1);assert.equal(updated.native.cascadeId,id);
  ok(await first.call('thread/unload',{threadId:id}));
}finally{await first.close();}
const cold=new RpcClient(binary,[],{env,cwd:root});
try{
  ok(await cold.call('initialize',{},false));
  const resumed=ok(await cold.call('thread/resume',{threadId:id},false));
  assert.equal(resumed.native.cascadeId,id);assert.equal(resumed.thread.status.type,'idle');
  assert.deepEqual(resumed.thread.turns,[]);
  ok(await cold.call('thread/unload',{threadId:id},false));
}finally{await cold.close();}
console.log(JSON.stringify({passed:true,modelCalls:0,configurationRebuild:true,coldResume:true,threadId:id}));
