import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { Agent } from '../src/core/agent.js';
import { SessionStore } from '../src/core/session.js';
import { ToolRegistry } from '../src/core/tools/registry.js';
import { bashTool } from '../src/core/tools/bash.js';

const chunk = (delta: object, finish: string | null = null) =>
  `data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;

test('agent runs model → tool → model over an OpenAI-compatible SSE endpoint', async () => {
  const bodies: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      const body = JSON.parse(raw);
      bodies.push(body);
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      if (bodies.length === 1) {
        res.write(chunk({ tool_calls: [{ index: 0, id: 'c1', function: { name: 'bash', arguments: '{"command":' } }] }));
        res.write(chunk({ tool_calls: [{ index: 0, function: { arguments: '"echo tango-ok"}' } }] }, 'tool_calls'));
      } else {
        res.write(chunk({ content: 'done' }, 'stop'));
      }
      res.write(`data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 10, completion_tokens: 5 } })}\n\n`);
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;
  const dir = await mkdtemp(join(tmpdir(), 'tango-agent-'));
  try {
    const store = await SessionStore.create(dir, dir);
    const tools = new ToolRegistry();
    tools.register(bashTool);
    const agent = new Agent({
      store,
      tools,
      cwd: dir,
      model: {
        id: 'fake/m',
        provider: 'fake',
        model: 'm',
        api: 'openai-completions',
        baseUrl: `http://127.0.0.1:${port}/v1`,
        apiKey: 'test',
        cost: { input: 1, output: 1 }
      }
    });
    const final = await agent.prompt('say hi');
    assert.equal(final.stopReason, 'stop');
    assert.equal(bodies.length, 2);
    const toolMsg = bodies[1].messages.find((m: any) => m.role === 'tool');
    assert.equal(toolMsg.tool_call_id, 'c1');
    assert.match(toolMsg.content, /tango-ok/);
    // user, assistant(tool_call), user(tool_result), assistant(final)
    assert.equal(store.messages().length, 4);
    assert.ok(agent.contextTokenEstimate() > 0);
    assert.ok(agent.totalCostUsd > 0);
  } finally {
    server.close();
    await rm(dir, { recursive: true, force: true });
  }
});

test('openai messages: tool-call-only assistant turns omit content instead of sending null', async () => {
  const { toOpenAIMessages } = await import('../src/core/models/openai.js');
  const msgs = toOpenAIMessages('sys', [
    { role: 'assistant', content: [{ type: 'tool_call', id: 'c1', name: 'ls', input: {} }] },
    { role: 'assistant', content: [{ type: 'text', text: 'hi' }] }
  ] as any);
  assert.equal('content' in msgs[1], false);
  assert.equal(msgs[1].tool_calls.length, 1);
  assert.equal(msgs[2].content, 'hi');
  assert.equal('reasoning_content' in msgs[2], false);
});

test('openai messages: thinking is sent back as reasoning_content', async () => {
  const { toOpenAIMessages } = await import('../src/core/models/openai.js');
  const msgs = toOpenAIMessages('sys', [
    {
      role: 'assistant',
      content: [
        { type: 'thinking', text: 'need ls' },
        { type: 'tool_call', id: 'c1', name: 'ls', input: {} }
      ]
    }
  ] as any);
  assert.equal(msgs[1].reasoning_content, 'need ls');
  assert.equal(msgs[1].tool_calls.length, 1);
});

test('openai messages: vendor extra on tool calls is echoed back as extra_content', async () => {
  const { toOpenAIMessages } = await import('../src/core/models/openai.js');
  const extra = { google: { thought_signature: 'sig' } };
  const msgs = toOpenAIMessages('sys', [
    { role: 'assistant', content: [{ type: 'tool_call', id: 'c1', name: 'ls', input: {}, extra }] }
  ] as any);
  assert.deepEqual(msgs[1].tool_calls[0].extra_content, extra);
});

