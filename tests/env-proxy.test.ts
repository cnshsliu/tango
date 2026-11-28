import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createServer } from 'node:http';
import { connect } from 'node:net';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { applyEnvProxy, proxyPlan, proxyStatus, watchLauncher } from '../src/core/env-proxy.js';

const ROOT = join(import.meta.dirname, '..');
// only the proxy variables under test, and none of what a tango that started this test passes on
const NO_PROXY_VARS = {
  HTTP_PROXY: undefined, http_proxy: undefined, HTTPS_PROXY: undefined, https_proxy: undefined, NO_PROXY: undefined, no_proxy: undefined, ALL_PROXY: undefined, all_proxy: undefined,
  NODE_USE_ENV_PROXY: undefined, NODE_OPTIONS: undefined, TANGO_RESUME: undefined, TANGO_MODE: undefined, TANGO_MODEL: undefined
};
const temps: string[] = [];
after(() => { for (const d of temps.splice(0)) rmSync(d, { recursive: true, force: true }); });
const tmp = (prefix: string) => { const d = mkdtempSync(join(tmpdir(), prefix)); temps.push(d); return d; };
const alive = (pid: number) => { try { process.kill(pid, 0); return true; } catch { return false; } };

test('proxyPlan: what to do for each Node version', () => {
  const set = { HTTP_PROXY: 'http://127.0.0.1:7890' };
  assert.equal(proxyPlan({}, '22.23.5', false), 'off', 'no proxy variables: nothing to do');
  assert.equal(proxyPlan(set, '22.23.5', false), 'relaunch', 'Node 22.21+ reads the switch only at startup');
  assert.equal(proxyPlan(set, '22.21.0', false), 'relaunch', 'the first 22 that reads it');
  assert.equal(proxyPlan(set, '22.20.0', false), 'unsupported', 'before that node ignores the variables');
  assert.equal(proxyPlan(set, '20.18.0', false), 'unsupported');
  assert.equal(proxyPlan(set, '24.0.0', false), 'relaunch');
  assert.equal(proxyPlan(set, '25.4.0', true), 'runtime', 'a node with http.setGlobalProxyFromEnv does it in place');
  assert.equal(proxyPlan({ ...set, NODE_USE_ENV_PROXY: '1' }, '22.23.5', false), 'off', 'on already: this is the relaunched copy');
  assert.equal(proxyPlan({ https_proxy: 'http://127.0.0.1:1' }, '22.23.5', false), 'relaunch', 'lowercase counts too');
});

test('proxyStatus: says how the variables are handled, and never shows credentials', () => {
  assert.equal(proxyStatus({}), 'off (no http(s)_proxy set)');
  assert.match(proxyStatus({ HTTP_PROXY: 'http://127.0.0.1:7890' }), /^NODE_USE_ENV_PROXY needed \(HTTP_PROXY=http:\/\/127\.0\.0\.1:7890\)$/);
  assert.equal(proxyStatus({ HTTP_PROXY: 'http://user:secret@proxy.example:3128', NODE_USE_ENV_PROXY: '1', TANGO_ENV_PROXY_LAUNCHER: '4242' }), 'NODE_USE_ENV_PROXY on, set by tango at start (HTTP_PROXY=http://***@proxy.example:3128)');
  assert.ok(!proxyStatus({ http_proxy: 'http://user:secret@proxy.example:3128', NODE_USE_ENV_PROXY: '1' }).includes('secret'));
  assert.match(proxyStatus({ HTTP_PROXY: 'x', NODE_USE_ENV_PROXY: '1' }), /HTTP_PROXY=set/, 'not a URL: say no more');
});

/** Run `program` (an ES module) through tango's start, with only the given proxy variables. */
function runTango(program: string, env: Record<string, string | undefined>) {
  const dir = tmp('tango-proxy-run-');
  const file = join(dir, 'start.mjs');
  writeFileSync(file, `
    const { applyEnvProxy, watchLauncher } = await import('./src/core/env-proxy.ts');
    watchLauncher();
    const relaunched = await applyEnvProxy();
    if (relaunched !== undefined) { process.exitCode = relaunched; } else {
      ${program}
    }
  `.replace("'./src/core/env-proxy.ts'", JSON.stringify(new URL('../src/core/env-proxy.ts', import.meta.url).href)));
  const child = spawn(process.execPath, ['--import', 'tsx', file], {
    cwd: ROOT,
    env: { ...process.env, ...NO_PROXY_VARS, ...env }
  }) as ChildProcessWithoutNullStreams;
  let out = '';
  let err = '';
  child.stdout.on('data', (b: Buffer) => (out += b));
  child.stderr.on('data', (b: Buffer) => (err += b));
  const exited = new Promise<{ code: number | null; signal: string | null }>((r) => child.on('exit', (code, signal) => r({ code, signal })));
  return { child, exited, text: () => ({ out, err }) };
}

