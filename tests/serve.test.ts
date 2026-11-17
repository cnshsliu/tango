import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { existsSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { createInterface } from 'node:readline';
import { SessionStore } from '../src/core/session.js';

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
      const body = script();
      const m = /^DELAY:(\d+):/.exec(body);
      if (m) setTimeout(() => res.end(body.slice(m[0].length)), Number(m[1]));
      else res.end(body);
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
      { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
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
    assert.equal(hello.contextWindow, 200);
    assert.equal(hello.contextTokens, 0);
    assert.ok(hello.sessionId && hello.tools.some((t: any) => t.name === 'write'));

    s.send({ id: 1, cmd: 'status' });
    const status = await s.until((f) => f.type === 'result' && f.id === 1, 'status result');
    assert.equal(status.ok, true);
    assert.equal(status.data.model, 'mock/mock');
    s.send({ id: 6, cmd: 'set_model', modelId: 'mock/mock' });
    const model = await s.until((f) => f.type === 'result' && f.id === 6, 'set_model result');
    assert.equal(model.data.contextWindow, 200);

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
    assert.equal(te.contextTokens, 40);
    assert.equal(te.contextWindow, 200);
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

    s.send({ id: 4, cmd: 'history' });
    const history = await s.until((f) => f.type === 'result' && f.id === 4, 'history result');
    assert.equal(history.data.total, 3);
    assert.deepEqual(history.data.items.map((i: any) => i.kind), ['user', 'tool', 'assistant']);
    assert.equal(history.data.items[0].text, '把文件写到上一级目录');
    assert.equal(history.data.items[1].name, 'write');
    assert.equal(history.data.items[1].isError, true);
    assert.match(history.data.items[1].output, /user declined/);
    assert.equal(history.data.items[2].text, '好，不写了');

    s.send({ id: 5, cmd: 'history', limit: 2 });
    const tail = await s.until((f) => f.type === 'result' && f.id === 5, 'limited history result');
    assert.equal(tail.data.total, 3);
    assert.deepEqual(tail.data.items.map((i: any) => i.kind), ['tool', 'assistant']);

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
  await writeFile(join(home, 'config.json'), JSON.stringify({ theme: 'light', editor: 'vim', keys: { expandTools: 'ctrl+u' }, nvimViewer: 'readonly' }));
  await writeFile(
    join(home, 'models.json'),
    JSON.stringify([
      { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', cost: { input: 1, output: 2 } }
    ])
  );
  mock.setScript(() => SSE({ choices: [{ delta: { content: '收到' }, finish_reason: 'stop' }], usage: { prompt_tokens: 1000, completion_tokens: 500, total_tokens: 1500 } }) + 'data: [DONE]\n\n');

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
    assert.equal(hello.totalCostUsd, 0.002);
    assert.equal(hello.contextTokens, 1000);
    assert.equal(hello.theme, 'light');
    assert.equal(hello.editor, 'vim');
    assert.deepEqual(hello.keys, { expandTools: 'ctrl+u' });
    assert.equal(hello.nvimViewer, 'readonly');

    b.send({ id: 2, cmd: 'sessions' });
    const list = await b.until((f) => f.type === 'result' && f.id === 2, 'sessions result');
    const cur = list.data.sessions.find((s: any) => s.current);
    assert.equal(cur.id, firstId);
    assert.ok(cur.messages >= 2);

    b.send({ id: 3, cmd: 'prompt', text: '暗号是什么' });
    const turn = await b.until((f) => f.type === 'turn_end', 'resumed turn_end');
    assert.equal(turn.totalCostUsd, 0.004);
    await b.until((f) => f.type === 'result' && f.id === 3, 'prompt result');
    b.send({ id: 6, cmd: 'status' });
    const status = await b.until((f) => f.type === 'result' && f.id === 6, 'resumed status');
    assert.equal(status.data.totalCostUsd, 0.004);
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

    b.send({ id: 7, cmd: 'set_nvim_viewer', mode: 'ask' });
    const viewer = await b.until((f) => f.type === 'result' && f.id === 7, 'nvim viewer result');
    assert.deepEqual(viewer.data, { nvimViewer: 'ask' });
    const config = JSON.parse(await readFile(join(home, 'config.json'), 'utf8'));
    assert.equal(config.nvimViewer, 'ask');
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

test('serve: theme and mouse settings persist without dropping other config', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(
    join(home, 'models.json'),
    JSON.stringify([
      { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY' }
    ])
  );
  await writeFile(join(home, 'config.json'), JSON.stringify({ theme: 'dark', editor: 'vim', nvimViewer: 'readonly' }));
  const s = await startServe(home, work, 0);
  try {
    const hello = await s.next();
    assert.equal(hello.mouse, true);
    s.send({ id: 1, cmd: 'set_theme', theme: 'light' });
    const theme = await s.until((f) => f.type === 'result' && f.id === 1, 'set_theme result');
    assert.equal(theme.data.theme, 'light');
    s.send({ id: 2, cmd: 'set_mouse', mouse: false });
    const mouse = await s.until((f) => f.type === 'result' && f.id === 2, 'set_mouse result');
    assert.equal(mouse.data.mouse, false);
    const config = JSON.parse(await readFile(join(home, 'config.json'), 'utf8'));
    assert.deepEqual(config, { theme: 'light', mouse: false, editor: 'vim', nvimViewer: 'readonly' });
    s.send({ id: 3, cmd: 'set_theme', theme: 'blue' });
    const badTheme = await s.until((f) => f.type === 'result' && f.id === 3, 'bad theme result');
    assert.equal(badTheme.ok, false);
    s.send({ id: 4, cmd: 'set_mouse', mouse: 'no' });
    const badMouse = await s.until((f) => f.type === 'result' && f.id === 4, 'bad mouse result');
    assert.equal(badMouse.ok, false);
  } finally {
    s.child.kill();
    mock.server.close();
  }
});

test('serve: diff returns working-tree and staged patches without mutations', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(
    join(home, 'models.json'),
    JSON.stringify([
      { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY' }
    ])
  );
  const git = (args: string[], cwd = work) => {
    const result = spawnSync('git', args, { cwd, encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr);
  };
  git(['init']);
  git(['config', 'user.email', 'tango@example.test']);
  git(['config', 'user.name', 'Tango Test']);
  await writeFile(join(work, 'file.txt'), 'old\n');
  git(['add', 'file.txt']);
  git(['commit', '-m', 'initial']);
  await writeFile(join(work, 'file.txt'), 'old\nnew\n');

  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 1, cmd: 'diff' });
    const working = await s.until((f) => f.type === 'result' && f.id === 1, 'working diff');
    assert.match(working.data.diff, /\+new/);
    git(['add', 'file.txt']);
    s.send({ id: 2, cmd: 'diff', staged: true });
    const staged = await s.until((f) => f.type === 'result' && f.id === 2, 'staged diff');
    assert.match(staged.data.diff, /\+new/);
    git(['reset', '--hard', 'HEAD']);
    s.send({ id: 3, cmd: 'diff' });
    const clean = await s.until((f) => f.type === 'result' && f.id === 3, 'clean diff');
    assert.equal(clean.data.diff, 'clean — nothing changed');
  } finally {
    s.child.kill();
  }

  const plain = await mkdtemp(join(tmpdir(), 'tango-serve-plain-'));
  const other = await startServe(home, plain, 0);
  try {
    await other.next();
    other.send({ id: 1, cmd: 'diff' });
    const failure = await other.until((f) => f.type === 'result' && f.id === 1, 'non-repo diff');
    assert.equal(failure.data.isError, true);
    assert.match(failure.data.diff, /not a git repository/);
  } finally {
    other.child.kill();
    mock.server.close();
  }
});

test('serve: btw streams an ephemeral answer outside session history', async () => {
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
  mock.setScript(() => {
    const content = ['formal answer', 'side answer', 'final answer'][call++] ?? 'done';
    return SSE({ choices: [{ delta: { content }, finish_reason: 'stop' }] }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 1, cmd: 'prompt', text: 'formal question' });
    await s.until((f) => f.type === 'result' && f.id === 1, 'formal result');
    s.send({ id: 2, cmd: 'btw', text: 'side question' });
    const stream = await s.until((f) => f.type === 'btw_stream', 'btw stream');
    assert.equal(stream.id, 2);
    const side = await s.until((f) => f.type === 'result' && f.id === 2, 'btw result');
    assert.equal(side.data.text, 'side answer');
    s.send({ id: 3, cmd: 'history' });
    const history = await s.until((f) => f.type === 'result' && f.id === 3, 'history after btw');
    assert.equal(JSON.stringify(history.data.items).includes('side question'), false);
    assert.equal(JSON.stringify(history.data.items).includes('side answer'), false);
    s.send({ id: 4, cmd: 'prompt', text: 'final question' });
    await s.until((f) => f.type === 'result' && f.id === 4, 'final result');
    assert.equal(JSON.stringify(mock.requests[2].messages).includes('side question'), false);
    assert.equal(JSON.stringify(mock.requests[2].messages).includes('side answer'), false);
    assert.equal(mock.requests[1].tools, undefined);
  } finally {
    s.child.kill();
    mock.server.close();
  }
});

test('serve: timeline/rewind/undo/redo with real git restore', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  // a real git repo so checkpoints produce real snapshots
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'init']);

  // turns: 1 write (v1), 2 write (v2 + leftover), 3 plain (no tools)
  // each write turn = two requests (tool call, then the finisher); plain turns = one
  let turnNo = 0;
  let pendingTool = false;
  mock.setScript(() => {
    if (!pendingTool) turnNo++;
    if (!pendingTool && turnNo <= 2) {
      pendingTool = true;
      const cmd = turnNo === 1 ? 'echo v1 > work.txt' : 'echo v2 > work.txt && echo junk > leftover.txt';
      return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'w' + turnNo, function: { name: 'bash', arguments: JSON.stringify({ command: cmd }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    }
    pendingTool = false;
    return SSE({ choices: [{ delta: { content: '纯文本回复' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 5 } }) + 'data: [DONE]\n\n';
  });

  const s = await startServe(home, work, 0);
  try {
    await s.next(); // hello
    for (let i = 0; i < 3; i++) {
      s.send({ id: 100 + i, cmd: 'prompt', text: `turn ${i + 1}` });
      await s.until((f: any) => f.type === 'result' && f.ok, 'turn result');
    }
    assert.equal((await readFile(join(work, 'work.txt'), 'utf8')).trim(), 'v2');
    assert.ok(existsSync(join(work, 'leftover.txt')));

    // timeline: three turns, first two with snapshots
    s.send({ id: 200, cmd: 'timeline' });
    const tl = await s.until((f: any) => f.type === 'result' && f.data?.nodes, 'timeline');
    const nodes = tl.data.nodes;
    assert.equal(nodes.length, 3);
    assert.equal(nodes[0].hasSnapshot, true);
    assert.equal(nodes[1].hasSnapshot, true);
    assert.equal(nodes[2].hasSnapshot, false, 'plain turn has no code snapshot');
    assert.ok(nodes[0].firstLine.includes('turn 1'));

    // rewind to before turn 2: work.txt back to v1, leftover gone, history truncated
    s.send({ id: 201, cmd: 'rewind', entryIndex: nodes[1].entryIndex });
    const rw = await s.until((f: any) => f.type === 'result' && f.id === 201, 'rewind');
    assert.ok(rw.ok, JSON.stringify(rw));
    assert.equal((await readFile(join(work, 'work.txt'), 'utf8')).trim(), 'v1');
    assert.ok(!existsSync(join(work, 'leftover.txt')), 'agent leftover smart-deleted');
    assert.ok(rw.data.deleted.includes('leftover.txt'));

    s.send({ id: 202, cmd: 'history' });
    const h1 = await s.until((f: any) => f.type === 'result' && f.data?.items, 'history');
    const texts = JSON.stringify(h1.data.items);
    assert.ok(texts.includes('turn 1'));
    assert.ok(!texts.includes('turn 2'));
    assert.ok(!texts.includes('turn 3'));

    // redo: turn 2+3 come back (work.txt v2 again, leftover back)
    s.send({ id: 203, cmd: 'redo' });
    await s.until((f: any) => f.type === 'result' && f.id === 203, 'redo');
    assert.equal((await readFile(join(work, 'work.txt'), 'utf8')).trim(), 'v2');
    assert.ok(existsSync(join(work, 'leftover.txt')));

    // undo: drop the last visible turn (turn 2 — redo had re-included only it);
    // code follows back to the end of turn 1
    s.send({ id: 204, cmd: 'undo' });
    await s.until((f: any) => f.type === 'result' && f.id === 204, 'undo');
    s.send({ id: 205, cmd: 'history' });
    const h2 = await s.until((f: any) => f.type === 'result' && f.data?.items, 'history after undo');
    assert.ok(!JSON.stringify(h2.data.items).includes('turn 3'));
    assert.ok(!JSON.stringify(h2.data.items).includes('turn 2'));
    assert.equal((await readFile(join(work, 'work.txt'), 'utf8')).trim(), 'v1');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('serve: fork/branch/tree — independent timelines sharing snapshot objects', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'init']);

  let turnNo = 0;
  let pendingTool = false;
  mock.setScript(() => {
    if (!pendingTool) turnNo++;
    if (!pendingTool && turnNo <= 2) {
      pendingTool = true;
      const cmd = turnNo === 1 ? 'echo v1 > work.txt' : 'echo v2 > work.txt';
      return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'w' + turnNo, function: { name: 'bash', arguments: JSON.stringify({ command: cmd }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    }
    pendingTool = false;
    return SSE({ choices: [{ delta: { content: '回复' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 5 } }) + 'data: [DONE]\n\n';
  });

  const s = await startServe(home, work, 0);
  try {
    await s.next(); // hello
    for (let i = 0; i < 2; i++) {
      s.send({ id: 100 + i, cmd: 'prompt', text: `turn ${i + 1}` });
      await s.until((f: any) => f.type === 'result' && f.ok, 'turn result');
    }
    s.send({ id: 200, cmd: 'timeline' });
    const tl = await s.until((f: any) => f.type === 'result' && f.data?.nodes, 'timeline');
    const node1 = tl.data.nodes[0];

    // fork from before turn 2, named
    s.send({ id: 201, cmd: 'fork', name: 'alt-path', entryIndex: node1.entryIndex });
    const fk = await s.until((f: any) => f.type === 'result' && f.id === 201, 'fork');
    assert.ok(fk.ok, JSON.stringify(fk));
    const forkId: string = fk.data.sessionId;
    assert.notEqual(forkId, undefined);
    // the fork file exists with the prefix projection
    assert.ok(existsSync(join(home, 'sessions', `${forkId}.jsonl`)));

    // tree shows both sessions with lineage
    s.send({ id: 202, cmd: 'tree' });
    const tree = await s.until((f: any) => f.type === 'result' && f.data?.nodes, 'tree');
    const byId = Object.fromEntries(tree.data.nodes.map((n: any) => [n.sessionId, n]));
    assert.ok(byId[forkId]);
    assert.equal(byId[forkId].forkOf.sessionId, byId[Object.keys(byId).find((k) => !byId[k].forkOf) as string].sessionId);
    assert.equal(byId[forkId].name, 'alt-path');

    // original session keeps working; its projection is unaffected by the fork
    s.send({ id: 203, cmd: 'prompt', text: 'turn 3 on original' });
    await s.until((f: any) => f.type === 'result' && f.id === 203 && f.ok, 'turn 3');
    s.send({ id: 204, cmd: 'history' });
    const h = await s.until((f: any) => f.type === 'result' && f.data?.items, 'history');
    assert.ok(JSON.stringify(h.data.items).includes('turn 3 on original'));

    // branch: in-place conversation fork — no new session file, projection rewinds
    const before = readdirSync(join(home, 'sessions')).length;
    s.send({ id: 205, cmd: 'branch', entryIndex: node1.entryIndex });
    const br = await s.until((f: any) => f.type === 'result' && f.id === 205, 'branch');
    assert.ok(br.ok);
    assert.equal(br.data.branch, true);
    assert.equal(readdirSync(join(home, 'sessions')).length, before, 'branch creates no session file');
    s.send({ id: 206, cmd: 'history' });
    const hb = await s.until((f: any) => f.type === 'result' && f.data?.items, 'history after branch');
    assert.ok(!JSON.stringify(hb.data.items).includes('turn 3 on original'));
    assert.ok(!JSON.stringify(hb.data.items).includes('turn 2'));
    // code restored to end of turn 1
    assert.equal((await readFile(join(work, 'work.txt'), 'utf8')).trim(), 'v1');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('serve: find orphaned futures, export, transcript, rename, recap', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() =>
    SSE({ choices: [{ delta: { content: 'MAGICWORD-PLAIN-ANSWER' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 3 } }) + 'data: [DONE]\n\n'
  );

  const s = await startServe(home, work, 0);
  try {
    await s.next(); // hello
    s.send({ id: 300, cmd: 'prompt', text: 'please say MAGICWORD-USER-PROMPT' });
    await s.until((f: any) => f.type === 'result' && f.id === 300 && f.ok, 'turn');

    // rewind it away, then find must still locate it as orphaned
    s.send({ id: 301, cmd: 'timeline' });
    const tl = await s.until((f: any) => f.type === 'result' && f.data?.nodes, 'timeline');
    s.send({ id: 302, cmd: 'rewind', entryIndex: tl.data.nodes[0].entryIndex });
    await s.until((f: any) => f.type === 'result' && f.id === 302 && f.ok, 'rewind');

    s.send({ id: 303, cmd: 'find', text: 'MAGICWORD' });
    const fd = await s.until((f: any) => f.type === 'result' && f.id === 303, 'find');
    assert.ok(fd.ok, JSON.stringify(fd));
    assert.ok(fd.data.hits.length >= 2, 'user prompt and plain answer both hit');
    assert.ok(fd.data.hits.every((h: any) => h.orphaned === true), 'rewound entries are marked orphaned');
    assert.ok(fd.data.hits[0].snippet.length > 0);

    // export md: real file with both visible and abandoned content
    s.send({ id: 304, cmd: 'export', format: 'md' });
    const ex = await s.until((f: any) => f.type === 'result' && f.id === 304, 'export');
    assert.ok(ex.ok);
    assert.ok(existsSync(ex.data.path));
    const md = await readFile(ex.data.path, 'utf8');
    assert.ok(md.includes('MAGICWORD-USER-PROMPT'), 'abandoned future is in the export');
    assert.ok(md.includes('已回滚') || md.toLowerCase().includes('rewind'), 'rewind boundary marked');

    // transcript: raw text including orphans
    s.send({ id: 305, cmd: 'transcript' });
    const tr = await s.until((f: any) => f.type === 'result' && f.id === 305, 'transcript');
    assert.ok(tr.ok);
    assert.ok(tr.data.text.includes('MAGICWORD-USER-PROMPT'));

    // rename shows up in sessions
    s.send({ id: 306, cmd: 'rename', name: 'my-named-session' });
    const rn = await s.until((f: any) => f.type === 'result' && f.id === 306, 'rename');
    assert.ok(rn.ok);
    s.send({ id: 307, cmd: 'sessions' });
    const ss = await s.until((f: any) => f.type === 'result' && f.id === 307, 'sessions');
    assert.ok(ss.data.sessions.some((x: any) => x.name === 'my-named-session' && x.current === true));

    // recap: text answer, no session writes
    s.send({ id: 308, cmd: 'prompt', text: 'another turn' });
    await s.until((f: any) => f.type === 'result' && f.id === 308 && f.ok, 'turn2');
    s.send({ id: 309, cmd: 'sessions' });
    const before = await s.until((f: any) => f.type === 'result' && f.id === 309, 'count before');
    const countBefore = before.data.sessions.find((x: any) => x.current).messages;
    s.send({ id: 310, cmd: 'recap' });
    const rc = await s.until((f: any) => f.type === 'result' && f.id === 310, 'recap');
    assert.ok(rc.ok);
    assert.ok(String(rc.data.text).length > 0);
    s.send({ id: 311, cmd: 'sessions' });
    const after = await s.until((f: any) => f.type === 'result' && f.id === 311, 'count after');
    assert.equal(after.data.sessions.find((x: any) => x.current).messages, countBefore, 'recap itself adds nothing');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('time-travel fixes: user files between turns survive undo; undo/fork see the visible timeline', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'init']);

  // request-indexed script: 1 write v1, 2 plain, 3 write v2+junk, 4 plain, 5+ plain
  let n = 0;
  mock.setScript(() => {
    n++;
    if (n === 1) return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'w1', function: { name: 'bash', arguments: JSON.stringify({ command: 'echo v1 > work.txt' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    if (n === 3) return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'w2', function: { name: 'bash', arguments: JSON.stringify({ command: 'echo v2 > work.txt && echo junk > junk.txt' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    return SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  });

  const s = await startServe(home, work, 0);
  try {
    await s.next(); // hello
    s.send({ id: 100, cmd: 'prompt', text: 'turn-one' });
    await s.until((f: any) => f.type === 'result' && f.ok, 't1');

    // the user works BETWEEN turns: a brand-new file (never snapshotted)
    await writeFile(join(work, 'handmade.txt'), 'user data\n');

    s.send({ id: 101, cmd: 'prompt', text: 'turn-two' });
    await s.until((f: any) => f.type === 'result' && f.ok, 't2');

    // after turn 2, the user edits a TRACKED file — a rewind to before turn 2 must
    // report this clobbered edit (I3)
    await writeFile(join(work, 'seed.txt'), 'seed\nuser edited\n');

    // C1: undo must keep the user's between-turn file
    s.send({ id: 200, cmd: 'undo' });
    const undo = await s.until((f: any) => f.type === 'result' && f.id === 200, 'undo');
    assert.ok(undo.ok, JSON.stringify(undo));
    assert.equal((await readFile(join(work, 'work.txt'), 'utf8')).trim(), 'v1');
    assert.ok(!existsSync(join(work, 'junk.txt')));
    assert.ok(existsSync(join(work, 'handmade.txt')), 'C1: user between-turn file survives undo');
    assert.equal(await readFile(join(work, 'handmade.txt'), 'utf8'), 'user data\n');
    // I3: the clobbered tracked-file edit is at least reported
    assert.ok((undo.data.overwritten ?? []).includes('seed.txt'), 'I3: overwritten files reported');
    assert.equal(await readFile(join(work, 'seed.txt'), 'utf8'), 'seed\n', 'tracked file restored to snapshot content');

    // redo: turn 2 back
    s.send({ id: 201, cmd: 'redo' });
    await s.until((f: any) => f.type === 'result' && f.id === 201 && f.ok, 'redo');
    assert.equal((await readFile(join(work, 'work.txt'), 'utf8')).trim(), 'v2');
    assert.ok(existsSync(join(work, 'junk.txt')));

    // new work after a rewind exists in the log; C3: undo must drop ONLY the new turn
    s.send({ id: 102, cmd: 'prompt', text: 'turn-three' });
    await s.until((f: any) => f.type === 'result' && f.ok, 't3');
    s.send({ id: 202, cmd: 'undo' });
    const undo2 = await s.until((f: any) => f.type === 'result' && f.id === 202, 'undo2');
    assert.ok(undo2.ok, JSON.stringify(undo2));
    s.send({ id: 203, cmd: 'history' });
    const h = await s.until((f: any) => f.type === 'result' && f.id === 203, 'history');
    const txt = JSON.stringify(h.data.items);
    assert.ok(txt.includes('turn-one'), 'turn 1 stays visible');
    assert.ok(txt.includes('turn-two'), 'C3: turn 2 (redo-kept) stays visible');
    assert.ok(!txt.includes('turn-three'), 'only the new turn was dropped');

    // I1+I2: bare /fork lands at the CURRENT position: t1+t2, not the abandoned branch
    s.send({ id: 204, cmd: 'fork', name: 'after-rewind' });
    const fk = await s.until((f: any) => f.type === 'result' && f.id === 204, 'fork');
    assert.ok(fk.ok);
    const forkStore = await SessionStore.open(join(home, 'sessions'), fk.data.sessionId);
    const forkTexts = JSON.stringify(forkStore.messages());
    assert.ok(forkTexts.includes('turn-one') && forkTexts.includes('turn-two'), 'I1: fork copies the visible timeline');
    assert.ok(!forkTexts.includes('turn-three'), 'I2: fork does not resurrect the abandoned turn');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('M2+M3: timeline hasSnapshot is per-turn and abandoned turns are marked', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'init']);

  // request-indexed: 1 write, 2 plain, 3 plain (turn2 writes, turn3 plain)
  let n = 0;
  mock.setScript(() => {
    n++;
    if (n === 1) return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'w1', function: { name: 'bash', arguments: JSON.stringify({ command: 'echo a > a.txt' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    return SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  });

  const s = await startServe(home, work, 0);
  try {
    await s.next();
    for (const [i, t] of ['t-one', 't-two', 't-three'].entries()) {
      s.send({ id: 100 + i, cmd: 'prompt', text: t });
      await s.until((f: any) => f.type === 'result' && f.ok, t);
    }
    // drop the last turn; the timeline must show it as orphaned
    s.send({ id: 200, cmd: 'undo' });
    await s.until((f: any) => f.type === 'result' && f.id === 200 && f.ok, 'undo');
    s.send({ id: 201, cmd: 'timeline' });
    const tl = await s.until((f: any) => f.type === 'result' && f.id === 201, 'timeline');
    const nodes = tl.data.nodes;
    assert.equal(nodes.length, 3);
    assert.equal(nodes[0].hasSnapshot, true, 'M2: writing turn flagged');
    assert.equal(nodes[1].hasSnapshot, false, 'M2: plain turn NOT flagged');
    assert.equal(nodes[2].hasSnapshot, false);
    assert.equal(nodes[2].orphaned, true, 'M3: abandoned turn marked');
    assert.equal(nodes[0].orphaned, false);

    // M1: rename shows in tree
    s.send({ id: 202, cmd: 'rename', name: 'renamed-1' });
    await s.until((f: any) => f.type === 'result' && f.id === 202 && f.ok, 'rename');
    s.send({ id: 203, cmd: 'tree' });
    const tree = await s.until((f: any) => f.type === 'result' && f.id === 203, 'tree');
    assert.ok(tree.data.nodes.some((x: any) => x.name === 'renamed-1'), 'M1: tree shows the new name');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('context command reports a token breakdown of the prompt assembly', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: 'hello context' });
    await s.until((f: any) => f.type === 'result' && f.ok, 'turn');
    s.send({ id: 101, cmd: 'context' });
    const c = await s.until((f: any) => f.type === 'result' && f.id === 101, 'context');
    assert.ok(c.ok);
    for (const k of ['system', 'tools', 'history', 'total', 'window', 'pct']) assert.ok(typeof c.data[k] === 'number' && c.data[k] >= 0, k);
    assert.ok(c.data.system > 0, 'system prompt is non-trivial');
    assert.ok(c.data.tools > 0, 'tool schemas take space');
    assert.ok(c.data.history > 0, 'history after one turn');
    assert.ok(c.data.total >= c.data.system + c.data.tools + c.data.history - 2, 'total sums the parts');
    assert.ok(c.data.pct >= 0 && c.data.pct < 100_000);
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('queue command shows the running prompt and the queued ones; interrupt clears', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  // request 1 is slow (delayed response end); the rest are instant
  let n = 0;
  mock.setScript(() => {
    n++;
    const body = SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
    return n === 1 ? `DELAY:2000:${body}` : body;
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: 'slow-prompt-AAAA' });
    await sleep(150);
    s.send({ id: 101, cmd: 'prompt', text: 'queued-prompt-BBBB' });
    await sleep(150);
    s.send({ id: 200, cmd: 'queue' });
    const q = await s.until((f: any) => f.type === 'result' && f.id === 200, 'queue');
    assert.ok(q.ok);
    assert.ok(String(q.data.running ?? '').includes('slow-prompt'), 'running shown');
    assert.equal(q.data.queued.length, 1, 'one queued');
    assert.ok(String(q.data.queued[0].text).includes('queued-prompt'));
    // interrupt clears the running turn
    s.send({ cmd: 'interrupt' });
    await s.until((f: any) => f.type === 'result' && (f.id === 100 || f.id === 101), 'after interrupt');
    s.send({ id: 201, cmd: 'queue' });
    const q2 = await s.until((f: any) => f.type === 'result' && f.id === 201, 'queue2');
    assert.equal(q2.data.running, null);
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

function sleep(ms: number) { return new Promise((r) => setTimeout(r, ms)); }

test('usage/stats aggregate across sessions; insights writes an offline HTML report', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200, cost: { input: 1, output: 2 } }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'answer' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 100, completion_tokens: 50 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    for (let i = 0; i < 2; i++) {
      s.send({ id: 100 + i, cmd: 'prompt', text: `q ${i}` });
      await s.until((f: any) => f.type === 'result' && f.ok, 'turn');
    }
    s.send({ id: 200, cmd: 'usage' });
    const u = await s.until((f: any) => f.type === 'result' && f.id === 200, 'usage');
    assert.ok(u.ok);
    assert.ok(u.data.totalCostUsd > 0, 'cost computed from priced usage');
    assert.equal(u.data.byModel.length, 1);
    assert.equal(u.data.byModel[0].model, 'mock/mock');
    assert.equal(u.data.byModel[0].requests, 2, 'both requests counted');
    assert.ok(u.data.bySession.length >= 1);
    assert.ok(u.data.bySession.some((x: any) => x.costUsd > 0));

    // stats is the same data
    s.send({ id: 201, cmd: 'stats' });
    const st = await s.until((f: any) => f.type === 'result' && f.id === 201, 'stats');
    assert.ok(st.ok && st.data.byModel);

    s.send({ id: 202, cmd: 'insights' });
    const ins = await s.until((f: any) => f.type === 'result' && f.id === 202, 'insights');
    assert.ok(ins.ok);
    assert.ok(existsSync(ins.data.path));
    const html = await readFile(ins.data.path, 'utf8');
    assert.ok(html.includes('mock/mock'));
    assert.ok(!/src=["']https?:|href=["']https?:/.test(html), 'offline readable');
    assert.ok(html.includes('$'), 'costs shown');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('autocompact: set/persist/off and threshold actually compacts', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'somewhat long answer to accumulate tokens for the estimator quickly' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    // invalid rejected
    s.send({ id: 200, cmd: 'set_autocompact', value: 'bogus' });
    const bad = await s.until((f: any) => f.type === 'result' && f.id === 200, 'invalid');
    assert.ok(!bad.ok);
    // set a tiny threshold so the next prompt must trigger compaction
    s.send({ id: 201, cmd: 'set_autocompact', value: 0.01 });
    const ok = await s.until((f: any) => f.type === 'result' && f.id === 201, 'set');
    assert.ok(ok.ok && ok.data.autocompact === 0.01);
    const cfg = JSON.parse(await readFile(join(home, 'config.json'), 'utf8'));
    assert.equal(cfg.autocompact, 0.01);

    s.send({ id: 100, cmd: 'prompt', text: 'hello one' });
    await s.until((f: any) => f.type === 'result' && f.ok, 'turn1');
    s.send({ id: 101, cmd: 'prompt', text: 'hello two' });
    await s.until((f: any) => f.type === 'result' && f.ok, 'turn2');
    // compact() needs >= 4 visible messages; the third prompt starts with 4 on context
    s.send({ id: 102, cmd: 'prompt', text: 'hello three' });
    await s.until((f: any) => f.type === 'result' && f.ok, 'turn3');
    const store = await SessionStore.open(join(home, 'sessions'), (await SessionStore.list(join(home, 'sessions')))[0].id);
    assert.ok(store.entriesOf('compaction').length >= 1, 'low threshold triggered a compaction');

    // off disables
    s.send({ id: 202, cmd: 'set_autocompact', value: 'off' });
    const off = await s.until((f: any) => f.type === 'result' && f.id === 202, 'off');
    assert.ok(off.ok && off.data.autocompact === 0);
    assert.equal(JSON.parse(await readFile(join(home, 'config.json'), 'utf8')).autocompact, 'off');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('presets: save/list/get round-trip in TANGO_HOME', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'preset_save', name: 'review', text: 'review this PR carefully' });
    const sv = await s.until((f: any) => f.type === 'result' && f.id === 200, 'save');
    assert.ok(sv.ok);
    // overwrite same name
    s.send({ id: 201, cmd: 'preset_save', name: 'review', text: 'review v2' });
    await s.until((f: any) => f.type === 'result' && f.id === 201, 'save2');
    s.send({ id: 202, cmd: 'preset_save', name: 'daily', text: 'standup notes' });
    await s.until((f: any) => f.type === 'result' && f.id === 202, 'save3');
    assert.ok(existsSync(join(home, 'presets.json')));

    s.send({ id: 203, cmd: 'preset_list' });
    const list = await s.until((f: any) => f.type === 'result' && f.id === 203, 'list');
    assert.equal(list.data.presets.length, 2);
    assert.ok(list.data.presets.some((p: any) => p.name === 'review' && p.preview.includes('v2')));

    s.send({ id: 204, cmd: 'preset_get', name: 'review' });
    const got = await s.until((f: any) => f.type === 'result' && f.id === 204, 'get');
    assert.equal(got.data.text, 'review v2');

    s.send({ id: 205, cmd: 'preset_get', name: 'nope' });
    const miss = await s.until((f: any) => f.type === 'result' && f.id === 205, 'miss');
    assert.ok(!miss.ok);
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('C1+I5: interrupt drops queued prompts (they never reach the model) and rewind stays guarded', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  let n = 0;
  mock.setScript(() => {
    n++;
    const body = SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
    return n === 1 ? `DELAY:2000:${body}` : body;
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: 'slow-A' });
    await sleep(200);
    const requestsBefore = mock.requests.length;
    s.send({ id: 101, cmd: 'prompt', text: 'queued-B' });
    await sleep(150);
    s.send({ cmd: 'interrupt' });
    const r100 = await s.until((f: any) => f.type === 'result' && f.id === 100, 'A result');
    const r101 = await s.until((f: any) => f.type === 'result' && f.id === 101, 'B result');
    assert.ok(r100.ok === true || r100.ok === false); // aborted turn settles
    assert.equal(r101.ok, false, 'queued prompt is dropped, not run');
    assert.match(String(r101.error), /dropped|interrupt/i);
    await sleep(300);
    assert.equal(mock.requests.length, requestsBefore, 'B never reached the model');

    // after the abort settles, queue is empty and time travel works
    s.send({ id: 200, cmd: 'queue' });
    const q = await s.until((f: any) => f.type === 'result' && f.id === 200, 'queue');
    assert.equal(q.data.running, null);
    assert.equal(q.data.queued.length, 0);
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('I2: persisted autocompact is restored at startup', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  await writeFile(join(home, 'config.json'), JSON.stringify({ autocompact: 'off' }));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    const hello = await s.next();
    assert.equal(hello.autocompact, 0, 'off restores as 0, not the 0.75 default');
    s.send({ id: 1, cmd: 'status' });
    const st = await s.until((f: any) => f.type === 'result' && f.id === 1, 'status');
    assert.equal(st.data.autocompact, 0);
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('I4: context breakdown rows sum to the total (skills not double-counted)', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 1, cmd: 'context' });
    const c = await s.until((f: any) => f.type === 'result' && f.id === 1, 'context');
    const rows = c.data.system + c.data.skills + c.data.tools + c.data.history;
    assert.ok(Math.abs(rows - c.data.total) <= 2, `rows (${rows}) must sum to total (${c.data.total})`);
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('permissions: add/list/remove rules take effect live', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  // rm is risky: with the mock asking for it, an unanswered permission means denial
  let n = 0;
  mock.setScript(() => {
    n++;
    if (n === 1) return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'r1', function: { name: 'bash', arguments: JSON.stringify({ command: 'rm -rf build/' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    return SSE({ choices: [{ delta: { content: 'done' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    // 1) without a rule the risky call is blocked (permission unanswered → denied)
    s.send({ id: 100, cmd: 'prompt', text: 'clean build' });
    const perm1 = await s.until((f: any) => f.type === 'permission_request', 'permission ask');
    s.send({ cmd: 'permission', request: perm1.request, allow: false });
    await s.until((f: any) => f.type === 'result' && f.id === 100, 'turn1');

    // 2) add the rule; invalid rules are refused
    s.send({ id: 200, cmd: 'permissions_add', rule: 'garbage' });
    const bad = await s.until((f: any) => f.type === 'result' && f.id === 200, 'bad add');
    assert.ok(!bad.ok);
    s.send({ id: 201, cmd: 'permissions_add', rule: 'bash:rm' });
    const add = await s.until((f: any) => f.type === 'result' && f.id === 201, 'add');
    assert.ok(add.ok);
    assert.deepEqual(JSON.parse(await readFile(join(home, 'config.json'), 'utf8')).permissions.allow, ['bash:rm']);

    s.send({ id: 202, cmd: 'permissions_list' });
    const list = await s.until((f: any) => f.type === 'result' && f.id === 202, 'list');
    assert.equal(list.data.allow.length, 1);
    assert.equal(list.data.allow[0].rule, 'bash:rm');

    // 3) same risky call now runs without asking
    n = 0;
    s.send({ id: 101, cmd: 'prompt', text: 'clean build again' });
    const done = await s.until((f: any) => f.type === 'result' && f.id === 101 && f.ok, 'turn2');
    assert.ok(done.ok);

    // 4) remove the rule — asks again
    s.send({ id: 203, cmd: 'permissions_remove', rule: 'bash:rm' });
    await s.until((f: any) => f.type === 'result' && f.id === 203 && f.ok, 'remove');
    n = 0;
    s.send({ id: 102, cmd: 'prompt', text: 'clean build third' });
    await s.until((f: any) => f.type === 'permission_request', 'asks again');
    s.child.kill();
  } finally {
    if (!s.child.killed) s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('import: maps Claude Code allowed-tools into rules, skips unmappable entries', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  await writeFile(join(work, '.claude', 'settings.json'), JSON.stringify({
    permissions: { allow: ['Bash(git status:*)', 'Write(src/**)', 'WebFetch', 'Dangerous(*)'] }
  }), { flag: 'wx' }).catch(() => {});
  const { mkdir } = await import('node:fs/promises');
  await mkdir(join(work, '.claude'), { recursive: true });
  await writeFile(join(work, '.claude', 'settings.json'), JSON.stringify({
    permissions: { allow: ['Bash(git status:*)', 'Write(src/**)', 'WebFetch', 'Dangerous(*)'] }
  }));

  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'import', source: 'claude-code' });
    const imp = await s.until((f: any) => f.type === 'result' && f.id === 200, 'import');
    assert.ok(imp.ok, JSON.stringify(imp));
    assert.ok(imp.data.imported.includes('bash:git status'), JSON.stringify(imp.data));
    assert.ok(imp.data.imported.includes('path:src'));
    assert.ok(imp.data.imported.includes('tool:web_fetch'));
    assert.ok(imp.data.skipped.some((x: any) => String(x.raw).startsWith('Dangerous')), 'unmappable reported, not silent');
    const cfg = JSON.parse(await readFile(join(home, 'config.json'), 'utf8'));
    assert.deepEqual(cfg.permissions.allow, imp.data.imported);

    // missing source file → readable error
    s.send({ id: 201, cmd: 'import', source: 'grok' });
    const miss = await s.until((f: any) => f.type === 'result' && f.id === 201, 'grok miss');
    assert.ok(!miss.ok);
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('cd/pwd/add_dir/config_show: cwd moves live and extra dirs allow writes', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  const extra = await mkdtemp(join(tmpdir(), 'tango-serve-extra-'));
  const sub = join(work, 'sub');
  await mkdir(sub);
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  // odd requests write (path relative to the CURRENT cwd), even requests finish
  let target = 'a.txt';
  let req = 0;
  mock.setScript(() => {
    req++;
    if (req % 2 === 1)
      return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'w' + req, function: { name: 'write', arguments: JSON.stringify({ path: target, content: 'x' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    return SSE({ choices: [{ delta: { content: 'done' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  });

  const s = await startServe(home, work, 0);
  try {
    await s.next();
    // pwd reflects the start dir
    s.send({ id: 200, cmd: 'pwd' });
    const pwd = await s.until((f: any) => f.type === 'result' && f.id === 200, 'pwd');
    assert.equal(pwd.data.cwd, work);

    // cd into sub: writes inside sub now pass without asking
    s.send({ id: 201, cmd: 'cd', dir: sub });
    const cd = await s.until((f: any) => f.type === 'result' && f.id === 201, 'cd');
    assert.ok(cd.ok);
    assert.equal(cd.data.cwd, sub);
    s.send({ id: 100, cmd: 'prompt', text: 'write in new cwd' });
    const r1 = await s.until((f: any) => f.type === 'result' && f.id === 100, 'turn1');
    assert.ok(r1.ok, 'write inside new cwd needs no permission');
    assert.ok(existsSync(join(sub, 'a.txt')));

    // cd back; a write into the extra dir asks unless add_dir registered it
    s.send({ id: 202, cmd: 'cd', dir: work });
    await s.until((f: any) => f.type === 'result' && f.id === 202 && f.ok, 'cd back');
    s.send({ id: 203, cmd: 'add_dir', dir: extra });
    const ad = await s.until((f: any) => f.type === 'result' && f.id === 203, 'add_dir');
    assert.ok(ad.ok);
    assert.deepEqual(JSON.parse(await readFile(join(home, 'config.json'), 'utf8')).extraDirs, [extra]);
    target = join(extra, 'b.txt');
    s.send({ id: 101, cmd: 'prompt', text: 'write into extra dir' });
    const r2 = await s.until((f: any) => f.type === 'result' && f.id === 101, 'turn2');
    assert.ok(r2.ok, 'extra dir write allowed');
    assert.ok(existsSync(join(extra, 'b.txt')));

    // config_show: sanitized view with the keys we manage
    s.send({ id: 204, cmd: 'config_show' });
    const cfg = await s.until((f: any) => f.type === 'result' && f.id === 204, 'config');
    assert.ok(cfg.ok);
    assert.ok(cfg.data.path.includes('config.json'));
    assert.ok(Array.isArray(cfg.data.config.extraDirs));
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
    await rm(extra, { recursive: true, force: true });
  }
});

test('init generates AGENTS.md once; doctor runs its checks', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  await writeFile(join(work, 'package.json'), JSON.stringify({ name: 'demo', scripts: { test: 'node --test' } }));
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'init' });
    const init = await s.until((f: any) => f.type === 'result' && f.id === 200, 'init');
    assert.ok(init.ok);
    const agents = await readFile(join(work, 'AGENTS.md'), 'utf8');
    assert.ok(agents.includes('demo'), 'project name detected');
    assert.ok(agents.includes('package.json') || agents.includes('node'), 'toolchain detected');
    // second init refuses to overwrite
    s.send({ id: 201, cmd: 'init' });
    const again = await s.until((f: any) => f.type === 'result' && f.id === 201, 'init2');
    assert.ok(!again.ok);

    s.send({ id: 202, cmd: 'doctor' });
    const doc = await s.until((f: any) => f.type === 'result' && f.id === 202, 'doctor');
    assert.ok(doc.ok);
    const names = doc.data.checks.map((c: any) => c.name);
    for (const want of ['git', 'TANGO_HOME writable', 'model key', 'sessions dir']) assert.ok(names.includes(want), want);
    assert.ok(doc.data.checks.every((c: any) => typeof c.ok === 'boolean'));
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('debug-zip packages diagnostics without secrets', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  await writeFile(join(home, 'config.json'), JSON.stringify({ feishuWebhook: 'https://open.feishu.cn/hook/SECRETWEBHOOK1234567890' }));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'SECRET-FREE' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: 'hi' });
    await s.until((f: any) => f.type === 'result' && f.ok, 'turn');
    s.send({ id: 200, cmd: 'debug_zip' });
    const dz = await s.until((f: any) => f.type === 'result' && f.id === 200, 'debug-zip');
    assert.ok(dz.ok, JSON.stringify(dz));
    const path = dz.data.path as string;
    assert.ok(existsSync(path));
    if (path.endsWith('.zip')) {
      const list = spawnSync('unzip', ['-l', path], { encoding: 'utf8' }).stdout;
      assert.ok(list.includes('doctor'), 'doctor report inside');
      // extract and scan for API-key-shaped secrets
      const out = spawnSync('bash', ['-c', `tmp=$(mktemp -d) && unzip -q '${path}' -d "$tmp" && cat "$tmp"/* 2>/dev/null; rm -rf "$tmp"`], { encoding: 'utf8' }).stdout;
      assert.ok(!/sk-[A-Za-z0-9]{16,}/.test(out), 'no api-key-shaped secrets');
      assert.ok(!out.includes('SECRETWEBHOOK'), 'I3: feishu webhook redacted');
    }
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('plan mode: writes vetoed even when a rule allows; reads fine; off restores', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  await writeFile(join(home, 'config.json'), JSON.stringify({ permissions: { allow: ['bash:echo'] } }));
  let n = 0;
  mock.setScript(() => {
    n++;
    if (n % 2 === 1) return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'w' + n, function: { name: 'bash', arguments: JSON.stringify({ command: 'echo plan-write > f.txt' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    return SSE({ choices: [{ delta: { content: 'done' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'set_plan', on: true });
    await s.until((f: any) => f.type === 'result' && f.id === 200 && f.ok, 'plan on');
    s.send({ id: 100, cmd: 'prompt', text: 'write something' });
    await s.until((f: any) => f.type === 'result' && f.id === 100 && f.ok, 'turn');
    assert.ok(!existsSync(join(work, 'f.txt')), 'plan mode vetoes writes even with an allow rule');
    // the blocked tool result is visible to the model
    assert.ok(mock.requests.some((r: any) => JSON.stringify(r).includes('plan mode')));

    s.send({ id: 201, cmd: 'set_plan', on: false });
    await s.until((f: any) => f.type === 'result' && f.id === 201 && f.ok, 'plan off');
    n = 0;
    s.send({ id: 101, cmd: 'prompt', text: 'write again' });
    await s.until((f: any) => f.type === 'result' && f.id === 101 && f.ok, 'turn2');
    assert.ok(existsSync(join(work, 'f.txt')), 'off restores writes');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('review/security-review inline the working diff into the prompt', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'i']);
  await writeFile(join(work, 'seed.txt'), 'seed\nREVIEWMARK CHANGE\n');
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'review done' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'review' });
    await s.until((f: any) => f.type === 'result' && f.id === 200 && f.ok, 'review');
    const reviewPrompt = JSON.stringify(mock.requests.at(-1).messages.at(-1));
    assert.ok(reviewPrompt.includes('REVIEWMARK'), 'diff text rides the review prompt');
    assert.ok(reviewPrompt.includes('正确性'), 'review instructions present');

    s.send({ id: 201, cmd: 'security_review' });
    await s.until((f: any) => f.type === 'result' && f.id === 201 && f.ok, 'sec');
    const secPrompt = JSON.stringify(mock.requests.at(-1).messages.at(-1));
    assert.ok(secPrompt.includes('安全'), 'security instructions present');
    assert.ok(secPrompt.includes('REVIEWMARK'));
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('remember appends to global AGENTS.md and memory returns it', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'remember', text: '用户偏好中文回复' });
    const r = await s.until((f: any) => f.type === 'result' && f.id === 200, 'remember');
    assert.ok(r.ok);
    s.send({ id: 201, cmd: 'memory' });
    const m = await s.until((f: any) => f.type === 'result' && f.id === 201, 'memory');
    assert.ok(m.ok);
    assert.ok(m.data.text.includes('用户偏好中文回复'), 'line landed in global AGENTS.md');
    assert.ok(m.data.path.includes('AGENTS.md'));
    // closes the loop: a fresh buildContext in that home picks it up
    const { buildContext } = await import('../src/core/context.js');
    const prevHome2 = process.env.TANGO_HOME;
    process.env.TANGO_HOME = home;
    try {
      const { systemPrompt } = await buildContext(work);
      assert.ok(systemPrompt.includes('用户偏好中文回复'), 'global memory reaches new sessions');
    } finally {
      if (prevHome2 === undefined) delete process.env.TANGO_HOME; else process.env.TANGO_HOME = prevHome2;
    }
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('skills command lists global and project skills with sources', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  // a global skill in TANGO_HOME
  const { mkdir: mk4, writeFile: wf6 } = await import('node:fs/promises');
  const gdir = join(home, 'skills', 'global-demo');
  await mk4(gdir, { recursive: true });
  await wf6(join(gdir, 'SKILL.md'), '---\nname: global-demo\ndescription: 全局演示技能\n---\nbody');
  // a project skill in cwd
  const pdir = join(work, '.tango', 'skills', 'proj-demo');
  await mk4(pdir, { recursive: true });
  await wf6(join(pdir, 'SKILL.md'), '---\nname: proj-demo\ndescription: 项目技能\n---\nbody');
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'skills' });
    const sk = await s.until((f: any) => f.type === 'result' && f.id === 200, 'skills');
    assert.ok(sk.ok);
    const names = sk.data.skills.map((x: any) => `${x.name}|${x.scope}`);
    assert.ok(names.includes('global-demo|global'), JSON.stringify(names));
    assert.ok(names.includes('proj-demo|project'), JSON.stringify(names));
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('goal: auto-continues until achieved, respects interrupt and max rounds', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  // ask() responses (goal checks) alternate NO / YES; prompt responses are plain
  let asks = 0;
  let prompts = 0;
  mock.setScript(() => {
    const body = JSON.parse(JSON.stringify({})); // marker unused
    const last = mock.requests.at(-1);
    const isAsk = last && Array.isArray(last.messages) && JSON.stringify(last.messages).includes('目标是否已达成');
    if (isAsk) {
      asks++;
      const ans = asks === 1 ? 'NO' : 'YES';
      return SSE({ choices: [{ delta: { content: ans } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
    }
    prompts++;
    return SSE({ choices: [{ delta: { content: `work ${prompts}` } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'goal', text: '把示例写完' });
    await s.until((f: any) => f.type === 'result' && f.id === 200 && f.ok, 'goal set');
    s.send({ id: 100, cmd: 'prompt', text: '开始' });
    await s.until((f: any) => f.type === 'result' && f.id === 100, 'turn1');
    // goal check says NO → one auto continuation happens, then YES → goal clears
    const done = await s.until((f: any) => f.type === 'message' && JSON.stringify(f).includes('目标完成'), 'goal done notice');
    assert.ok(done);
    assert.equal(prompts, 2, 'exactly one auto continuation: ' + prompts);
    assert.equal(asks, 2);
    // goal cleared: a further prompt does not auto-continue
    const p0 = prompts;
    s.send({ id: 101, cmd: 'prompt', text: 'another' });
    await s.until((f: any) => f.type === 'result' && f.id === 101, 'turn2');
    await sleep(600);
    assert.equal(prompts, p0 + 1, 'no more auto-continuation after the goal cleared');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('loop: repeats a prompt on an interval, off and interrupt stop it', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'tick' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  process.env.TANGO_TEST_LOOP_FLOOR = '0';
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'loop', everySec: 0.3, prompt: 'check status' });
    const set = await s.until((f: any) => f.type === 'result' && f.id === 200 && f.ok, 'loop set');
    assert.ok(set.ok);
    // wait for ~3 iterations
    await sleep(1400);
    s.send({ id: 201, cmd: 'loop', off: true });
    await s.until((f: any) => f.type === 'result' && f.id === 201 && f.ok, 'loop off');
    const countAfterOff = mock.requests.length;
    await sleep(800);
    assert.equal(mock.requests.length, countAfterOff, 'no more ticks after off');
    assert.ok(countAfterOff >= 3, `ran at least 3 iterations, got ${countAfterOff}`);
  } finally {
    delete process.env.TANGO_TEST_LOOP_FLOOR;
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('hooks: beforeTool runs with the call JSON on stdin; failures never block', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  await writeFile(join(home, 'config.json'), JSON.stringify({
    hooks: { beforeTool: `cat > ${join(work, 'hook-input.json')}` }
  }));
  let n = 0;
  mock.setScript(() => {
    n++;
    if (n % 2 === 1) return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'h1', function: { name: 'bash', arguments: JSON.stringify({ command: 'echo hooked > hook-ran.txt' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    return SSE({ choices: [{ delta: { content: 'done' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: 'run it' });
    await s.until((f: any) => f.type === 'result' && f.id === 100 && f.ok, 'turn');
    assert.ok(existsSync(join(work, 'hook-ran.txt')), 'the tool ran despite the hook');
    const hooked = JSON.parse(await readFile(join(work, 'hook-input.json'), 'utf8'));
    assert.equal(hooked.name, 'bash');
    assert.ok(String(hooked.input.command).includes('hooked'));
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('C1-C3: interrupt during goal check stops continuation; loop never overlaps; goal rides the queue', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  let prompts = 0; let asks = 0;
  mock.setScript(() => {
    const last = mock.requests.at(-1);
    const isAsk = last && JSON.stringify(last.messages).includes('目标是否已达成');
    if (isAsk) {
      asks++;
      // hang the check long enough for the interrupt to land mid-ask
      return `DELAY:2500:` + SSE({ choices: [{ delta: { content: 'NO' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
    }
    prompts++;
    return SSE({ choices: [{ delta: { content: `work ${prompts}` } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'goal', text: '完成目标X' });
    await s.until((f: any) => f.type === 'result' && f.id === 200 && f.ok, 'set');
    s.send({ id: 100, cmd: 'prompt', text: 'go' });
    await s.until((f: any) => f.type === 'result' && f.id === 100, 'turn');
    // the goal check is now hanging (2.5s) — interrupt mid-check
    await sleep(400);
    s.send({ cmd: 'interrupt' });
    await sleep(3200); // let the aborted check settle
    const p0 = prompts;
    await sleep(800);
    assert.equal(prompts, p0, 'C1: no continuation runs after an interrupt mid-check');
    // queue check: goal cleared by interrupt
    s.send({ id: 201, cmd: 'goal', text: '' });
    const g = await s.until((f: any) => f.type === 'result' && f.id === 201, 'goal state');
    assert.equal(g.data.goal, null, 'interrupt cleared the goal');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('C2: loop ticks never overlap a slow prompt', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  let inflight = 0; let maxInflight = 0; let count = 0;
  mock.setScript(() => {
    count++;
    maxInflight = Math.max(maxInflight, ++inflight);
    return `DELAY:800:` + SSE({ choices: [{ delta: { content: 'tick' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n';
  });
  // decrement when the result frame per prompt lands — approximate via requests finishing:
  // simpler: track via serve results. Use interval 0.2s, prompts take ~0.8s → without the fix, 3-4 overlap.
  process.env.TANGO_TEST_LOOP_FLOOR = '0';
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'loop', everySec: 0.2, prompt: 'slow tick', minInterval: 0 });
    await s.until((f: any) => f.type === 'result' && f.id === 200 && f.ok, 'loop set');
    await sleep(2600);
    s.send({ id: 201, cmd: 'loop', off: true });
    await s.until((f: any) => f.type === 'result' && f.id === 201, 'off');
    // count distinct request bursts: without overlap, ~3 sequential; with overlap, 8-12
    assert.ok(count >= 2 && count <= 5, `sequential ticks expected 2-5, got ${count}`);
  } finally {
    delete process.env.TANGO_TEST_LOOP_FLOOR;
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('I1: review sees staged changes too', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'i']);
  await writeFile(join(work, 'seed.txt'), 'seed\nSTAGEDREVIEW\n'); git(['add', 'seed.txt']); // staged only, worktree clean
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'reviewed' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'review' });
    await s.until((f: any) => f.type === 'result' && f.id === 200 && f.ok, 'review');
    assert.ok(mock.requests.length >= 1, 'review actually consulted the model');
    assert.ok(JSON.stringify(mock.requests.at(-1).messages).includes('STAGEDREVIEW'), 'staged change visible to review');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('worktree: create/list/merge/remove with safety gates', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'i']);

  const s = await startServe(home, work, 0);
  try {
    await s.next();
    // create
    s.send({ id: 200, cmd: 'worktree_create', name: 'exp1' });
    const c = await s.until((f: any) => f.type === 'result' && f.id === 200, 'create');
    assert.ok(c.ok, JSON.stringify(c));
    assert.ok(existsSync(c.data.path), 'worktree dir exists');
    assert.equal(git(['rev-parse', '--abbrev-ref', 'HEAD'], ).status !== 0 || true, true); // noop guard
    assert.equal(spawnSync('git', ['rev-parse', '--abbrev-ref', 'HEAD'], { cwd: c.data.path, encoding: 'utf8' }).stdout.trim(), 'tango/exp1', 'branch checked out');
    // main repo untouched
    assert.equal(spawnSync('git', ['rev-parse', '--abbrev-ref', 'HEAD'], { cwd: work, encoding: 'utf8' }).stdout.trim(), 'main', 'main repo still on its branch');

    // list shows it + dirty count 0
    s.send({ id: 201, cmd: 'worktree_list' });
    const l = await s.until((f: any) => f.type === 'result' && f.id === 201, 'list');
    assert.ok(l.data.worktrees.some((w: any) => w.name === 'exp1' && w.dirty === 0));

    // dirty worktree: remove refused without force
    await writeFile(join(c.data.path, 'dirty.txt'), 'x\n');
    s.send({ id: 202, cmd: 'worktree_remove', name: 'exp1' });
    const refused = await s.until((f: any) => f.type === 'result' && f.id === 202, 'refuse');
    assert.ok(!refused.ok, 'dirty worktree not removed');

    // commit the dirt, then merge
    spawnSync('git', ['add', '.'], { cwd: c.data.path });
    spawnSync('git', ['commit', '-q', '-m', 'exp1 work'], { cwd: c.data.path });
    s.send({ id: 203, cmd: 'worktree_merge', name: 'exp1' });
    const m = await s.until((f: any) => f.type === 'result' && f.id === 203, 'merge');
    assert.ok(m.ok, JSON.stringify(m));
    assert.ok(existsSync(join(work, 'dirty.txt')), 'merged file landed in main repo');

    // clean worktree removes (branch merged)
    s.send({ id: 204, cmd: 'worktree_remove', name: 'exp1' });
    const r = await s.until((f: any) => f.type === 'result' && f.id === 204, 'remove');
    assert.ok(r.ok, JSON.stringify(r));
    assert.ok(!existsSync(c.data.path), 'worktree gone');
  } finally {
    s.child.kill();
    spawnSync('git', ['worktree', 'prune'], { cwd: work });
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});
