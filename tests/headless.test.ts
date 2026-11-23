import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';

const SSE = (o: object) => `data: ${JSON.stringify(o)}\n\n`;
const toolCall = (id: string, command: string) =>
  SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id, function: { name: 'bash', arguments: JSON.stringify({ command }) } }] }, finish_reason: 'tool_calls' }], usage: { prompt_tokens: 30, completion_tokens: 4 } }) + 'data: [DONE]\n\n';
const answer = (text: string) =>
  SSE({ choices: [{ delta: { content: text } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 50, completion_tokens: 6 } }) + 'data: [DONE]\n\n';

/** run `tango <args>` against a scripted mock model; returns parsed stdout lines */
async function run(script: string[], args: string[], config?: object) {
  let n = 0;
  const server = createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      const body = script[Math.min(n++, script.length - 1)];
      // a script entry 'HTTP 503' answers with that status instead of a stream
      const status = /^HTTP (\d{3})$/.exec(body);
      if (status) return void res.writeHead(Number(status[1])).end('busy');
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.end(body);
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const home = await mkdtemp(join(tmpdir(), 'tango-json-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-json-work-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_KEY', cost: { input: 1, output: 2 } }
  ]));
  if (config) await writeFile(join(home, 'config.json'), JSON.stringify(config));
  const env: NodeJS.ProcessEnv = { ...process.env, TANGO_HOME: home, MOCK_KEY: 'k' };
  for (const k of ['HTTP_PROXY', 'http_proxy', 'HTTPS_PROXY', 'https_proxy', 'TANGO_MODEL']) delete env[k];
  try {
    const child = spawn(process.execPath, ['--import', 'tsx', 'src/cli.ts', '-m', 'mock/mock', '--cwd', work, ...args], {
      cwd: join(import.meta.dirname, '..'), env, stdio: ['ignore', 'pipe', 'pipe']
    });
    let out = '';
    let err = '';
    child.stdout.on('data', (d) => (out += d));
    child.stderr.on('data', (d) => (err += d));
    const code = await new Promise<number | null>((r) => child.on('close', r));
    const json = args.includes('--format') && args[args.indexOf('--format') + 1] === 'json';
    return { code, out, err, lines: json && out.trim() ? out.trim().split('\n').map((l) => JSON.parse(l)) : [], requests: n };
  } finally {
    server.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
}

test('--format json: every stdout line is an event; tool_use carries timing, step_finish usage', async () => {
  const r = await run([toolCall('c1', 'echo json-ok'), answer('done')], ['--format', 'json', 'run echo']);
  assert.equal(r.code, 0, r.err);
  const types = r.lines.map((l) => l.type);
  assert.deepEqual(types, ['start', 'step_start', 'step_finish', 'tool_use', 'step_start', 'text', 'step_finish', 'done']);
  const [start] = r.lines;
  assert.equal(start.model, 'mock/mock');
  assert.ok(start.session);
  assert.match(start.cwd, /tango-json-work-/, 'the --cwd directory, not where tango was launched');
  const tool = r.lines.find((l) => l.type === 'tool_use');
  assert.equal(tool.name, 'bash');
  assert.deepEqual(tool.input, { command: 'echo json-ok' });
  assert.match(tool.output, /json-ok/);
  assert.equal(tool.isError, false);
  assert.equal(typeof tool.ms, 'number');
  assert.equal(r.lines[2].usage.input, 30);
  assert.equal(r.lines.find((l) => l.type === 'text').text, 'done');
  const done = r.lines.at(-1);
  assert.equal(done.stopReason, 'stop');
  assert.equal(done.steps, 2);
  assert.equal(done.toolCalls, 1);
  assert.deepEqual(done.usage, { input: 80, output: 10, cacheRead: 0, cacheWrite: 0 });
  assert.equal(done.session, start.session);
});

test('--max-tool-rounds caps the loop; json reports an error event and exit 1', async () => {
  const r = await run([toolCall('c1', 'true')], ['--format', 'json', '--max-tool-rounds', '2', 'loop forever']);
  assert.equal(r.code, 1);
  assert.equal(r.requests, 2, 'exactly 2 model rounds');
  const err = r.lines.find((l) => l.type === 'error');
  assert.match(err.message, /上限 2 次/);
  assert.equal(r.lines.at(-1).type, 'done');
  assert.equal(r.lines.at(-1).stopReason, 'error');
});

test('bad values are rejected before anything runs', async () => {
  for (const args of [['--format', 'xml', 'hi'], ['--max-tool-rounds', '0', 'hi'], ['--max-tool-rounds', 'abc', 'hi'], ['--format', 'json']]) {
    const r = await run([answer('x')], args);
    assert.equal(r.code, 1, args.join(' '));
    assert.equal(r.requests, 0, args.join(' '));
    assert.match(r.err, /--format|--max-tool-rounds/);
  }
});

test('text mode (default) is unchanged and also honours --max-tool-rounds', async () => {
  const plain = await run([answer('plain hello')], ['say hi']);
  assert.equal(plain.code, 0);
  assert.match(plain.out, /plain hello/);
  const capped = await run([toolCall('c1', 'true')], ['--max-tool-rounds', '1', 'loop']);
  assert.equal(capped.requests, 1);
  assert.equal(capped.code, 1);
});

test('--format json: a failed request is retried with a retry line; config retries caps it', async () => {
  const ok = await run(['HTTP 503', answer('after retry')], ['--format', 'json', 'hi'], { retries: 1 });
  assert.equal(ok.code, 0, ok.err);
  assert.equal(ok.requests, 2);
  const retry = ok.lines.find((l) => l.type === 'retry');
  assert.deepEqual({ ...retry, delayMs: undefined }, { type: 'retry', step: 1, attempt: 1, max: 1, reason: 'HTTP 503', delayMs: undefined });
  assert.ok(ok.lines.some((l) => l.type === 'text' && l.text === 'after retry'));

  const gaveUp = await run(['HTTP 503'], ['--format', 'json', 'hi'], { retries: 1 });
  assert.equal(gaveUp.code, 1);
  assert.equal(gaveUp.requests, 2, 'one try plus the configured single retry');
  assert.match(gaveUp.lines.find((l) => l.type === 'error')?.message ?? '', /HTTP 503[\s\S]*gave up after 1 retry/);
});
