import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createServer } from 'node:http';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';

// Permission rules used to be the snapshot a session started with: an "always allow" saved in
// another window, or a /cd into a project that has rules, changed nothing until a restart —
// and the dialog kept asking for things that were already allowed on disk.

const home = await mkdtemp(join(tmpdir(), 'tango-home-'));
process.env.TANGO_HOME = home;
const SSE = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;
const RULE = JSON.stringify({ permissions: { allow: ['bash:rm'] } }, null, 2);

// the stand-in model: every odd call asks to run `rm -f marker` (risky: "deletes files"), every
// even one answers
let calls = 0;
const mock = createServer((req, res) => {
  req.on('data', () => {});
  req.on('end', () => {
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    res.end(
      ++calls % 2
        ? SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: `c${calls}`, function: { name: 'bash', arguments: JSON.stringify({ command: 'rm -f marker' }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n'
        : SSE({ choices: [{ delta: { content: '好' } }] }) +
            SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 10, completion_tokens: 1, total_tokens: 11 } }) +
            'data: [DONE]\n\n'
    );
  });
});
await new Promise<void>((r) => mock.listen(0, '127.0.0.1', r));
await writeFile(
  join(home, 'models.json'),
  JSON.stringify([
    {
      id: 'mock/mock',
      provider: 'mock',
      model: 'mock-model',
      api: 'openai-completions',
      baseUrl: `http://127.0.0.1:${(mock.address() as AddressInfo).port}/v1`,
      apiKey: 'MOCK_SERVE_KEY',
      contextWindow: 200000
    }
  ])
);

test.after(async () => {
  mock.close();
  await rm(home, { recursive: true, force: true });
});

function startServe(cwd: string) {
  const env: NodeJS.ProcessEnv = { ...process.env, TANGO_HOME: home, MOCK_SERVE_KEY: 'sk-test' };
  for (const k of ['TANGO_RESUME', 'TANGO_MODE', 'TANGO_MODEL', 'TANGO_LAUNCH_NOTICE']) delete env[k];
  // `--import tsx` is resolved against the working directory: load it by absolute path
  const tsx = pathToFileURL(join(import.meta.dirname, '..', 'node_modules', 'tsx', 'dist', 'loader.mjs')).href;
  const child = spawn(process.execPath, ['--import', tsx, join(import.meta.dirname, '..', 'src', 'cli.ts'), 'serve', '-m', 'mock/mock', '--cwd', cwd], {
    cwd: join(import.meta.dirname, '..'),
    env
  }) as ChildProcessWithoutNullStreams;
  const frames: any[] = [];
  const waiters: ((f: any) => void)[] = [];
  createInterface({ input: child.stdout }).on('line', (line) => {
    try {
      const frame = JSON.parse(line);
      (waiters.shift() ?? ((f: any) => frames.push(f)))(frame);
    } catch {}
  });
  const next = () =>
    new Promise<any>((resolve) => {
      if (frames.length) return resolve(frames.shift());
      waiters.push(resolve);
      setTimeout(() => resolve({ type: 'timeout' }), 20_000).unref?.();
    });
  const until = async (pred: (f: any) => boolean) => {
    for (let i = 0; i < 4000; i++) {
      const f = await next();
      if (pred(f)) return f;
    }
    return { type: 'timeout' };
  };
  return { child, until, send: (o: unknown) => child.stdin.write(JSON.stringify(o) + '\n') };
}
type Serve = ReturnType<typeof startServe>;

/** One prompt in `dir`: did the dialog ask, and did the command actually run (the marker is gone)? */
async function turn(s: Serve, dir: string, id: number) {
  await writeFile(join(dir, 'marker'), 'x');
  calls = 0;
  s.send({ id, cmd: 'prompt', text: '删一下' });
  let asked = false;
  let toolRan = false;
  lastTool = '';
  await s.until((f) => {
    if (f.type === 'permission_request') {
      asked = true;
      s.send({ cmd: 'permission', request: f.request, allow: false });
    }
    if (f.type === 'tool_result') {
      if (f.isError === false) toolRan = true;
      lastTool = `${f.name}: ${String(f.output ?? '').slice(0, 200)}`;
    }
    return f.type === 'result' && f.id === id;
  });
  await new Promise((r) => setTimeout(r, 150));
  return { asked, toolRan, markerGone: !existsSync(join(dir, 'marker')) };
}

/** What the last tool of the last turn said — part of every assertion message here. */
let lastTool = '';
const withTool = (msg: string) => `${msg}${lastTool ? ` · last tool: ${lastTool}` : ''}`;

async function project(rules?: string) {
  const dir = await mkdtemp(join(tmpdir(), 'tango-proj-'));
  await mkdir(join(dir, '.tango'));
  if (rules) await writeFile(join(dir, '.tango', 'config.local.json'), rules);
  return dir;
}

test("a rule saved by another window applies to a session that is already open", async () => {
  const work = await project();
  const s = startServe(work);
  try {
    assert.equal((await s.until((f) => f.type === 'hello')).type, 'hello');
    assert.deepEqual(await turn(s, work, 1), { asked: true, toolRan: false, markerGone: false }, withTool('no rule yet: asks'));

    // another window's "always allow": the file changes under the running session
    await writeFile(join(work, '.tango', 'config.local.json'), RULE);
    assert.deepEqual(await turn(s, work, 2), { asked: false, toolRan: true, markerGone: true }, withTool('picked up without a restart'));

    // and a rule taken away is gone just as fast
    await writeFile(join(work, '.tango', 'config.local.json'), JSON.stringify({ permissions: { allow: [] } }, null, 2));
    assert.deepEqual(await turn(s, work, 3), { asked: true, toolRan: false, markerGone: false }, withTool('asks again once the rule is removed'));
  } finally {
    s.child.kill();
    await rm(work, { recursive: true, force: true });
  }
});

test('after /cd the rules of the directory the session is in now apply', async () => {
  const plain = await mkdtemp(join(tmpdir(), 'tango-plain-'));
  const withRules = await project(RULE);
  const s = startServe(plain);
  try {
    assert.equal((await s.until((f) => f.type === 'hello')).type, 'hello');
    assert.deepEqual(await turn(s, plain, 1), { asked: true, toolRan: false, markerGone: false }, withTool('a directory without rules: asks'));

    s.send({ id: 10, cmd: 'cd', dir: withRules });
    const cd = await s.until((f) => f.type === 'result' && f.id === 10);
    assert.equal(cd.ok, true, JSON.stringify(cd));
    assert.deepEqual(await turn(s, withRules, 2), { asked: false, toolRan: true, markerGone: true }, "the new directory's rule applies");

    // /permissions shows what is in force now
    s.send({ id: 11, cmd: 'permissions_list' });
    const list = await s.until((f) => f.type === 'result' && f.id === 11);
    assert.ok(JSON.stringify(list.data).includes('bash:rm'), JSON.stringify(list.data));

    // back in the plain directory the rule no longer applies
    s.send({ id: 12, cmd: 'cd', dir: plain });
    assert.equal((await s.until((f) => f.type === 'result' && f.id === 12)).ok, true);
    assert.deepEqual(await turn(s, plain, 3), { asked: true, toolRan: false, markerGone: false }, "the old directory's lack of rules applies again");
  } finally {
    s.child.kill();
    await rm(plain, { recursive: true, force: true });
    await rm(withRules, { recursive: true, force: true });
  }
});
