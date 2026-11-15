import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { createInterface } from 'node:readline';

/**
 * End-to-end protocol test: real `tango serve` child process against a local
 * mock OpenAI-compatible SSE endpoint. No API keys, no network.
 */

const SSE = (obj: unknown) => `data: ${JSON.stringify(obj)}\n\n`;

async function startMock() {
  const requests: any[] = [];
  let script: () => string = () => '';
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      requests.push(JSON.parse(raw));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.end(script());
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  return { server, requests, setScript: (f: () => string) => (script = f) };
}

async function startServe(home: string, cwd: string, port: number, extraEnv: Record<string, string> = {}) {
  const env: NodeJS.ProcessEnv = { ...process.env, TANGO_HOME: home, MOCK_SERVE_KEY: 'sk-test' };
  for (const k of ['HTTP_PROXY', 'http_proxy', 'HTTPS_PROXY', 'https_proxy', 'TANGO_RESUME', 'TANGO_MODE', 'TANGO_MODEL']) delete env[k];
  Object.assign(env, extraEnv);
  const child = spawn(process.execPath, ['--import', 'tsx', 'src/cli.ts', 'serve', '-m', 'mock/mock', '--cwd', cwd], {
    cwd: join(import.meta.dirname, '..'),
    env
  }) as ChildProcessWithoutNullStreams;
  const frames: any[] = [];
  const waiters: ((f: any) => void)[] = [];
  createInterface({ input: child.stdout }).on('line', (line) => {
    const frame = JSON.parse(line);
    const wait = waiters.shift();
    if (wait) wait(frame);
    else frames.push(frame);
  });
  const next = () =>
    new Promise<any>((resolve, reject) => {
      if (frames.length) return resolve(frames.shift());
      waiters.push(resolve);
      setTimeout(() => reject(new Error('timeout waiting for a frame')), 15_000).unref?.();
    });
  const until = async (pred: (f: any) => boolean, what: string) => {
    for (let i = 0; i < 100; i++) {
      const f = await next();
      if (f.type === 'error') throw new Error(`server error frame: ${f.message}`);
      if (pred(f)) return f;
    }
    throw new Error(`no "${what}" frame`);
  };
  const send = (cmd: object) => child.stdin.write(JSON.stringify(cmd) + '\n');
  return { child, send, next, until };
}

