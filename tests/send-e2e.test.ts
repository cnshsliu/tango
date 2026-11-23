import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createServer } from 'node:http';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { createInterface } from 'node:readline';

/** Two live `tango serve` processes talking through /send: both stay alive, neither auto-replies. */

const SSE = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;
const plain = SSE({ choices: [{ delta: { content: 'ok' } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';

function startServe(home: string, cwd: string) {
  const env: NodeJS.ProcessEnv = { ...process.env, TANGO_HOME: home, MOCK_SERVE_KEY: 'sk-test' };
  for (const k of ['HTTP_PROXY', 'http_proxy', 'HTTPS_PROXY', 'https_proxy', 'TANGO_RESUME', 'TANGO_MODE', 'TANGO_MODEL']) delete env[k];
  const child = spawn(process.execPath, ['--import', 'tsx', 'src/cli.ts', 'serve', '-m', 'mock/mock', '--cwd', cwd], {
    cwd: join(import.meta.dirname, '..'),
    env
  }) as ChildProcessWithoutNullStreams;
  const frames: any[] = [];
  createInterface({ input: child.stdout }).on('line', (l) => frames.push(JSON.parse(l)));
  // waits for a frame matching pred, consuming everything up to it
  const until = (pred: (f: any) => boolean, what: string, ms = 15_000) =>
    new Promise<any>((resolve, reject) => {
      const t0 = Date.now();
      const iv = setInterval(() => {
        const i = frames.findIndex(pred);
        const err = frames.find((f) => f.type === 'error');
        if (err) { clearInterval(iv); return reject(new Error(`server error: ${err.message}`)); }
        if (i >= 0) { clearInterval(iv); return resolve(frames.splice(0, i + 1)[i]); }
        if (Date.now() - t0 > ms) { clearInterval(iv); reject(new Error(`timeout waiting for ${what}`)); }
      }, 25);
    });
  const send = (cmd: object) => child.stdin.write(JSON.stringify(cmd) + '\n');
  return { child, send, until, frames };
}

test('send: A mails live B, B gets it while idle, it reaches B\'s next prompt, no auto-reply', async () => {
  const requests: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => { requests.push(JSON.parse(raw)); res.writeHead(200, { 'content-type': 'text/event-stream' }); res.end(plain); });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const home = await mkdtemp(join(tmpdir(), 'send-e2e-home-'));
  const da = await mkdtemp(join(tmpdir(), 'send-e2e-a-'));
  const db = await mkdtemp(join(tmpdir(), 'send-e2e-b-'));
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200000 }
  ]));
  const A = startServe(home, da), B = startServe(home, db);
  try {
    const [ha, hb] = await Promise.all([A.until((f) => f.type === 'hello', 'A hello'), B.until((f) => f.type === 'hello', 'B hello')]);
    assert.notEqual(ha.sessionId, hb.sessionId);

    A.send({ id: 10, cmd: 'send', session: hb.sessionId.slice(0, 8), text: '测试跑完了，可以合并了' });
    const r = await A.until((f) => f.type === 'result' && f.id === 10, 'send result');
    assert.equal(r.ok, true, r.error);
    assert.equal(r.data.to, hb.sessionId);
    assert.ok(r.data.targetLive, 'B is open, so the sender is told it is live');

    // B idle: the 2s poll shows it without any prompt
    const note = await B.until((f) => f.type === 'notice' && String(f.text).includes('✉'), 'B mail notice', 6000);
    assert.match(note.text, /测试跑完了/);
    assert.match(note.text, new RegExp(ha.sessionId.slice(0, 8)));
    assert.equal(requests.length, 0, 'receiving mail must not start a turn (no auto-reply)');

    B.send({ id: 11, cmd: 'history' });
    const h = await B.until((f) => f.type === 'result' && f.id === 11, 'history');
    const mail = h.data.items.find((x: any) => x.kind === 'mail');
    assert.deepEqual({ from: mail.from, text: mail.text }, { from: ha.sessionId, text: '测试跑完了，可以合并了' });

    // B's next prompt carries the message in the model context
    B.send({ id: 12, cmd: 'prompt', text: '继续' });
    await B.until((f) => f.type === 'result' && f.id === 12, 'B turn', 20_000);
    const body = JSON.stringify(requests.at(-1));
    assert.match(body, /message from session/);
    assert.match(body, /测试跑完了/);

    // reply path: B → A works the same way, both stay alive
    B.send({ id: 13, cmd: 'send', session: ha.sessionId, text: '收到' });
    assert.equal((await B.until((f) => f.type === 'result' && f.id === 13, 'reply')).ok, true);
    await A.until((f) => f.type === 'notice' && String(f.text).includes('收到'), 'A mail notice', 6000);

    // sending to yourself is refused
    A.send({ id: 14, cmd: 'send', session: ha.sessionId, text: 'x' });
    assert.equal((await A.until((f) => f.type === 'result' && f.id === 14, 'self')).ok, false);
  } finally {
    A.child.kill(); B.child.kill();
    server.close();
    await rm(home, { recursive: true, force: true });
    await rm(da, { recursive: true, force: true });
    await rm(db, { recursive: true, force: true });
  }
});