test('malformed tool arguments become a tool error the model can see, not a crash', async () => {
  const bodies: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      bodies.push(JSON.parse(raw));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      if (bodies.length === 1) {
        res.write(chunk({ tool_calls: [{ index: 0, id: 'c1', function: { name: 'bash', arguments: '{"command": "ls' } }] }, 'tool_calls'));
      } else {
        res.write(chunk({ content: 'retrying' }, 'stop'));
      }
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;
  const dir = await mkdtemp(join(tmpdir(), 'tango-agent-'));
  try {
    const store = await SessionStore.create(dir, dir);
    const tools = new ToolRegistry();
    tools.register(bashTool);
    const agent = new Agent({
      store,
      tools,
      cwd: dir,
      model: { id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'test' }
    });
    const final = await agent.prompt('go');
    assert.equal(final.stopReason, 'stop');
    const toolMsg = bodies[1].messages.find((m: any) => m.role === 'tool');
    assert.match(toolMsg.content, /invalid arguments for bash/);
    const call = bodies[1].messages.find((m: any) => m.tool_calls);
    assert.equal(call.tool_calls[0].function.arguments, '{}');
  } finally {
    server.close();
    await rm(dir, { recursive: true, force: true });
  }
});

test('network failures name the cause instead of "fetch failed"', async () => {
  const { describeNetError } = await import('../src/core/models/util.js');
  assert.match(describeNetError({ cause: { code: 'ENOTFOUND' } }), /DNS lookup failed/);
  assert.match(describeNetError({ cause: { code: 'ECONNREFUSED' } }), /refused/);
});