test('serve: hello, status, prompt with a permission denial, session persistence', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(
    join(home, 'models.json'),
    JSON.stringify([
      { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY' }
    ])
  );

  // turn 1: model asks to write outside cwd (permission → deny); turn 2: plain answer
  let call = 0;
  mock.setScript(() =>
    call++ === 0
      ? SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'c1', function: { name: 'write', arguments: JSON.stringify({ path: join(work, '..', 'escape.txt'), content: 'x' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n'
      : SSE({ choices: [{ delta: { content: '好，不写了' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 40, completion_tokens: 5, total_tokens: 45 } }) + 'data: [DONE]\n\n'
  );

  const s = await startServe(home, work, 0);
  try {
    const hello = await s.next();
    assert.equal(hello.type, 'hello');
    assert.equal(hello.protocol, 1);
    assert.equal(hello.model, 'mock/mock');
    assert.equal(hello.mode, 'ask');
    assert.ok(hello.sessionId && hello.tools.some((t: any) => t.name === 'write'));

    s.send({ id: 1, cmd: 'status' });
    const status = await s.until((f) => f.type === 'result' && f.id === 1, 'status result');
    assert.equal(status.ok, true);
    assert.equal(status.data.model, 'mock/mock');

    s.send({ id: 2, cmd: 'prompt', text: '把文件写到上一级目录' });
    const perm = await s.until((f) => f.type === 'permission_request', 'permission request');
    assert.equal(perm.call.name, 'write');
    assert.match(perm.reason, /outside the working directory/);
    s.send({ cmd: 'permission', request: perm.request, allow: false });

    const blocked = await s.until((f) => f.type === 'tool_result', 'tool_result');
    assert.equal(blocked.isError, true);
    assert.match(blocked.output, /blocked/);

    const msg = await s.until((f) => f.type === 'message', 'message');
    assert.equal(msg.message.content[0].text, '好，不写了');
    const te = await s.until((f) => f.type === 'turn_end', 'turn_end');
    assert.equal(te.usage.input, 40);
    const res = await s.until((f) => f.type === 'result' && f.id === 2, 'prompt result');
    assert.deepEqual(res.data, { stopReason: 'stop' });

    s.send({ id: 3, cmd: 'bogus' });
    const bad = await s.until((f) => f.type === 'result' && f.id === 3, 'unknown-cmd result');
    assert.equal(bad.ok, false);
    assert.match(bad.error, /unknown command/);

    // the model saw the denial and answered after it; session file has it all
    assert.equal(mock.requests.length, 2);
    const toolMsg = mock.requests[1].messages.find((m: any) => m.role === 'tool');
    assert.match(toolMsg.content, /user declined/);
    const body = await readFile(join(home, 'sessions', `${hello.sessionId}.jsonl`), 'utf8');
    assert.match(body, /"kind":"message"/);
    assert.match(body, /user declined/);

    s.child.stdin.end();
    await new Promise<number>((r) => s.child.on('exit', r));
  } finally {
    s.child.kill();
    mock.server.close();
  }
});

test('serve: stdin EOF does not cut a queued prompt short (pipe usage)', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(
    join(home, 'models.json'),
    JSON.stringify([
      { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY' }
    ])
  );
  mock.setScript(() => SSE({ choices: [{ delta: { content: '管道里的答复' }, finish_reason: 'stop' }] }) + 'data: [DONE]\n\n');

  const s = await startServe(home, work, 0);
  try {
    await s.next(); // hello
    s.send({ id: 1, cmd: 'prompt', text: '说一句' });
    s.child.stdin.end(); // EOF while the prompt is still in flight
    const msg = await s.until((f) => f.type === 'message', 'message after EOF');
    assert.equal(msg.message.content[0].text, '管道里的答复');
    const res = await s.until((f) => f.type === 'result' && f.id === 1, 'result after EOF');
    assert.deepEqual(res.data, { stopReason: 'stop' });
    await new Promise<number>((r) => s.child.on('exit', r)); // then it exits on its own
  } finally {
    s.child.kill();
    mock.server.close();
  }
});

test('serve: set_mode and read-only mode are enforced', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(
    join(home, 'models.json'),
    JSON.stringify([
      { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY' }
    ])
  );
  let call = 0;
  mock.setScript(() =>
    call++ === 0
      ? SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'c1', function: { name: 'bash', arguments: JSON.stringify({ command: 'rm -rf /tmp/nope' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n'
      : SSE({ choices: [{ delta: { content: 'ok' }, finish_reason: 'stop' }] }) + 'data: [DONE]\n\n'
  );

  const s = await startServe(home, work, 0);
  try {
    const hello = await s.next();
    assert.equal(hello.mode, 'ask');

    s.send({ id: 1, cmd: 'set_mode', mode: 'readonly' });
    const modeRes = await s.until((f) => f.type === 'result' && f.id === 1, 'set_mode result');
    assert.deepEqual(modeRes.data, { mode: 'readonly' });

    s.send({ id: 2, cmd: 'prompt', text: '删掉那个目录' });
    // readonly denies without asking: no permission_request frame, straight to a blocked tool_result
    const blocked = await s.until((f) => f.type === 'tool_result', 'blocked tool_result');
    assert.equal(blocked.isError, true);
    assert.match(blocked.output, /read-only mode/);
    await s.until((f) => f.type === 'result' && f.id === 2, 'prompt result');

    s.send({ id: 3, cmd: 'set_mode', mode: 'nope' });
    const bad = await s.until((f) => f.type === 'result' && f.id === 3, 'bad mode result');
    assert.equal(bad.ok, false);
  } finally {
    s.child.kill();
    mock.server.close();
  }
});

test('serve: sessions listing, TANGO_RESUME restores context, TANGO_MODE, save_skill', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(
    join(home, 'models.json'),
    JSON.stringify([
      { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY' }
    ])
  );
  mock.setScript(() => SSE({ choices: [{ delta: { content: '收到' }, finish_reason: 'stop' }] }) + 'data: [DONE]\n\n');

  // first child: one turn, then exit
  const a = await startServe(home, work, 0);
  let firstId: string;
  try {
    const hello = await a.next();
    firstId = hello.sessionId;
    a.send({ id: 1, cmd: 'prompt', text: '暗号是月亮饼' });
    await a.until((f) => f.type === 'result' && f.id === 1, 'prompt result');
  } finally {
    a.child.kill();
  }

  // second child resumes by prefix through the environment, in auto mode
  const b = await startServe(home, work, 0, { TANGO_RESUME: firstId.slice(0, 8), TANGO_MODE: 'auto' });
  try {
    const hello = await b.next();
    assert.equal(hello.sessionId, firstId);
    assert.equal(hello.mode, 'auto');

    b.send({ id: 2, cmd: 'sessions' });
    const list = await b.until((f) => f.type === 'result' && f.id === 2, 'sessions result');
    const cur = list.data.sessions.find((s: any) => s.current);
    assert.equal(cur.id, firstId);
    assert.ok(cur.messages >= 2);

    b.send({ id: 3, cmd: 'prompt', text: '暗号是什么' });
    await b.until((f) => f.type === 'result' && f.id === 3, 'prompt result');
    // the resumed turn sent the earlier conversation to the model
    const last = JSON.stringify(mock.requests.at(-1).messages);
    assert.match(last, /月亮饼/);

    b.send({ id: 4, cmd: 'save_skill', arg: 'finish' });
    const drafts = await b.until((f) => f.type === 'result' && f.id === 4, 'drafts result');
    assert.deepEqual(drafts.data, { drafts: [] });
    b.send({ id: 5, cmd: 'save_skill', arg: '暗号流程' });
    const saved = await b.until((f) => f.type === 'result' && f.id === 5, 'save_skill result');
    assert.equal(saved.ok, true);
    assert.match(saved.data.draft, /\.draft\.md$/);
  } finally {
    b.child.kill();
    mock.server.close();
  }
});

test('serve: set_mode lands mid-turn while a permission dialog is open ("always allow")', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(
    join(home, 'models.json'),
    JSON.stringify([
      { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY' }
    ])
  );
  const rm = (id: string, f: string) =>
    SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id, function: { name: 'bash', arguments: JSON.stringify({ command: `rm -f ${f}` }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
  let call = 0;
  mock.setScript(() => {
    call++;
    if (call === 1) return rm('c1', 'a.txt');
    if (call === 2) return rm('c2', 'b.txt');
    return SSE({ choices: [{ delta: { content: 'done' }, finish_reason: 'stop' }] }) + 'data: [DONE]\n\n';
  });

  const s = await startServe(home, work, 0);
  try {
    await s.next(); // hello
    s.send({ id: 1, cmd: 'prompt', text: '删两个文件' });
    const req = await s.until((f) => f.type === 'permission_request', 'permission_request');
    // the TUI's "a": flip to auto first, then allow — the mode change must not queue behind the prompt
    s.send({ id: 2, cmd: 'set_mode', mode: 'auto' });
    const mode = await s.until((f) => f.type === 'result' && f.id === 2, 'set_mode result');
    assert.equal(mode.data.mode, 'auto');
    s.send({ cmd: 'permission', request: req.request, allow: true });
    // second rm runs without another dialog
    const seen: string[] = [];
    await s.until((f) => {
      seen.push(f.type);
      return f.type === 'result' && f.id === 1;
    }, 'prompt result');
    assert.equal(seen.filter((t) => t === 'permission_request').length, 0);
    assert.equal(seen.filter((t) => t === 'tool_result').length, 2);
  } finally {
    s.child.kill();
    mock.server.close();
  }
});
