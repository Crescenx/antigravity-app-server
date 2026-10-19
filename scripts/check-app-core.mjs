// Explicit real-account lifecycle smoke; at most three logical generations per
// model, no tools, no retries. This is not a full permissions regression suite.
import assert from 'node:assert/strict';
import {mkdir, mkdtemp, writeFile} from 'node:fs/promises';
import path from 'node:path';
import {RpcClient} from '../test/spec/rpc-client.mjs';
import {appCoreEnv} from './app-core-env.mjs';

assert.ok(process.argv.includes('--live'), 'Explicit --live required; this test performs paid Gemini and Claude inference');
const env = appCoreEnv();
const root = path.resolve(import.meta.dirname, '..');
const binary = process.env.AGY_APP_SERVER_BINARY || path.join(root, 'target/debug/antigravity-app-server');
await mkdir(path.join(root, '.tmp'), {recursive:true});
const scratch = await mkdtemp(path.join(root, '.tmp/app-core-smoke-'));
const cwd = path.join(scratch, 'workspace');
await mkdir(cwd);
env.AGY_APP_SERVER_HOME = path.join(scratch, 'state');
const report = {scratch, runtime:'app-core-v1', maxModelCallsPerModel:3, timeoutMsPerTurn:120000,
  scope:'Gemini and Claude text generation, persisted history and cold resume; no tool or permission claims', checks:[], passed:false};
const ok = response => { assert.ok(!response.error, JSON.stringify(response.error)); return response.result; };
let client, active;
const events = [];
async function connect() {
  client = new RpcClient(binary, [], {cwd:root, env});
  ok(await client.call('initialize', {}));
}
async function disconnect() {
  if (!client) return;
  await client.close();
  events.push(...client.events);
  client = undefined;
}
async function turn(threadId, marker) {
  const start = ok(await client.call('turn/start', {threadId, input:[{type:'text', text:`Reply with exactly ${marker}. Do not call tools.`}]}));
  active = {threadId, turnId:start.turn.id};
  const deadline = Date.now() + report.timeoutMsPerTurn;
  while (Date.now() < deadline) {
    const done = client.events.find(e => e.method === 'turn/completed' && e.params.turn.id === start.turn.id);
    if (done) {
      active = undefined;
      assert.equal(done.params.turn.status, 'completed', JSON.stringify(done.params.turn.error));
      const text = done.params.turn.items.filter(i => i.type === 'agentMessage').map(i => i.text).join('\n');
      assert.ok(text.includes(marker), 'expected response marker missing');
      assert.ok(!client.events.some(e => e.method === 'item/completed' && ['commandExecution','mcpToolCall','fileChange'].includes(e.params.item.type)), 'tools-disabled session executed a tool');
      return done.params.turn.id;
    }
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  throw new Error('Turn deadline exceeded; request will be interrupted, never replayed');
}
try {
  await connect();
  const catalog = ok(await client.call('model/list', {limit:100})).data;
  const select = (family, preferred, override) => {
    const name = override || catalog.find(m => m.model === preferred)?.model || catalog.find(m => m.model.startsWith(family))?.model;
    assert.ok(name && name.startsWith(family) && catalog.some(m => m.model === name), `No advertised ${family} model matching selection`);
    return name;
  };
  const models = [select('gemini-', 'gemini-3-flash', process.env.AGY_SMOKE_GEMINI_MODEL), select('claude-', 'claude-sonnet-4-6', process.env.AGY_SMOKE_CLAUDE_MODEL)];
  for (const [index, model] of models.entries()) {
    const started = ok(await client.call('thread/start', {cwd, model, approvalPolicy:'never', sandbox:'read-only',
      config:{tools_disabled:true, mcp_servers:{}}, budget:{maxModelCalls:3, scope:'BUDGET_SCOPE_LIFETIME'}}));
    const id = started.thread.id;
    assert.equal(started.native.cascadeId, id);
    const first = await turn(id, `APP_CORE_${index}_FIRST`);
    ok(await client.call('thread/unload', {threadId:id}));
    await disconnect();
    await connect();
    const resumed = ok(await client.call('thread/resume', {threadId:id}));
    assert.equal(resumed.native.cascadeId, id);
    const saved = ok(await client.call('thread/read', {threadId:id, includeTurns:true})).thread;
    assert.ok(saved.turns.some(t => t.id === first));
    await turn(id, `APP_CORE_${index}_RESUMED`);
    const final = ok(await client.call('thread/read', {threadId:id, includeTurns:true})).thread;
    assert.equal(final.turns.length, 2);
    report.checks.push({model, threadId:id, turns:2, coldResume:true, passed:true, usage:final.tokenUsage});
    ok(await client.call('thread/unload', {threadId:id}));
  }
  report.passed = true;
} catch (error) {
  report.error = error.message;
  process.exitCode = 1;
} finally {
  if (active && client) { try { await client.call('turn/interrupt', active); } catch {} }
  await disconnect();
  await writeFile(path.join(scratch, 'events.json'), JSON.stringify(events, null, 2));
  await writeFile(path.join(scratch, 'report.json'), JSON.stringify(report, null, 2));
  console.log(JSON.stringify(report, null, 2));
}