test('relaunch: the copy runs with the switch on, quietly, and its exit status is passed on', { skip: !proxyPlan({ HTTP_PROXY: 'x' }, undefined, false) || proxyPlan({ HTTP_PROXY: 'x' }) !== 'relaunch' }, async () => {
  const r = runTango(
    `console.log(JSON.stringify({ switch: process.env.NODE_USE_ENV_PROXY, launcher: process.env.TANGO_ENV_PROXY_LAUNCHER, ppid: process.ppid, execArgv: process.execArgv })); process.exitCode = 7;`,
    { HTTPS_PROXY: 'http://127.0.0.1:7890' }
  );
  const { code } = await r.exited;
  assert.equal(code, 7, 'the launcher exits with the copy\'s status');
  const { out, err } = r.text();
  assert.equal(err, '', `no "experimental" warning on stderr: ${err}`);
  const seen = JSON.parse(out);
  assert.equal(seen.switch, '1');
  assert.equal(seen.launcher, String(r.child.pid), 'the copy knows its launcher');
  assert.equal(seen.ppid, r.child.pid);
  assert.ok(seen.execArgv.includes('--import') && seen.execArgv.some((a: string) => a.startsWith('--disable-warning')), 'node options travel with the copy');
});

test('relaunch: a signal sent to the launcher reaches the copy, which can clean up first', { skip: process.platform === 'win32' ? 'Windows has no signals to send the launcher' : proxyPlan({ HTTP_PROXY: 'x' }) !== 'relaunch' }, async () => {
  const dir = tmp('tango-proxy-signal-');
  const note = join(dir, 'note');
  const r = runTango(
    `const fs = await import('node:fs');
     process.on('SIGTERM', () => { fs.writeFileSync(${JSON.stringify(note)}, 'copy got SIGTERM'); process.exit(0); });
     console.log('up'); setInterval(() => {}, 1000);`,
    { HTTP_PROXY: 'http://127.0.0.1:7890' }
  );
  // the copy's line comes through the launcher's inherited stdout
  for (let i = 0; i < 100 && !r.text().out.includes('up'); i++) await new Promise((ok) => setTimeout(ok, 50));
  assert.match(r.text().out, /up/);
  r.child.kill('SIGTERM'); // what the TUI and launchd do: signal the process they started
  const { code } = await r.exited;
  assert.equal(code, 0, 'the launcher leaves with the copy\'s status');
  assert.match(readFileSync(note, 'utf8'), /^copy got SIGTERM/);
});

test('relaunch: a copy whose launcher is killed outright leaves on its own', { skip: proxyPlan({ HTTP_PROXY: 'x' }) !== 'relaunch' }, async () => {
  const pidFile = join(tmp('tango-proxy-orphan-'), 'pid');
  const r = runTango(
    `const fs = await import('node:fs'); fs.writeFileSync(${JSON.stringify(pidFile)}, String(process.pid)); setInterval(() => {}, 1000);`,
    { HTTP_PROXY: 'http://127.0.0.1:7890' }
  );
  for (let i = 0; i < 100 && !existsSync(pidFile); i++) await new Promise((ok) => setTimeout(ok, 50));
  const copy = Number(readFileSync(pidFile, 'utf8'));
  assert.ok(alive(copy), 'the copy is running');
  r.child.kill('SIGKILL'); // nothing can be passed on any more
  await r.exited;
  for (let i = 0; i < 80 && alive(copy); i++) await new Promise((ok) => setTimeout(ok, 100));
  assert.equal(alive(copy), false, 'the copy noticed it was orphaned and exited');
});

/** A CONNECT proxy that only tunnels and remembers what it was asked for. */
async function tunnelProxy() {
  const seen: string[] = [];
  const server = createServer((req, res) => { res.writeHead(502); res.end(); });
  server.on('connect', (req, client, head) => {
    seen.push(req.url ?? '');
    const [host, port] = (req.url ?? '').split(':');
    const up = connect(Number(port) || 443, host, () => {
      client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      up.write(head);
      up.pipe(client);
      client.pipe(up);
    });
    up.on('error', () => client.destroy());
    client.on('error', () => up.destroy());
  });
  await new Promise<void>((ok) => server.listen(0, '127.0.0.1', ok));
  return { seen, port: (server.address() as AddressInfo).port, close: () => server.close() };
}

const sse = (text: string) => `data: ${JSON.stringify({ choices: [{ delta: { content: text }, finish_reason: 'stop' }] })}\n\ndata: [DONE]\n\n`;

