import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { SessionStore } from '../src/core/session.js';
import type { ModelSpec } from '../src/core/types.js';

const SSE = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;
const say = (text: string) =>
  SSE({ choices: [{ index: 0, delta: { content: text } }] }) +
  SSE({ choices: [{ index: 0, delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 5 } }) +
  'data: [DONE]\n\n';
const call = (name: string, args: object, id = 'c1') =>
  SSE({ choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id, function: { name, arguments: JSON.stringify(args) } }] } }] }) +
  SSE({ choices: [{ index: 0, delta: {}, finish_reason: 'tool_calls' }], usage: { prompt_tokens: 10, completion_tokens: 5 } }) +
  'data: [DONE]\n\n';

/** A scripted model: `reply(body, n)` picks the SSE for each request (n = request index). */
async function mockModel(reply: (body: any, n: number) => string) {
  const bodies: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      const body = JSON.parse(raw);
      bodies.push(body);
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.end(reply(body, bodies.length - 1));
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const model: ModelSpec = {
    id: 'mock/m', provider: 'mock', model: 'm', api: 'openai-completions',
    baseUrl: `http://127.0.0.1:${(server.address() as AddressInfo).port}/v1`, apiKey: 'test', cost: { input: 1, output: 1 }
  };
  return { server, bodies, model };
}

const isSub = (body: any) => JSON.stringify(body.messages[0]).includes('# Sub-agent');
const toolNames = (body: any) => (body.tools ?? []).map((t: any) => t.function.name).sort();
const lastToolResult = (body: any) => [...body.messages].reverse().find((m: any) => m.role === 'tool')?.content ?? '';
/** the request answers a fresh user prompt (not a tool result) */
const fresh = (body: any) => body.messages.at(-1)?.role === 'user';

async function setup() {
  const home = await mkdtemp(join(tmpdir(), 'tango-task-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-task-work-'));
  process.env.TANGO_HOME = home;
  const host = await import('../src/host.js');
  const store = await SessionStore.create(join(home, 'sessions'), work);
  const done = async () => {
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  };
  return { home, work, host, store, done };
}

test('task: explore runs read-only in its own sub-session and reports back as the tool result', async () => {
  const { host, store, work, home, done } = await setup();
  let subBody: any;
  const m = await mockModel((body) => {
    if (isSub(body)) {
      subBody = body;
      return say('FOUND-X at a.ts:3');
    }
    if (fresh(body)) return call('task', { agent: 'explore', description: 'find X', prompt: 'look for X and report the line' });
    return say('parent done');
  });
  try {
    const agent = host.makeAgent(m.model, store, work, { mode: 'ask' });
    const events: any[] = [];
    agent.events.on('task_done', (e) => void events.push(e));
    const final = await agent.prompt('where is X?');
    assert.equal(final.content.map((b: any) => b.text ?? '').join(''), 'parent done');
    // the parent sees `task`; the sub-agent gets read tools only, never `task` (no recursion)
    assert.ok(toolNames(m.bodies[0]).includes('task'));
    const subTools = toolNames(subBody);
    assert.ok(subTools.includes('read') && subTools.includes('grep'), subTools.join(','));
    for (const t of ['task', 'write', 'edit', 'bash', 'office_write', 'notify']) assert.ok(!subTools.includes(t), `sub has ${t}`);
    assert.match(JSON.stringify(subBody.messages[0]), /read-only/);
    // the sub-agent sees only the delegated prompt, not the parent's conversation
    assert.ok(!JSON.stringify(subBody.messages).includes('where is X?'));
    const result = lastToolResult(m.bodies[2]);
    assert.match(result, /\[explore · session [0-9a-f]{12}\]\nFOUND-X at a\.ts:3/);
    // its own session, linked to the parent and hidden from lists by that link
    assert.equal(events.length, 1);
    assert.equal(events[0].background, false);
    const sub = await SessionStore.open(join(home, 'sessions'), events[0].sessionId);
    assert.equal(sub.meta()?.parent, store.id);
    assert.match(sub.meta()?.name ?? '', /^task:explore find X/);
    // session lists (the /resume picker, --sessions, --continue) skip it
    const listed = await SessionStore.list(join(home, 'sessions'));
    assert.equal(listed.find((x) => x.id === events[0].sessionId)?.parent, store.id);
    // the sub-agent's spend counts toward the parent's
    assert.ok(events[0].costUsd > 0 && agent.totalCostUsd > events[0].costUsd);
  } finally {
    m.server.close();
    await done();
  }
});

