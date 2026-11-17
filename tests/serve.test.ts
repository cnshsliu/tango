import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
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