test('HTTP errors and non-JSON stream events surface readable messages', async () => {
  const server = createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      if (req.url?.includes('bad')) {
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        res.end('data: <html>gateway</html>\n\n');
      } else {
        res.writeHead(503);
        res.end('No healthy upstream');
      }
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;
  const { openaiAdapter } = await import('../src/core/models/openai.js');
  const spec = (path: string) => ({ id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${port}/${path}`, apiKey: 'k' }) as any;
  const drain = async (s: any) => {
    for await (const _ of openaiAdapter.stream(s, { system: '', messages: [], tools: [] } as any)) void _;
  };
  try {
    await assert.rejects(drain(spec('v1')), /HTTP 503 No healthy upstream/);
    await assert.rejects(drain(spec('bad')), /malformed stream event/);
  } finally {
    server.close();
  }
});

test('agent ask uses context without tools or session writes', async () => {
  const bodies: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      bodies.push(JSON.parse(raw));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write(chunk({ content: 'side answer' }, 'stop'));
      res.write(`data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 12, completion_tokens: 3 } })}\n\n`);
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;
  const dir = await mkdtemp(join(tmpdir(), 'tango-agent-ask-'));
  try {
    const store = await SessionStore.create(dir, dir);
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'existing context' }] });
    const agent = new Agent({
      store,
      tools: new ToolRegistry(),
      cwd: dir,
      model: { id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'test' }
    });
    const before = JSON.stringify(store.allMessages());
    const streamed: string[] = [];
    const answer = await agent.ask('side question', (ev) => {
      if (ev.type === 'text') streamed.push(ev.delta);
    });
    assert.equal(answer.content[0].type, 'text');
    assert.equal(streamed.join(''), 'side answer');
    assert.deepEqual(bodies[0].messages.at(-1).content, [{ type: 'text', text: 'side question' }]);
    assert.deepEqual(
      bodies[0].messages.find((m: any) => m.role === 'user' && JSON.stringify(m.content).includes('existing context')).content,
      [{ type: 'text', text: 'existing context' }]
    );
    assert.equal(bodies[0].tools, undefined);
    assert.equal(JSON.stringify(store.allMessages()), before);
  } finally {
    server.close();
    await rm(dir, { recursive: true, force: true });
  }
});

test('installCheckpoints: one snapshot per prompt, before the first write-effect tool', async () => {
  const { installCheckpoints, checkpointEnabled, latestCheckpoints } = await import('../src/core/checkpoint.js');
  const { EventBus } = await import('../src/core/events.js');
  const { spawnSync } = await import('node:child_process');
  const { writeFile } = await import('node:fs/promises');

  const bodies: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      bodies.push(JSON.parse(raw));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      if (bodies.length % 2 === 1) {
        res.write(chunk({ tool_calls: [{ index: 0, id: `c${bodies.length}`, function: { name: 'bash', arguments: JSON.stringify({ command: 'echo turn-work >> work.txt' }) } }] }, 'tool_calls'));
      } else {
        res.write(chunk({ content: 'done' }, 'stop'));
      }
      res.write(`data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 10, completion_tokens: 5 } })}\n\n`);
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;

  const dir = await mkdtemp(join(tmpdir(), 'tango-ckpt-'));
  try {
    const g = (args: string[]) => spawnSync('git', args, { cwd: dir, encoding: 'utf8' });
    g(['init', '-q']); g(['config', 'user.email', 't@t']); g(['config', 'user.name', 't']);
    await writeFile(join(dir, 'seed.txt'), 'seed\n'); g(['add', '.']); g(['commit', '-q', '-m', 'init']);

    const store = await SessionStore.create(join(dir, 'sessions'), dir);
    const events = new EventBus();
    installCheckpoints(events, { cwd: dir, store });
    const tools = new ToolRegistry();
    tools.register(bashTool);
    const agent = new Agent({
      store, tools, events, cwd: dir,
      model: { id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'test' }
    });

    // turn 1: one write tool → pre-turn + post-turn checkpoint (post captures leftovers)
    await agent.prompt('do work');
    let cps = store.entriesOf('checkpoint');
    assert.equal(cps.length, 2, 'pre + post snapshots for a write turn');
    assert.ok((cps[0] as any).hash.length > 10);
    assert.equal(spawnSync('git', ['cat-file', '-t', (cps[0] as any).hash], { cwd: dir, encoding: 'utf8' }).stdout.trim(), 'commit');
    // the PRE snapshot predates the tool's write; the POST one contains it
    const { snapshotFileList } = await import('../src/core/git-snapshot.js');
    assert.ok(!(await snapshotFileList(dir, (cps[0] as any).hash)).has('work.txt'));
    assert.ok((await snapshotFileList(dir, (cps[1] as any).hash)).has('work.txt'));

    // turn 2: another write tool → two more checkpoints
    await agent.prompt('more work');
    cps = store.entriesOf('checkpoint');
    assert.equal(cps.length, 4);

    assert.equal(latestCheckpoints(store).length, 4);
    assert.ok(latestCheckpoints(store)[0].entryIndex > 0, 'checkpoint carries its raw entry index');

    // config gate
    assert.equal(checkpointEnabled({}), true);
    assert.equal(checkpointEnabled({ checkpoint: 'off' }), false);
    assert.equal(checkpointEnabled({ checkpoint: 'on' }), true);
  } finally {
    server.close();
    await rm(dir, { recursive: true, force: true });
  }
});

test('installCheckpoints: a failing snapshot never vetoes the tool', async () => {
  const { installCheckpoints } = await import('../src/core/checkpoint.js');
  const { EventBus } = await import('../src/core/events.js');
  const dir = await mkdtemp(join(tmpdir(), 'tango-ckpt2-'));
  try {
    const store = await SessionStore.create(dir, dir);
    const events = new EventBus();
    installCheckpoints(events, {
      cwd: dir,
      store,
      snapshot: async () => { throw new Error('boom'); }
    });
    let ran = false;
    events.on('after_tool', () => { ran = true; });
    const tool = { name: 'write', description: 'd', input: {}, writeEffect: true, execute: async () => ({ content: [{ type: 'text' as const, text: 'ok' }] }) };
    const veto = await events.emit('before_tool', {
      call: { type: 'tool_call', id: 'x', name: 'write', input: {} },
      tool: tool as any
    });
    assert.equal(veto, undefined); // no veto despite snapshot failure
    assert.equal(store.entriesOf('checkpoint').length, 0);

    // read-only tools never trigger a checkpoint
    await events.emit('prompt_start', { text: 'go' });
    const readTool = { name: 'read', description: 'd', input: {}, execute: async () => ({ content: [{ type: 'text' as const, text: 'ok' }] }) };
    await events.emit('before_tool', { call: { type: 'tool_call', id: 'y', name: 'read', input: {} }, tool: readTool as any });
    assert.equal(store.entriesOf('checkpoint').length, 0);
    // prompt with no write tool at all → armed, but nothing fires
    const writeTool2 = { ...tool, name: 'write2' };
    await events.emit('prompt_start', { text: 'go2' });
    await events.emit('before_tool', { call: { type: 'tool_call', id: 'z', name: 'write2', input: {} }, tool: { ...writeTool2, writeEffect: false } as any });
    assert.equal(store.entriesOf('checkpoint').length, 0);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