test('task: general may write through the parent gate; plan mode refuses it but still allows explore', async () => {
  const { host, store, work, done } = await setup();
  const target = join(work, 'out.txt');
  let subTools: string[] = [];
  let agentName = 'general';
  const m = await mockModel((body) => {
    if (isSub(body)) {
      subTools = toolNames(body);
      if (fresh(body) && agentName === 'general') return call('write', { path: target, content: 'hi' }, 's1');
      return say('wrote it');
    }
    if (fresh(body)) return call('task', { agent: agentName, description: 'write a file', prompt: 'write out.txt' });
    return say('ok');
  });
  try {
    const perm: any = { mode: 'ask' };
    const agent = host.makeAgent(m.model, store, work, perm);
    await agent.prompt('go');
    assert.equal(await readFile(target, 'utf8'), 'hi');
    assert.ok(subTools.includes('write') && subTools.includes('bash') && !subTools.includes('task'));

    perm.plan.on = true;
    const before = m.bodies.length;
    await agent.prompt('again');
    assert.match(lastToolResult(m.bodies.at(-1)), /plan mode/);
    assert.ok(!m.bodies.slice(before).some(isSub), 'no sub-agent was started');

    agentName = 'explore';
    await agent.prompt('read only is fine');
    assert.match(lastToolResult(m.bodies.at(-1)), /\[explore · session/);
  } finally {
    m.server.close();
    await done();
  }
});

test('task: background explore returns at once and delivers its report as a user message', async () => {
  const { host, store, work, done } = await setup();
  let release!: () => void;
  const gate = new Promise<void>((r) => (release = r));
  const m = await mockModel((body) => {
    if (isSub(body)) return say('BG-REPORT: auth lives in mw.ts');
    if (fresh(body)) {
      const text = JSON.stringify(body.messages.at(-1));
      if (text.includes('write please')) return call('task', { agent: 'general', description: 'w', prompt: 'p', background: true });
      if (text.includes('research')) return call('task', { agent: 'explore', description: 'auth research', prompt: 'find auth', background: true });
    }
    return say('ok');
  });
  try {
    const agent = host.makeAgent(m.model, store, work, { mode: 'ask' });
    const finished = new Promise<any>((r) => agent.events.on('task_done', (e) => void r(e)));
    agent.events.on('prompt_end', () => release());
    await agent.prompt('research auth');
    assert.match(lastToolResult(m.bodies[1]), /started background task t1 \(explore: auth research\)/);
    await gate;
    const e = await finished;
    assert.equal(e.background, true);
    assert.equal(e.ok, true);
    // the report is a user message in the parent session (never the model's own words)
    const report = store.messages().find((x) => JSON.stringify(x).includes('BG-REPORT'));
    assert.equal(report?.role, 'user');
    assert.match(JSON.stringify(report), /后台任务 t1 · explore：auth research 完成/);

    // background is read-only only: a writing sub-agent must run in the foreground
    await agent.prompt('write please');
    assert.match(lastToolResult(m.bodies.at(-1)), /background tasks must be read-only/);
  } finally {
    m.server.close();
    await done();
  }
});

test('task: custom sub-agents from config get their instruction; unknown names list what exists', async () => {
  const { host, store, work, done } = await setup();
  let subBody: any;
  let ask = 'reviewer';
  const m = await mockModel((body) => {
    if (isSub(body)) {
      subBody = body;
      return say('LGTM');
    }
    if (fresh(body)) return call('task', { agent: ask, description: 'review', prompt: 'review the diff' });
    return say('ok');
  });
  try {
    const cfg = { subAgents: [{ name: 'reviewer', description: 'strict code review', instruction: 'REVIEW-RULES: flag every TODO', readonly: true }] };
    const agent = host.makeAgent(m.model, store, work, { mode: 'ask' }, cfg);
    const schema = agent.tools.schemas().find((t) => t.name === 'task')!;
    assert.match(schema.description, /- reviewer \(read-only\): strict code review/);
    assert.match(schema.description, /- explore \(read-only\)/);
    await agent.prompt('review it');
    assert.match(JSON.stringify(subBody.messages[0]), /REVIEW-RULES: flag every TODO/);
    assert.ok(!toolNames(subBody).includes('bash'), 'readonly custom agent has no bash');
    assert.match(lastToolResult(m.bodies.at(-1)), /\[reviewer · session/);

    ask = 'nobody';
    await agent.prompt('again');
    assert.match(lastToolResult(m.bodies.at(-1)), /unknown sub-agent "nobody"; available: explore, general, reviewer/);
  } finally {
    m.server.close();
    await done();
  }
});

test('readonly mode: explore may be delegated, a writing sub-agent may not', async () => {
  const { host, store, work, done } = await setup();
  let ask = 'general';
  const m = await mockModel((body) => {
    if (isSub(body)) return say('read');
    if (fresh(body)) return call('task', { agent: ask, description: 'd', prompt: 'p' });
    return say('ok');
  });
  try {
    const agent = host.makeAgent(m.model, store, work, { mode: 'readonly' });
    await agent.prompt('one');
    assert.match(lastToolResult(m.bodies.at(-1)), /read-only mode: only read-only sub-agents/);
    ask = 'explore';
    await agent.prompt('two');
    assert.match(lastToolResult(m.bodies.at(-1)), /\[explore · session/);
  } finally {
    m.server.close();
    await done();
  }
});

test('deliver: idle reports are stored now, mid-turn ones ride the next request without the 补充 prefix', async () => {
  const { host, store, work, done } = await setup();
  let agent: any;
  const m = await mockModel((body, n) => {
    if (n === 0) {
      void agent.deliver('MIDTURN-REPORT');
      return call('ls', { path: '.' });
    }
    return say('ok');
  });
  try {
    agent = host.makeAgent(m.model, store, work, { mode: 'ask' });
    assert.equal(await agent.deliver('IDLE-REPORT'), 'stored');
    assert.equal(store.messages().at(-1)?.role, 'user');
    await agent.prompt('go');
    const seen = JSON.stringify(m.bodies[1].messages);
    assert.ok(seen.includes('IDLE-REPORT') && seen.includes('MIDTURN-REPORT'));
    assert.ok(!seen.includes('（补充）MIDTURN-REPORT'));
  } finally {
    m.server.close();
    await done();
  }
});

test('task: an interrupted sub-agent hands back its work so far; each of its steps is announced', async () => {
  const { host, store, work, done } = await setup();
  await writeFile(join(work, 'notes.txt'), 'YINGZHI-DECK at /dev/pitch/a.pptx\n');
  let parent: any;
  const m = await mockModel((body) => {
    if (isSub(body)) {
      // step 1: read; when its result comes back, the user interrupts the turn
      if (fresh(body)) return call('read', { path: join(work, 'notes.txt') }, 's1');
      parent.abort();
      return say('never seen');
    }
    if (fresh(body)) return call('task', { agent: 'explore', description: 'find deck', prompt: 'find the deck' }, 'p1');
    return say('parent done');
  });
  try {
    parent = host.makeAgent(m.model, store, work, { mode: 'ask' });
    const progress: any[] = [];
    parent.events.on('task_progress', (e: any) => void progress.push(e));
    let result = '';
    parent.events.on('after_tool', ({ call, result: r }: any) => {
      if (call.name === 'task') result = r.content.map((b: any) => b.text ?? '').join('');
    });
    await parent.prompt('where is the deck?');
    // the tool result carries what the sub-agent had read, not a bare "aborted"
    assert.match(result, /interrupted before its report/);
    assert.match(result, /\[read .*notes\.txt\]\n.*YINGZHI-DECK/);
    // its step was announced, tagged with the parent's task call id
    assert.deepEqual(
      progress.map((p) => [p.callId, p.agent, p.step, p.tool]),
      [['p1', 'explore', 1, 'read']]
    );
    assert.match(progress[0].detail, /notes\.txt$/);
  } finally {
    m.server.close();
    await done();
  }
});

test('partialReport: newest results first, within the cap', async () => {
  const { partialReport } = await import('../src/host.js');
  const msgs: any[] = [
    { role: 'assistant', content: [{ type: 'text', text: 'looking' }, { type: 'tool_call', id: 'a', name: 'grep', input: { pattern: 'x' } }] },
    { role: 'user', content: [{ type: 'tool_result', toolCallId: 'a', content: [{ type: 'text', text: 'OLD'.repeat(10) }] }] },
    { role: 'assistant', content: [{ type: 'tool_call', id: 'b', name: 'find', input: { pattern: 'y' } }] },
    { role: 'user', content: [{ type: 'tool_result', toolCallId: 'b', content: [{ type: 'text', text: 'NEW' }] }] }
  ];
  const r = partialReport(msgs);
  assert.ok(r.startsWith('last note: looking'));
  assert.ok(r.indexOf('[find y]\nNEW') < r.indexOf('[grep x]'));
  assert.ok(partialReport(msgs, 260).length <= 260);
  assert.equal(partialReport([]), '');
});