test("end to end: a model request goes through http(s)_proxy (and no_proxy still bypasses), where node alone ignored it", { skip: proxyPlan({ HTTP_PROXY: 'x' }) !== 'relaunch' && proxyPlan({ HTTP_PROXY: 'x' }) !== 'runtime' }, async () => {
  const hits: string[] = [];
  const model = createServer((req, res) => {
    hits.push('model');
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    res.end(sse('proxied-ok'));
  });
  await new Promise<void>((ok) => model.listen(0, '127.0.0.1', ok));
  const modelPort = (model.address() as AddressInfo).port;
  const proxy = await tunnelProxy();
  const home = tmp('tango-proxy-home-');
  const work = tmp('tango-proxy-work-');
  writeFileSync(join(home, 'models.json'), JSON.stringify([{ id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${modelPort}/v1`, apiKey: 'k', contextWindow: 8000 }]));
  const tango = (proxyEnv: Record<string, string | undefined>) => {
    const child = spawn(process.execPath, ['--import', 'tsx', 'src/cli.ts', '-m', 'mock/mock', '--cwd', work, 'say ok'], {
      cwd: ROOT,
      env: { ...process.env, ...NO_PROXY_VARS, TANGO_HOME: home, ...proxyEnv }
    });
    let out = '';
    let err = '';
    child.stdout.on('data', (b: Buffer) => (out += b));
    child.stderr.on('data', (b: Buffer) => (err += b));
    const done = new Promise<{ code: number | null; out: string; err: string }>((r) => child.on('exit', (code) => r({ code, out, err })));
    return done;
  };
  try {
    const through = await tango({ HTTP_PROXY: `http://127.0.0.1:${proxy.port}` });
    assert.equal(through.code, 0, through.err);
    assert.match(through.out, /proxied-ok/, 'the answer came back through the tunnel');
    assert.deepEqual(proxy.seen, [`127.0.0.1:${modelPort}`], 'the model request was tunnelled through the proxy');
    assert.doesNotMatch(through.err, /UNDICI-EHPA|Warning/, `the switch's warning stays out of stderr: ${through.err}`);
    // no_proxy is honoured by the switch too: a local model listed there goes direct
    proxy.seen.length = 0;
    hits.length = 0;
    const bypassed = await tango({ HTTP_PROXY: `http://127.0.0.1:${proxy.port}`, NO_PROXY: '127.0.0.1' });
    assert.equal(bypassed.code, 0, bypassed.err);
    assert.deepEqual(proxy.seen, [], 'nothing reached the proxy');
    assert.deepEqual(hits, ['model'], 'the model was still reached');
  } finally {
    proxy.close();
    model.close();
  }
});

test('watchLauncher: nothing to do outside a relaunched copy', () => {
  assert.doesNotThrow(() => watchLauncher({}));
  assert.doesNotThrow(() => watchLauncher({ TANGO_ENV_PROXY_LAUNCHER: 'not-a-pid' }));
});

test("serve: started the way the TUI starts it (pipes, proxy variables set), the protocol still works and a stop reaches the real serve", { skip: proxyPlan({ HTTP_PROXY: 'x' }) !== 'relaunch' }, async () => {
  const home = tmp('tango-proxy-serve-home-');
  const work = tmp('tango-proxy-serve-work-');
  const model = createServer((_req, res) => { res.writeHead(500); res.end(); });
  await new Promise<void>((ok) => model.listen(0, '127.0.0.1', ok));
  writeFileSync(join(home, 'models.json'), JSON.stringify([{ id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(model.address() as AddressInfo).port}/v1`, apiKey: 'k', contextWindow: 8000 }]));
  const child = spawn(process.execPath, ['--import', 'tsx', 'src/cli.ts', 'serve', '-m', 'mock/mock', '--cwd', work], {
    cwd: ROOT,
    env: { ...process.env, ...NO_PROXY_VARS, TANGO_HOME: home, HTTP_PROXY: 'http://127.0.0.1:9' } // unreachable on purpose: nothing is requested here
  }) as ChildProcessWithoutNullStreams;
  let out = '';
  let err = '';
  child.stdout.on('data', (b: Buffer) => (out += b));
  child.stderr.on('data', (b: Buffer) => (err += b));
  const line = async () => {
    for (let i = 0; i < 400 && !out.includes('\n'); i++) await new Promise((ok) => setTimeout(ok, 25));
    const i = out.indexOf('\n');
    assert.ok(i > 0, `no frame arrived; stderr: ${err}`);
    const frame = JSON.parse(out.slice(0, i));
    out = out.slice(i + 1);
    return frame;
  };
  try {
    const hello = await line();
    assert.equal(hello.type, 'hello', 'the relaunched serve owns the pipes');
    const lock = join(home, 'locks', 'sessions', `${hello.sessionId}.json`);
    for (let i = 0; i < 100 && !existsSync(lock); i++) await new Promise((ok) => setTimeout(ok, 20));
    assert.ok(existsSync(lock), 'the serve holds its session lock');
    assert.equal(JSON.parse(readFileSync(lock, 'utf8')).pid, r_childPid(lock), 'the lock names the serve, not its launcher');
    // what the TUI does on /quit and /restart: SIGTERM to the process it started
    child.kill('SIGTERM');
    const code = await new Promise<number | null>((r) => child.on('exit', (c) => r(c)));
    assert.equal(code, 0, `clean stop (stderr: ${err})`);
    for (let i = 0; i < 100 && existsSync(lock); i++) await new Promise((ok) => setTimeout(ok, 20));
    assert.equal(existsSync(lock), false, 'the serve cleaned up before leaving: the signal reached it');
  } finally {
    child.kill();
    model.close();
  }
  function r_childPid(_lock: string) {
    return Number(readFileSync(_lock, 'utf8').match(/"pid":(\d+)/)?.[1]);
  }
});
