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
 * End-to-end protocol test: real `tango serve --stdio` child process against a local
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

async function startServe(home: string, cwd: string, port: number) {
  const env = { ...process.env, TANGO_HOME: home, MOCK_SERVE_KEY: 'sk-test' };
  for (const k of ['HTTP_PROXY', 'http_proxy', 'HTTPS_PROXY', 'https_proxy']) delete env[k];
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

test('serve --stdio: hello, status, prompt with a permission denial, session persistence', async () => {
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

test('serve --stdio: stdin EOF does not cut a queued prompt short (pipe usage)', async () => {
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

test('serve --stdio: set_mode and read-only mode are enforced', async () => {
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
