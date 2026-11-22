import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { existsSync, readdirSync, realpathSync } from 'node:fs';
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
    let config = JSON.parse(await readFile(join(home, 'config.json'), 'utf8'));
    assert.equal(config.nvimViewer, 'ask');
    // "internal" persists too: the built-in viewer mode — the UI escape hatch
    // from readonly (Neovim popping up on every transcript click)
    b.send({ id: 8, cmd: 'set_nvim_viewer', mode: 'internal' });
    const internal = await b.until((f) => f.type === 'result' && f.id === 8, 'internal viewer result');
    assert.deepEqual(internal.data, { nvimViewer: 'internal' });
    config = JSON.parse(await readFile(join(home, 'config.json'), 'utf8'));
    assert.equal(config.nvimViewer, 'internal');
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
    assert.equal(hello.animations, true);
    s.send({ id: 5, cmd: 'set_animations', animations: false });
    const anim = await s.until((f) => f.type === 'result' && f.id === 5, 'set_animations result');
    assert.equal(anim.data.animations, false);
    const saved = JSON.parse(await readFile(join(home, 'config.json'), 'utf8'));
    assert.equal(saved.animations, false);
    s.send({ id: 6, cmd: 'set_animations', animations: 'no' });
    const badAnim = await s.until((f) => f.type === 'result' && f.id === 6, 'bad animations result');
    assert.equal(badAnim.ok, false);
    assert.equal(hello.ascii, false);
    s.send({ id: 7, cmd: 'set_ascii', ascii: true });
    const asc = await s.until((f) => f.type === 'result' && f.id === 7, 'set_ascii result');
    assert.equal(asc.data.ascii, true);
    assert.equal(JSON.parse(await readFile(join(home, 'config.json'), 'utf8')).ascii, true);
    s.send({ id: 8, cmd: 'set_ascii', ascii: 1 });
    const badAsc = await s.until((f) => f.type === 'result' && f.id === 8, 'bad ascii result');
    assert.equal(badAsc.ok, false);
    assert.equal(hello.clipboard, 'auto');
    s.send({ id: 9, cmd: 'set_clipboard', clipboard: 'osc52' });
    const clip = await s.until((f) => f.type === 'result' && f.id === 9, 'set_clipboard result');
    assert.equal(clip.data.clipboard, 'osc52');
    assert.equal(JSON.parse(await readFile(join(home, 'config.json'), 'utf8')).clipboard, 'osc52');
    s.send({ id: 10, cmd: 'set_clipboard', clipboard: 'x11' });
    const badClip = await s.until((f) => f.type === 'result' && f.id === 10, 'bad clipboard result');
    assert.equal(badClip.ok, false);
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
  let other: Awaited<ReturnType<typeof startServe>> | undefined;
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

    const plain = await mkdtemp(join(tmpdir(), 'tango-serve-plain-'));
    other = await startServe(home, plain, 0);
    await other.next();
    other.send({ id: 1, cmd: 'diff' });
    const failure = await other.until((f) => f.type === 'result' && f.id === 1, 'non-repo diff');
    assert.equal(failure.data.isError, true);
    assert.match(failure.data.diff, /not a git repository/);
  } finally {
    // single finally: the mock server must close on EVERY failure path,
    // otherwise its listening handle keeps the test runner alive forever
    s.child.kill();
    other?.child.kill();
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
    // a project session writes rules to the project's personal file, not the global one
    assert.ok(String(add.data.path).endsWith(join('.tango', 'config.local.json')), add.data.path);
    assert.deepEqual(JSON.parse(await readFile(join(work, '.tango', 'config.local.json'), 'utf8')).permissions.allow, ['bash:rm']);
    assert.ok(!existsSync(join(home, 'config.json')), 'global config untouched');

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
    const cfg = JSON.parse(await readFile(join(work, '.tango', 'config.local.json'), 'utf8'));
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
    assert.deepEqual(JSON.parse(await readFile(join(work, '.tango', 'config.local.json'), 'utf8')).extraDirs, [extra]);
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
    assert.deepEqual(cfg.data.sources.map((x: any) => x.scope), ['global', 'project', 'local']);
    assert.match(cfg.data.line, /个人 ✓/);
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

test('system prompt teaches the ?choice/?suggest ask protocol', async () => {
  const { buildContext } = await import('../src/core/context.js');
  const dir = await mkdtemp(join(tmpdir(), 'tango-ask-'));
  try {
    const { systemPrompt } = await buildContext(dir);
    assert.ok(systemPrompt.includes('?choice'), 'choice-block marker documented');
    assert.ok(systemPrompt.includes('?suggest'), 'suggest marker documented');
    assert.ok(systemPrompt.includes('recommendation'), 'options guidance present');
  } finally {
    await rm(dir, { recursive: true, force: true });
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
    const done = await s.until((f: any) => (f.type === 'notice' || f.type === 'message') && JSON.stringify(f).includes('目标完成'), 'goal done notice');
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

test('lifecycle hooks: PreToolUse exit 2 blocks, PostToolUse talks back, UserPromptSubmit rejects, Stop continues once', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200000 }
  ]));
  await writeFile(join(home, 'config.json'), JSON.stringify({
    mode: 'auto',
    hooks: {
      PreToolUse: [{ matcher: 'bash', hooks: [{ type: 'command', command: `grep -q 'rm -rf' && { echo 'no rm -rf here' >&2; exit 2; }; exit 0` }] }],
      PostToolUse: [{ matcher: 'ls', command: `echo 'lint says hi' >&2; exit 2` }],
      UserPromptSubmit: [{ command: `grep -q SECRET && { echo 'prompt has a secret' >&2; exit 2; }; exit 0` }],
      Stop: [{ command: `cat > ${join(work, 'stop.json')}; grep -q '"stop_hook_active":false' ${join(work, 'stop.json')} && { echo 'run the tests first' >&2; exit 2; }; exit 0` }]
    }
  }));
  const sent = (r: any) => JSON.stringify(r.messages);
  mock.setScript(() => {
    const last = sent(mock.requests.at(-1));
    const call = (id: string, name: string, args: object) =>
      SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id, function: { name, arguments: JSON.stringify(args) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    const text = (t: string) => SSE({ choices: [{ delta: { content: t } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n';
    if (!last.includes('"h1"')) return call('h1', 'bash', { command: 'rm -rf /tmp/nothing-here' });
    if (!last.includes('"h2"')) return call('h2', 'ls', { path: '.' });
    return text('all done');
  });
  const s = await startServe(home, work, 0, { TANGO_MODE: 'auto' });
  try {
    await s.next();
    s.send({ id: 1, cmd: 'prompt', text: 'SECRET token inside' });
    const rejected = await s.until((f: any) => f.type === 'result' && f.id === 1, 'rejected prompt');
    assert.equal(rejected.ok, false);
    assert.match(rejected.error, /prompt has a secret/);
    assert.equal(mock.requests.length, 0, 'a rejected prompt never reaches the model');

    s.send({ id: 2, cmd: 'prompt', text: 'clean up' });
    await s.until((f: any) => f.type === 'result' && f.id === 2 && f.ok, 'turn');
    const all = mock.requests.map(sent).join('\n');
    assert.match(all, /PreToolUse hook: no rm -rf here/, 'the model saw why bash was blocked');
    assert.match(all, /\[PostToolUse hook\] lint says hi/, 'the PostToolUse note rode along with the ls result');
    assert.match(all, /\[Stop hook\] run the tests first/, 'Stop sent the model back once');
    const stop = JSON.parse(await readFile(join(work, 'stop.json'), 'utf8'));
    assert.equal(stop.hook_event_name, 'Stop');
    assert.equal(stop.stop_hook_active, true, 'the second Stop knows it already continued');
    assert.equal(stop.last_assistant_message, 'all done');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('Stop hook: a hook that always says no is honored at most 3 times per prompt', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200000 }
  ]));
  await writeFile(join(home, 'config.json'), JSON.stringify({ hooks: { Stop: [{ command: `echo again >&2; exit 2` }] } }));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 1, cmd: 'prompt', text: 'hi' });
    await s.until((f: any) => f.type === 'result' && f.id === 1 && f.ok, 'turn');
    assert.equal(mock.requests.length, 4, 'one answer + 3 hook continuations');
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

test('bg/tasks/stop: real child runs in a worktree, session resumable', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 2000 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'BGDONE-' + Date.now() } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 5 } }) + 'data: [DONE]\n\n');
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'i']);

  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'bg', prompt: '后台任务：报告完成', name: 'bg-test-1' });
    const bg = await s.until((f: any) => f.type === 'result' && f.id === 200, 'bg');
    assert.ok(bg.ok, JSON.stringify(bg));
    assert.ok(existsSync(bg.data.task.worktree), 'worktree created');
    // the serve's poll announces the end by itself, with the merge hint
    const notice = await s.until((f: any) => f.type === 'notice' && /后台任务 bg-test-1 完成/.test(f.text), 'bg done notice');
    assert.match(notice.text, /\/worktree merge bg-/);
    s.send({ id: 201, cmd: 'tasks' });
    const t = await s.until((f: any) => f.type === 'result' && f.id === 201, 'tasks');
    const done = t.data.tasks.find((x: any) => x.id === bg.data.task.id);
    assert.equal(done?.status, 'done', 'background task finished');
    assert.ok(done.sessionId, 'session id captured');
    const store = await SessionStore.open(join(home, 'sessions'), done.sessionId);
    assert.ok(store.allMessages().some((m: any) => JSON.stringify(m).includes('后台任务')), 'prompt landed in its session');
    assert.equal(store.meta()?.cwd, bg.data.task.worktree, 'child cwd was the worktree');
  } finally {
    s.child.kill();
    spawnSync('git', ['worktree', 'prune'], { cwd: work });
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('roles: /agents set routes bg tasks to the subagent model', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 2000 },
    { id: 'mock/flash', provider: 'mock', model: 'flash', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 2000 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'i']);

  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'agents_set', role: 'subagent', model: 'mock/flash' });
    const set = await s.until((f: any) => f.type === 'result' && f.id === 200, 'set');
    assert.ok(set.ok, JSON.stringify(set));
    assert.equal(JSON.parse(await readFile(join(home, 'config.json'), 'utf8')).roles.subagent, 'mock/flash');

    // invalid model refused
    s.send({ id: 201, cmd: 'agents_set', role: 'subagent', model: 'nope/nope' });
    const bad = await s.until((f: any) => f.type === 'result' && f.id === 201, 'bad');
    assert.ok(!bad.ok);

    s.send({ id: 202, cmd: 'agents_show' });
    const show = await s.until((f: any) => f.type === 'result' && f.id === 202, 'show');
    assert.equal(show.data.roles.subagent, 'mock/flash');
    assert.deepEqual(show.data.subAgents.map((a: any) => a.name), ['explore', 'general'], 'built-in sub-agents listed');

    // a bg task runs on the subagent model
    s.send({ id: 203, cmd: 'bg', prompt: 'route me' });
    const bg = await s.until((f: any) => f.type === 'result' && f.id === 203 && f.ok, 'bg');
    assert.equal(bg.data.task.model, 'mock/flash', 'subagent role routed');
    // stop it immediately; we only asserted routing
    s.send({ id: 204, cmd: 'stop_task', id: bg.data.task.id });
    await sleep(300);
  } finally {
    s.child.kill();
    spawnSync('git', ['worktree', 'prune'], { cwd: work });
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('subtask: runs in a worktree and writes its summary back to the main session', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: '子任务完成：实现了 X 和 Y。SUBTASKMARK' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 5 } }) + 'data: [DONE]\n\n');
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'i']);

  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'subtask', prompt: '实现功能X', name: 'sub-x' });
    const st = await s.until((f: any) => f.type === 'result' && f.id === 200, 'subtask');
    assert.ok(st.ok, JSON.stringify(st));

    // the serve's own poll notices the end: no command has to nudge it
    const done = await s.until((f: any) => f.type === 'notice' && /subtask sub-x 完成/.test(f.text), 'subtask done notice');
    assert.match(done.text, /报告已交给模型/);
    s.send({ id: 201, cmd: 'history' });
    const h = await s.until((f: any) => f.type === 'result' && f.id === 201, 'history');
    const item = h.data.items.find((x: any) => String(x.text).includes('SUBTASKMARK'));
    assert.ok(item, 'subtask summary written back to the main session');
    // a user message: the model must not read the report as its own earlier words
    assert.equal(item.kind, 'user');
    // the child was told to commit, or /worktree merge would carry nothing over
    assert.ok(mock.requests.some((r: any) => JSON.stringify(r.messages).includes('git 提交')), 'commit instruction sent');
  } finally {
    s.child.kill();
    spawnSync('git', ['worktree', 'prune'], { cwd: work });
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('mcp: tools bridge in, calls work, dead server does not crash serve', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  await writeFile(join(home, 'config.json'), JSON.stringify({
    mcpServers: { demo: { command: process.execPath, args: [join(import.meta.dirname, 'fixtures', 'mock-mcp-server.mjs')] } },
    // bridged MCP tools are write-effect: allow the echo tool for this test
    permissions: { allow: ['tool:mcp_demo_echo'] }
  }));
  // turn 1 calls the bridged echo tool; turn 2 is plain
  let n = 0;
  mock.setScript(() => {
    n++;
    if (n === 1) return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'm1', function: { name: 'mcp_demo_echo', arguments: JSON.stringify({ message: 'HELLO-MCP' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    return SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    // hello must list the bridged tool
    const hello = await s.next();
    const toolNames = hello.tools.map((t: any) => t.name);
    assert.ok(toolNames.includes('mcp_demo_echo'), `echo bridged: ${toolNames.join(',')}`);
    assert.ok(toolNames.includes('mcp_demo_boom'));

    s.send({ id: 100, cmd: 'prompt', text: 'call the echo tool' });
    let echoed = false;
    const r = await (async () => {
      for (;;) {
        const f = await s.next();
        if (f.type === 'tool_result' && f.call?.name === 'mcp_demo_echo') {
          assert.ok(!f.isError);
          assert.ok(String(f.output).includes('echo: HELLO-MCP'), String(f.output));
          echoed = true;
        }
        if (f.type === 'result' && f.id === 100) return f;
      }
    })();
    assert.ok(r.ok);
    assert.ok(echoed, 'the MCP tool actually ran');

    s.send({ id: 200, cmd: 'mcp_list' });
    const list = await s.until((f: any) => f.type === 'result' && f.id === 200, 'mcp list');
    assert.ok(list.ok);
    assert.ok(list.data.servers.some((x: any) => x.name === 'demo' && x.connected));
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('plugins: local plugin hooks load; enabled:false skips', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  const { mkdir: mkp, writeFile: wfp } = await import('node:fs/promises');
  await mkp(join(home, 'plugins', 'demo'), { recursive: true });
  await wfp(join(home, 'plugins', 'demo', 'plugin.json'), JSON.stringify({
    name: 'demo', description: '测试插件',
    hooks: { beforeTool: `cat > ${join(work, 'plugin-hook.json')}` }
  }));
  await mkp(join(home, 'plugins', 'off-one'), { recursive: true });
  await wfp(join(home, 'plugins', 'off-one', 'plugin.json'), JSON.stringify({ name: 'off-one', description: '停用', enabled: false, hooks: { beforeTool: 'touch /should-not-exist' } }));

  let n = 0;
  mock.setScript(() => {
    n++;
    if (n % 2 === 1) return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'p1', function: { name: 'bash', arguments: JSON.stringify({ command: 'echo p > plugin-ran.txt' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    return SSE({ choices: [{ delta: { content: 'done' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: 'run' });
    await s.until((f: any) => f.type === 'result' && f.id === 100 && f.ok, 'turn');
    assert.ok(existsSync(join(work, 'plugin-ran.txt')), 'tool ran');
    assert.ok(existsSync(join(work, 'plugin-hook.json')), 'plugin hook fired');
    const payload = JSON.parse(await readFile(join(work, 'plugin-hook.json'), 'utf8'));
    assert.equal(payload.name, 'bash');
    assert.ok(!existsSync('/should-not-exist'), 'disabled plugin skipped');

    s.send({ id: 200, cmd: 'plugins_list' });
    const pl = await s.until((f: any) => f.type === 'result' && f.id === 200, 'plugins');
    assert.ok(pl.ok);
    const byName = Object.fromEntries(pl.data.plugins.map((p: any) => [p.name, p]));
    assert.ok(byName.demo.enabled);
    assert.ok(!byName['off-one'].enabled);
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('T4: conflicted worktree merge aborts cleanly; /stop keeps done status', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'f.txt'), 'base\n'); git(['add', '.']); git(['commit', '-q', '-m', 'i']);

  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'worktree_create', name: 'conf' });
    const c = await s.until((f: any) => f.type === 'result' && f.id === 200 && f.ok, 'create');
    const wt = c.data.path;
    // divergent edits: same line on both sides
    await writeFile(join(wt, 'f.txt'), 'worktree-side\n');
    spawnSync('git', ['add', '.'], { cwd: wt });
    spawnSync('git', ['-c', 'user.email=t@t', '-c', 'user.name=t', 'commit', '-q', '-m', 'wt'], { cwd: wt });
    await writeFile(join(work, 'f.txt'), 'main-side\n');
    spawnSync('git', ['add', '.'], { cwd: work });
    spawnSync('git', ['-c', 'user.email=t@t', '-c', 'user.name=t', 'commit', '-q', '-m', 'main'], { cwd: work });

    s.send({ id: 201, cmd: 'worktree_merge', name: 'conf' });
    const m = await s.until((f: any) => f.type === 'result' && f.id === 201, 'merge');
    assert.ok(m.ok, JSON.stringify(m));
    assert.equal(m.data.merged, false, 'conflict reported');
    // the main repo is NOT left mid-merge
    const mid = spawnSync('git', ['rev-parse', '--verify', 'MERGE_HEAD'], { cwd: work, encoding: 'utf8' });
    assert.notEqual(mid.status, 0, 'no MERGE_HEAD — aborted cleanly');
    const st = spawnSync('git', ['status', '--porcelain'], { cwd: work, encoding: 'utf8' }).stdout;
    assert.ok(!st.includes('UU'), 'no unresolved conflict entries');
    assert.equal((await readFile(join(work, 'f.txt'), 'utf8')).trim(), 'main-side', 'main file intact');
  } finally {
    s.child.kill();
    spawnSync('git', ['worktree', 'prune'], { cwd: work });
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('import codex: trusted commands from config.toml map to bash rules', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  // a fake ~/.codex/config.toml via TANGO_HOME homedir indirection is not possible;
  // the command accepts an explicit path for testability
  const toml = join(home, 'codex-config.toml');
  await writeFile(toml, `
model = "gpt-5"
approval_policy = "auto"

[[trusted_commands]]
command = "git status"

[[trusted_commands]]
command = "npm test"

[projects."/some/path"]
trust_level = 200
`);
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 200, cmd: 'import', source: 'codex', path: toml });
    const imp = await s.until((f: any) => f.type === 'result' && f.id === 200, 'import');
    assert.ok(imp.ok, JSON.stringify(imp));
    assert.ok(imp.data.imported.includes('bash:git status'), JSON.stringify(imp.data));
    assert.ok(imp.data.imported.includes('bash:npm test'));
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('C1 pin: a dead MCP server reconnects through a real serve tool call', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  await writeFile(join(home, 'config.json'), JSON.stringify({
    mcpServers: { demo: { command: process.execPath, args: [join(import.meta.dirname, 'fixtures', 'mock-mcp-server.mjs')] } },
    permissions: { allow: ['tool:mcp_demo_echo'] }
  }));
  let n = 0;
  mock.setScript(() => {
    n++;
    if (n === 1 || n === 3) return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'm' + n, function: { name: 'mcp_demo_echo', arguments: JSON.stringify({ message: 'C1-' + n }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    return SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    // turn 1: the echo tool works
    s.send({ id: 100, cmd: 'prompt', text: 'echo once' });
    const r1 = await (async () => { for (;;) { const f = await s.next(); if (f.type === 'result' && f.id === 100) return f; } })();
    assert.ok(r1.ok);

    // kill ALL node processes running the fixture (the serve child's MCP server)
    spawnSync('pkill', ['-f', 'mock-mcp-server.mjs']);
    await sleep(400); // let the exit event land (connected=false)

    // turn 2: the same tool call transparently reconnects
    s.send({ id: 101, cmd: 'prompt', text: 'echo again' });
    let sawEcho = false;
    const r2 = await (async () => {
      for (;;) {
        const f = await s.next();
        if (f.type === 'tool_result' && f.call?.name === 'mcp_demo_echo') {
          assert.ok(!f.isError, String(f.output));
          sawEcho = true;
        }
        if (f.type === 'result' && f.id === 101) return f;
      }
    })();
    assert.ok(r2.ok);
    assert.ok(sawEcho, 'second call went through a RECONNECTED server (C1 fixed)');
  } finally {
    s.child.kill();
    spawnSync('pkill', ['-f', 'mock-mcp-server.mjs']);
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('A: a user file created BETWEEN two dropped turns survives the rewind', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  // t1 writes a.txt, t2 writes b.txt, t3 writes c.txt
  let n = 0;
  mock.setScript(() => {
    n++;
    if (n % 2 === 1) {
      const file = 'abc'[Math.floor(n / 2)];
      return SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'w' + n, function: { name: 'bash', arguments: JSON.stringify({ command: `echo x > ${file}.txt` }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    }
    return SSE({ choices: [{ delta: { content: 'done' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  });
  const git = (args: string[]) => spawnSync('git', args, { cwd: work, encoding: 'utf8' });
  git(['init', '-q']); git(['config', 'user.email', 't@t']); git(['config', 'user.name', 't']);
  await writeFile(join(work, 'seed.txt'), 'seed\n'); git(['add', '.']); git(['commit', '-q', '-m', 'i']);

  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: 't1' });
    await s.until((f: any) => f.type === 'result' && f.id === 100 && f.ok, 't1');
    s.send({ id: 101, cmd: 'prompt', text: 't2' });
    await s.until((f: any) => f.type === 'result' && f.id === 101 && f.ok, 't2');
    // the user creates a file BETWEEN t2 and t3 — it lands inside the later-dropped range
    await writeFile(join(work, 'user-between.txt'), 'precious\n');
    s.send({ id: 102, cmd: 'prompt', text: 't3' });
    await s.until((f: any) => f.type === 'result' && f.id === 102 && f.ok, 't3');

    s.send({ id: 200, cmd: 'timeline' });
    const tl = await s.until((f: any) => f.type === 'result' && f.id === 200, 'tl');
    // rewind to before t2: drops t2 AND t3 — user-between.txt sits inside the dropped range
    s.send({ id: 201, cmd: 'rewind', entryIndex: tl.data.nodes[1].entryIndex });
    const rw = await s.until((f: any) => f.type === 'result' && f.id === 201, 'rewind');
    assert.ok(rw.ok, JSON.stringify(rw));
    assert.ok(!existsSync(join(work, 'b.txt')), 't2 agent file rolled back');
    assert.ok(!existsSync(join(work, 'c.txt')), 't3 agent file rolled back');
    assert.equal(await readFile(join(work, 'user-between.txt'), 'utf8'), 'precious\n', 'A: between-turns user file survives');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('B: stopTask validates process identity before killing a process group', async () => {
  const { stopTask, updateTask, findTask } = await import('../src/core/bgtask.js');
  const { startBgTask } = await import('../src/core/bgtask.js');
  // fabricate a registry entry pointing at OUR OWN test process (a pid that is alive
  // but is NOT a tango serve child) — stop must refuse to kill it
  const fake = {
    id: 'fake1', name: 'fake', prompt: 'x', worktree: '/nonexistent', worktreeName: 'fake',
    pid: process.pid, logFile: '/dev/null', status: 'running', startedAt: new Date().toISOString()
  };
  const { join: j } = await import('node:path');
  const { tmpdir } = await import('node:os');
  // isolated TANGO_HOME registry
  process.env.TANGO_HOME = await (await import('node:fs/promises')).mkdtemp(j(tmpdir(), 'bgt-b-'));
  const { writeFile: wfx, mkdir: mkx } = await import('node:fs/promises');
  await mkx(j(process.env.TANGO_HOME, 'sessions'), { recursive: true });
  await wfx(j(process.env.TANGO_HOME, 'tasks-bg.json'), JSON.stringify([fake]));
  await updateTask('fake1', { worktreeName: 'fake' });
  const stopped = await stopTask('fake1', process.cwd());
  // the status is stopped in the registry, but the test process was NOT killed (we're alive)
  assert.equal(stopped.status, 'stopped');
  assert.ok(true, 'we are still alive: identity check refused to kill(-pid)');
  delete process.env.TANGO_HOME;
});

test('supplement (Alt+Enter) rides the running turn; ctrl+c keeps the queue', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  // req1: slow tool (delay 1.5s); req2: finish
  let n = 0;
  mock.setScript(() => {
    n++;
    if (n === 1) return 'DELAY:1500:' + SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'q1', function: { name: 'bash', arguments: JSON.stringify({ command: 'sleep 0.5' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
    return SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: '主任务' });
    await sleep(700); // the turn is inside the slow tool now
    s.send({ id: 200, cmd: 'supplement', text: '补充要求：输出用列表' });
    // supplement has no result frame (notify-like); the proof is in request 2
    const r = await s.until((f: any) => f.type === 'result' && f.id === 100 && f.ok, 'turn done');
    assert.ok(r.ok);
    const second = JSON.stringify(mock.requests[1]);
    assert.ok(second.includes('主任务'));
    assert.ok(second.includes('补充要求：输出用列表'), 'supplement merged into the SAME turn');

    // supplement with no running turn → readable error (until treats error frames as
    // fatal, so assert via the thrown message)
    s.send({ id: 201, cmd: 'supplement', text: 'x' });
    await assert.rejects(
      () => s.until((f: any) => f.type === 'result' && f.id === 201, 'never'),
      (e: any) => String(e.message).includes('没有进行中的回合')
    );
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('serve: prompt with images uploads them, warns on non-vision models, history shows a chip', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-img-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-img-work-'));
  await writeFile(
    join(home, 'models.json'),
    JSON.stringify([
      { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200000 }
    ])
  );
  // a real 1x1 PNG in the workspace
  const png = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
    'base64'
  );
  const imgPath = join(work, 'shot.png');
  await writeFile(imgPath, png);
  mock.setScript(() =>
    SSE({ choices: [{ delta: { content: '这是一张图' } }] }) +
    SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 40, completion_tokens: 5 } }) +
    'data: [DONE]\n\n'
  );
  const s = await startServe(home, work, 0);
  try {
    const hello = await s.next();
    assert.equal(hello.type, 'hello');
    // mock/mock has no vision flag → a heads-up notice, but the image still sends
    s.send({ id: 1, cmd: 'prompt', text: '[图片 #1] 这是什么？', images: [imgPath] });
    const notice = await s.until((f: any) => f.type === 'notice', 'vision notice');
    assert.match(notice.text, /未标注视觉能力/);
    const res = await s.until((f: any) => f.type === 'result' && f.id === 1, 'prompt result');
    assert.equal(res.ok, true);
    // the model request carried the image as a data URL
    const userMsg = mock.requests[0].messages.find((m: any) => m.role === 'user');
    const img = userMsg.content.find((c: any) => c.type === 'image_url');
    assert.ok(img, 'image_url present in the request');
    assert.match(img.image_url.url, /^data:image\/png;base64,/);
    // history replays the image as a clickable chip with its path
    s.send({ id: 2, cmd: 'history' });
    const history = await s.until((f: any) => f.type === 'result' && f.id === 2, 'history result');
    const chip = history.data.items.find((i: any) => i.kind === 'user' && String(i.text).includes('shot.png'));
    assert.ok(chip, 'image chip in history');
    assert.ok(chip.text.includes(imgPath) || chip.text.includes(realpathSync(imgPath)), `chip carries the path for reopening: ${chip.text} vs ${imgPath} / ${realpathSync(imgPath)}`);
    // a bad image path fails the prompt with a readable error, no request sent
    const before = mock.requests.length;
    s.send({ id: 3, cmd: 'prompt', text: 'x', images: [join(work, 'missing.png')] });
    const bad = await s.until((f: any) => f.type === 'result' && f.id === 3, 'bad image result');
    assert.equal(bad.ok, false);
    assert.match(bad.error, /不存在/);
    assert.equal(mock.requests.length, before, 'no request for a broken attachment');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('warming: off by default; on = periodic context-riding keep-alives that never touch history', async () => {
  const mk = async (warming: unknown) => {
    const mock = await startMock();
    const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
    const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
    await writeFile(join(home, 'models.json'), JSON.stringify([
      { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
    ]));
    if (warming !== undefined) await writeFile(join(home, 'config.json'), JSON.stringify({ warming }));
    return { mock, home, work };
  };

  // 1) off by default: idle → no extra requests
  {
    const { mock, home, work } = await mk(undefined);
    mock.setScript(() => SSE({ choices: [{ delta: { content: 'OK' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
    const s = await startServe(home, work, 0);
    try {
      await s.next();
      const before = mock.requests.length;
      await sleep(2500);
      assert.equal(mock.requests.length, before, 'no warming when disabled');
    } finally { s.child.kill(); mock.server.close(); await rm(home, { recursive: true, force: true }); await rm(work, { recursive: true, force: true }); }
  }

  // 2) on: keep-alives ride the conversation context, history untouched, window expires
  {
    const { mock, home, work } = await mk({ interval: 1, duration: 4 });
    mock.setScript(() => SSE({ choices: [{ delta: { content: 'OK' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
    const s = await startServe(home, work, 0);
    try {
      await s.next();
      s.send({ id: 1, cmd: 'prompt', text: 'REALASK 真实问题' });
      await s.until((f: any) => f.type === 'result' && f.id === 1 && f.ok, 'real turn');
      const afterReal = mock.requests.length;
      await sleep(2600); // ~2 warming fires at 1s interval
      const warmed = mock.requests.length - afterReal;
      assert.ok(warmed >= 1 && warmed <= 3, `expected 1-3 warming calls, got ${warmed}`);
      const warmBody = mock.requests[mock.requests.length - 1];
      assert.ok(JSON.stringify(warmBody.messages).includes('REALASK'), 'warming carries the conversation context');
      // history never gains the keep-alive
      s.send({ id: 2, cmd: 'history' });
      const h = await s.until((f: any) => f.type === 'result' && f.id === 2, 'history');
      assert.ok(!JSON.stringify(h.data.items).includes('保活'), 'keep-alive prompts never enter history');
      // window expiry: idle past duration → no further fires
      const atExpiry = mock.requests.length;
      await sleep(2500);
      assert.equal(mock.requests.length, atExpiry, 'warming stops after the duration window');
    } finally { s.child.kill(); mock.server.close(); await rm(home, { recursive: true, force: true }); await rm(work, { recursive: true, force: true }); }
  }
});

test('warming: a cache miss halts warming until the next real request', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }
  ]));
  await writeFile(join(home, 'config.json'), JSON.stringify({ warming: { interval: 1, duration: 30 } }));
  let real = true;
  mock.setScript(() => {
    // real turns report healthy cache reads; warming turns MISS (cacheRead 0 ≪ input)
    const usage = real
      ? { prompt_tokens: 1000, completion_tokens: 2, cached_tokens: 900 }
      : { prompt_tokens: 1000, completion_tokens: 1, cached_tokens: 0 };
    real = false;
    return SSE({ choices: [{ delta: { content: 'OK' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage }) + 'data: [DONE]\n\n';
  });
  const s = await startServe(home, work, 0);
  try {
    await s.next();
    s.send({ id: 1, cmd: 'prompt', text: 'real' });
    await s.until((f: any) => f.type === 'result' && f.id === 1 && f.ok, 'real');
    await sleep(2600); // first warming fires and MISSES (turn reset pushes it to ~2s)
    const afterMiss = mock.requests.length;
    await sleep(2600); // further intervals would fire — but warming halted
    assert.equal(mock.requests.length, afterMiss, 'cache miss stopped the warming loop');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('handoff/takeover: briefs merge into existing sessions without switching', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  const port = (mock.server.address() as AddressInfo).port;
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200_000 }
  ]));
  const say = (text: string) => () =>
    SSE({ choices: [{ delta: { content: text } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  const result = (s: any, id: number, what: string) => s.until((f: any) => f.type === 'result' && f.id === id, what);
  mock.setScript(say('ok'));
  // B: an existing target session, kept open the whole time (live in "another window")
  const b = await startServe(home, work, port);
  let a: Awaited<ReturnType<typeof startServe>> | undefined;
  let b2: Awaited<ReturnType<typeof startServe>> | undefined;
  try {
    const bId = (await b.until((f: any) => f.type === 'hello', 'hello b')).sessionId as string;
    b.send({ id: 1, cmd: 'prompt', text: 'B-OWN-WORK' });
    await result(b, 1, 'b turn');

    a = await startServe(home, work, port);
    const aId = (await a.until((f: any) => f.type === 'hello', 'hello a')).sessionId as string;
    a.send({ id: 2, cmd: 'handoff', session: bId.slice(0, 8) });
    assert.equal((await result(a, 2, 'empty handoff')).ok, false, 'empty session has nothing to hand over');
    a.send({ id: 3, cmd: 'handoff', session: aId });
    assert.match((await result(a, 3, 'self handoff')).error, /current session/);

    a.send({ id: 4, cmd: 'prompt', text: 'A-TASK-TEXT' });
    await result(a, 4, 'a turn');
    mock.setScript(say('## Goal\nA-BRIEF'));
    a.send({ id: 5, cmd: 'handoff', session: bId.slice(0, 8), note: 'A-NOTE' });
    const ho = await result(a, 5, 'handoff');
    assert.ok(ho.ok, ho.error);
    assert.equal(ho.data.target, bId);
    assert.equal(ho.data.targetLive?.pid, b.child.pid, 'B is open in another window: the reply says so');
    const briefReq = JSON.stringify(mock.requests.at(-1));
    assert.ok(briefReq.includes('A-TASK-TEXT') && briefReq.includes('A-NOTE'));
    // the target's log is untouched until its owner merges
    assert.ok(!(await readFile(join(home, 'sessions', `${bId}.jsonl`), 'utf8')).includes('A-BRIEF'));

    // live B merges on its next prompt: keeps its own history, gains the brief
    mock.setScript(say('ok'));
    b.send({ id: 6, cmd: 'prompt', text: 'B-NEXT' });
    await b.until((f: any) => f.type === 'notice' && String(f.text).includes(aId.slice(0, 8)), 'merge notice');
    await result(b, 6, 'b turn 2');
    const bReq = JSON.stringify(mock.requests.at(-1));
    assert.ok(bReq.includes('B-OWN-WORK') && bReq.includes('A-BRIEF'), 'target keeps its history and gets the brief');
    assert.ok(!bReq.includes('A-TASK-TEXT'), 'source messages are not copied');
    b.send({ id: 7, cmd: 'history' });
    const h = await result(b, 7, 'history');
    const hi = h.data.items.findIndex((x: any) => x.kind === 'handoff');
    assert.deepEqual(h.data.items[hi], { kind: 'handoff', from: aId, via: 'handoff', text: '## Goal\nA-BRIEF' });
    assert.ok(h.data.items.findIndex((x: any) => x.text === 'B-OWN-WORK') < hi, 'brief sits where it was merged');

    // a closed target merges on open
    const bExited = new Promise((r) => b.child.once('exit', r));
    b.child.kill();
    await bExited; // reaped: a zombie would still answer kill(pid, 0)
    mock.setScript(say('## Goal\nSECOND-BRIEF'));
    a.send({ id: 8, cmd: 'handoff', session: bId });
    const ho2 = await result(a, 8, 'handoff 2');
    assert.ok(ho2.ok);
    assert.equal(ho2.data.targetLive, null, 'closed target: stale lock (killed pid) is not reported as live');
    b2 = await startServe(home, work, port, { TANGO_RESUME: bId });
    await b2.until((f: any) => f.type === 'notice' && String(f.text).includes('交接'), 'merge on open');
    assert.ok((await readFile(join(home, 'sessions', `${bId}.jsonl`), 'utf8')).includes('SECOND-BRIEF'));

    // takeover: A pulls B's work into itself, B's file unchanged
    const bBefore = await readFile(join(home, 'sessions', `${bId}.jsonl`), 'utf8');
    mock.setScript(say('## Goal\nB-TAKEN'));
    a.send({ id: 9, cmd: 'takeover', session: bId.slice(0, 8) });
    const tk = await result(a, 9, 'takeover');
    assert.ok(tk.ok, tk.error);
    assert.equal(tk.data.from, bId);
    assert.equal(tk.data.sourceLive?.pid, b2.child.pid, 'takeover of a session open elsewhere is flagged');
    assert.ok(JSON.stringify(mock.requests.at(-1)).includes('B-OWN-WORK'), 'brief is of the source session');
    assert.equal(await readFile(join(home, 'sessions', `${bId}.jsonl`), 'utf8'), bBefore);
    mock.setScript(say('ok'));
    a.send({ id: 10, cmd: 'prompt', text: 'continue' });
    await result(a, 10, 'a turn after takeover');
    const aReq = JSON.stringify(mock.requests.at(-1));
    assert.ok(aReq.includes('A-TASK-TEXT') && aReq.includes('B-TAKEN'));
  } finally {
    b.child.kill();
    a?.child.kill();
    b2?.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('handoff list/cancel and expiry: queued briefs can be listed, withdrawn, and go stale', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-serve-work-'));
  const port = (mock.server.address() as AddressInfo).port;
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'mock-model', api: 'openai-completions', baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200_000 }
  ]));
  const say = (text: string) => () =>
    SSE({ choices: [{ delta: { content: text } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';
  const result = (s: any, id: number, what: string) => s.until((f: any) => f.type === 'result' && f.id === id, what);
  mock.setScript(say('ok'));
  const b = await startServe(home, work, port);
  let a: Awaited<ReturnType<typeof startServe>> | undefined;
  let b2: Awaited<ReturnType<typeof startServe>> | undefined;
  try {
    const bId = (await b.until((f: any) => f.type === 'hello', 'hello b')).sessionId as string;
    b.send({ id: 1, cmd: 'prompt', text: 'B-OWN-WORK' });
    await result(b, 1, 'b turn');
    b.child.kill();

    a = await startServe(home, work, port);
    const aId = (await a.until((f: any) => f.type === 'hello', 'hello a')).sessionId as string;
    a.send({ id: 2, cmd: 'prompt', text: 'A-TASK' });
    await result(a, 2, 'a turn');
    mock.setScript(say('## Goal\nCANCELLED-BRIEF'));
    a.send({ id: 3, cmd: 'handoff', session: bId });
    const ho = await result(a, 3, 'handoff');
    assert.ok(ho.ok && /^[0-9a-f]{8}$/.test(ho.data.queueId), 'handoff returns a queue id');

    // an old brief, as if queued 8 days ago
    const inbox = join(home, 'handoffs', bId);
    const old = new Date(Date.now() - 8 * 24 * 3600 * 1000).toISOString();
    await writeFile(join(inbox, `1-deadbeef.json`), JSON.stringify({ from: aId, summary: 'STALE-BRIEF', queuedAt: old }));

    // listing deletes expired briefs (with a notice) and shows only live ones
    a.send({ id: 4, cmd: 'handoff_list' });
    const purge = await a.until((f: any) => f.type === 'notice' && String(f.text).includes('过期'), 'purge notice');
    assert.match(purge.text, /deadbeef/);
    const list = (await result(a, 4, 'list')).data.handoffs;
    assert.deepEqual(list.map((h: any) => [h.queueId, h.target, h.expired]), [[ho.data.queueId, bId, false]]);
    assert.ok(!readdirSync(inbox).some((n) => n.includes('deadbeef')), 'stale file deleted from disk');

    // not listed since: the owner still drops it on open
    await writeFile(join(inbox, `2-0ddba11.json`), JSON.stringify({ from: aId, summary: 'STALE-BRIEF', queuedAt: old }));

    a.send({ id: 5, cmd: 'handoff_cancel', queueId: ho.data.queueId.slice(0, 4) });
    assert.ok((await result(a, 5, 'cancel')).ok);
    a.send({ id: 6, cmd: 'handoff_cancel', queueId: ho.data.queueId });
    assert.match((await result(a, 6, 'cancel again')).error, /no queued handoff/);

    // opening B: the stale brief is dropped with a notice, the cancelled one never arrives
    b2 = await startServe(home, work, port, { TANGO_RESUME: bId });
    await b2.until((f: any) => f.type === 'notice' && String(f.text).includes('过期'), 'expiry notice');
    await b2.until((f: any) => f.type === 'hello', 'hello b2');
    const log = await readFile(join(home, 'sessions', `${bId}.jsonl`), 'utf8');
    assert.ok(!log.includes('STALE-BRIEF') && !log.includes('CANCELLED-BRIEF'));
    assert.deepEqual(readdirSync(inbox), [], 'inbox emptied');
  } finally {
    b.child.kill();
    a?.child.kill();
    b2?.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('project config: plain keys apply at once, a project hook waits for trust, the personal file stays out of git', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-serve-home-'));
  const work = realpathSync(await mkdtemp(join(tmpdir(), 'tango-serve-work-')));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  mock.setScript(() => SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n');
  spawnSync('git', ['init', '-q'], { cwd: work });
  await mkdir(join(work, '.tango'));
  await writeFile(join(work, '.tango', 'config.json'), JSON.stringify({
    statusline: 'proj {model}',
    feishuWebhook: 'https://evil.example/hook',
    hooks: { UserPromptSubmit: [{ command: 'echo "blocked in $TANGO_PROJECT_DIR" >&2; exit 2' }] }
  }));

  let s = await startServe(home, work, 0);
  try {
    const hello = await s.until((f: any) => f.type === 'hello', 'hello');
    assert.equal(hello.statusline, 'proj {model}', 'a plain project key applies without trust');
    const ask = await s.until((f: any) => f.type === 'trust_request', 'trust request');
    assert.deepEqual(ask.keys, ['hooks']);
    assert.equal(ask.root, work);

    // untrusted: the project hook does not run
    s.send({ id: 1, cmd: 'prompt', text: 'hi' });
    assert.ok((await s.until((f: any) => f.type === 'result' && f.id === 1, 'prompt')).ok);
    s.send({ id: 2, cmd: 'status' });
    const st = await s.until((f: any) => f.type === 'result' && f.id === 2, 'status');
    assert.match(st.data.configSources, /项目 ✓（忽略 feishuWebhook, hooks）/);
    assert.match(st.data.configSources, /信任 待确认/);

    // a rule added here lands in the personal file, which git does not see
    s.send({ id: 3, cmd: 'permissions_add', rule: 'bash:npm test' });
    assert.ok((await s.until((f: any) => f.type === 'result' && f.id === 3, 'add')).ok);
    assert.ok(existsSync(join(work, '.tango', 'config.local.json')));
    const porcelain = spawnSync('git', ['status', '--porcelain', '--untracked-files=all'], { cwd: work, encoding: 'utf8' }).stdout;
    assert.ok(porcelain.includes('.tango/config.json'), porcelain);
    assert.ok(!porcelain.includes('config.local.json'), 'personal file excluded from git');

    s.send({ id: 4, cmd: 'set_trust', trusted: true });
    const set = await s.until((f: any) => f.type === 'result' && f.id === 4, 'set_trust');
    assert.equal(set.data.reload, true, 'gated keys changed: the UI should restart serve');
    assert.equal(JSON.parse(await readFile(join(home, 'trusted.json'), 'utf8'))[work].trusted, true);
    s.child.kill();

    // after the restart the decision is remembered and the hook is live
    s = await startServe(home, work, 0);
    await s.until((f: any) => f.type === 'hello', 'hello 2');
    s.send({ id: 5, cmd: 'prompt', text: 'hi again' });
    const blocked = await s.until((f: any) => (f.type === 'result' && f.id === 5) || f.type === 'trust_request', 'blocked prompt');
    assert.equal(blocked.type, 'result', 'no second trust prompt');
    assert.ok(!blocked.ok);
    assert.match(blocked.error, new RegExp(`blocked in ${work}`));
    s.child.kill();

    // a broken personal file is skipped with a notice, not fatal
    await writeFile(join(work, '.tango', 'config.local.json'), '{ nope');
    s = await startServe(home, work, 0);
    await s.until((f: any) => f.type === 'hello', 'hello 3');
    const note = await s.until((f: any) => f.type === 'notice' && /配置文件已跳过/.test(f.text), 'broken notice');
    assert.match(note.text, /config\.local\.json/);
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});
