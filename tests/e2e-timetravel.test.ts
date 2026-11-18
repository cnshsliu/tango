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
 * Real-process time-travel acceptance: `tango serve` + mock SSE + a REAL git repo.
 * Covers the plan's Review Focus: user files survive rewind (RF#2), interrupt vs
 * snapshot (RF#1), fork independence (RF#5), export offline readability, snapshot latency.
 */

const SSE = (obj: unknown) => `data: ${JSON.stringify(obj)}\n\n`;

async function startMock() {
  let script: () => string = () => '';
  const requests: any[] = [];
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

async function startServe(home: string, cwd: string) {
  const env: NodeJS.ProcessEnv = { ...process.env, TANGO_HOME: home, MOCK_SERVE_KEY: 'sk-test' };
  for (const k of ['HTTP_PROXY', 'http_proxy', 'HTTPS_PROXY', 'https_proxy', 'TANGO_RESUME', 'TANGO_MODE', 'TANGO_MODEL']) delete env[k];
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
  const next = () => new Promise<any>((resolve, reject) => {
    if (frames.length) return resolve(frames.shift());
    waiters.push(resolve);
    setTimeout(() => reject(new Error('timeout waiting for a frame')), 20_000).unref?.();
  });
  const until = async (pred: (f: any) => boolean, what: string) => {
    for (let i = 0; i < 200; i++) {
      const f = await next();
      if (f.type === 'error') throw new Error(`server error frame: ${f.message}`);
      if (pred(f)) return f;
    }
    throw new Error(`no "${what}" frame`);
  };
  const send = (cmd: object) => child.stdin.write(JSON.stringify(cmd) + '\n');
  return { child, send, next, until };
}

async function freshRepo(): Promise<string> {
  const repo = await mkdtemp(join(tmpdir(), 'tango-e2e-'));
  const g = (args: string[]) => spawnSync('git', args, { cwd: repo, encoding: 'utf8' });
  g(['init', '-q']); g(['config', 'user.email', 't@t']); g(['config', 'user.name', 't']);
  await writeFile(join(repo, 'seed.txt'), 'seed\n'); g(['add', '.']); g(['commit', '-q', '-m', 'init']);
  return repo;
}

const writeTurn = (cmd: string) => SSE({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'w' + Math.random().toString(36).slice(2, 6), function: { name: 'bash', arguments: JSON.stringify({ command: cmd }) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
const plainTurn = (text = 'ok') => SSE({ choices: [{ delta: { content: text } }] }) + SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } }) + 'data: [DONE]\n\n';

test('e2e A+B+D: real undo/redo round-trip, user file survives, abandoned future found', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-e2e-home-'));
  const repo = await freshRepo();
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  let turnNo = 0; let pendingTool = false;
  mock.setScript(() => {
    if (!pendingTool) turnNo++;
    if (!pendingTool && turnNo <= 2) {
      pendingTool = true;
      return writeTurn(turnNo === 1 ? 'echo v1 > work.txt' : 'echo v2 > work.txt && echo agent-leftover > junk.txt');
    }
    pendingTool = false;
    return plainTurn('ABANDONED-ANSWER-' + turnNo);
  });

  const s = await startServe(home, repo);
  try {
    await s.next(); // hello
    for (let i = 0; i < 2; i++) {
      s.send({ id: 100 + i, cmd: 'prompt', text: `turn ${i + 1}` });
      await s.until((f: any) => f.type === 'result' && f.ok, 'turn');
    }
    // B: the user hand-makes a file between turns — no snapshot knows it
    await writeFile(join(repo, 'handmade.txt'), 'user data\n');

    // A: undo the second turn
    s.send({ id: 200, cmd: 'undo' });
    const undo = await s.until((f: any) => f.type === 'result' && f.id === 200, 'undo');
    assert.ok(undo.ok);
    assert.equal((await readFile(join(repo, 'work.txt'), 'utf8')).trim(), 'v1');
    assert.ok(!existsSync(join(repo, 'junk.txt')), 'agent leftover deleted');
    // B: the user's file survives and is reported
    assert.ok(undo.data.kept.includes('handmade.txt'));
    assert.equal(await readFile(join(repo, 'handmade.txt'), 'utf8'), 'user data\n');

    // D: the abandoned future is findable
    s.send({ id: 201, cmd: 'find', text: 'turn 2' });
    const fd = await s.until((f: any) => f.type === 'result' && f.id === 201, 'find');
    assert.ok(fd.data.hits.some((h: any) => h.orphaned === true));

    // A: redo brings turn 2 back (code included)
    s.send({ id: 202, cmd: 'redo' });
    await s.until((f: any) => f.type === 'result' && f.id === 202, 'redo');
    assert.equal((await readFile(join(repo, 'work.txt'), 'utf8')).trim(), 'v2');
    assert.ok(existsSync(join(repo, 'junk.txt')));
    assert.equal(await readFile(join(repo, 'handmade.txt'), 'utf8'), 'user data\n', 'still untouched');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(repo, { recursive: true, force: true });
  }
});

test('e2e C: fork independence — two timelines, one workspace snapshot pool', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-e2e-home-'));
  const repo = await freshRepo();
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  let turnNo = 0; let pendingTool = false;
  mock.setScript(() => {
    if (!pendingTool) turnNo++;
    if (!pendingTool && turnNo <= 1) {
      pendingTool = true;
      return writeTurn('echo base > work.txt');
    }
    pendingTool = false;
    return plainTurn();
  });

  const s = await startServe(home, repo);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: 'turn 1' });
    await s.until((f: any) => f.type === 'result' && f.ok, 'turn');

    s.send({ id: 201, cmd: 'fork', name: 'branch-b' });
    const fk = await s.until((f: any) => f.type === 'result' && f.id === 201, 'fork');
    assert.ok(fk.ok);
    const forkId = fk.data.sessionId as string;

    // original continues; fork (in a second serve instance) rewinds independently
    s.send({ id: 202, cmd: 'timeline' });
    const tl = await s.until((f: any) => f.type === 'result' && f.id === 202, 'timeline');
    const originalNodes = tl.data.nodes;

    const s2env = { ...process.env, TANGO_HOME: home, TANGO_RESUME: forkId, MOCK_SERVE_KEY: 'sk-test' };
    for (const k of ['HTTP_PROXY', 'http_proxy', 'HTTPS_PROXY', 'https_proxy', 'TANGO_MODE', 'TANGO_MODEL']) delete s2env[k as string];
    const s2 = spawn(process.execPath, ['--import', 'tsx', 'src/cli.ts', 'serve', '-m', 'mock/mock', '--cwd', repo], { cwd: join(import.meta.dirname, '..'), env: s2env }) as ChildProcessWithoutNullStreams;
    const frames2: any[] = [];
    const waiters2: ((f: any) => void)[] = [];
    createInterface({ input: s2.stdout }).on('line', (l) => {
      const f = JSON.parse(l);
      const w = waiters2.shift();
      if (w) w(f); else frames2.push(f);
    });
    const until2 = async (pred: (f: any) => boolean, what: string) => {
      for (let i = 0; i < 200; i++) {
        const f = frames2.length ? frames2.shift() : await new Promise<any>((r) => waiters2.push(r));
        if (f.type === 'error') throw new Error(f.message);
        if (pred(f)) return f;
      }
      throw new Error(`no ${what}`);
    };
    try {
      await until2((f: any) => f.type === 'hello', 'hello');
      // rewind the fork to before turn 1
      s2.stdin.write(JSON.stringify({ id: 1, cmd: 'rewind', entryIndex: originalNodes[0].entryIndex }) + '\n');
      const rw = await until2((f: any) => f.type === 'result' && f.id === 1, 'fork rewind');
      assert.ok(rw.ok);
      assert.ok(!existsSync(join(repo, 'work.txt')), 'rewinding the fork clears shared workspace files');
      // the ORIGINAL session's projection is untouched by the fork's rewind
      s.send({ id: 203, cmd: 'history' });
      const h = await s.until((f: any) => f.type === 'result' && f.id === 203, 'history');
      assert.ok(JSON.stringify(h.data.items).includes('turn 1'));
    } finally {
      s2.kill();
    }
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(repo, { recursive: true, force: true });
  }
});

test('e2e E+G: export html is offline-readable; snapshot latency under 100ms', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-e2e-home-'));
  const repo = await freshRepo();
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  let pendingTool = false;
  mock.setScript(() => {
    if (!pendingTool) {
      pendingTool = true;
      return writeTurn('echo hi > a.txt');
    }
    pendingTool = false;
    return plainTurn();
  });

  const s = await startServe(home, repo);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: 'do it' });
    await s.until((f: any) => f.type === 'result' && f.ok, 'turn');
    // the budget is the latency a snapshot ADDS to a turn: time the snapshot call itself
    const { takeSnapshot } = await import('../src/core/git-snapshot.js');
    // median of 3: a single run is noise-sensitive (scheduler, disk cache) and flaked at 107-118ms
    const samples: number[] = [];
    let h: string | null = null;
    for (let i = 0; i < 3; i++) {
      const t0 = Date.now();
      h = await takeSnapshot(repo, 'timing');
      samples.push(Date.now() - t0);
    }
    const snapshotMs = samples.sort((a, b) => a - b)[1];
    assert.ok(h, 'snapshot succeeded');
    assert.ok(snapshotMs < 100, `takeSnapshot median ${snapshotMs}ms over [${samples}] (budget 100ms)`);

    s.send({ id: 201, cmd: 'export', format: 'html' });
    const ex = await s.until((f: any) => f.type === 'result' && f.id === 201, 'export');
    assert.ok(ex.ok);
    const html = await readFile(ex.data.path, 'utf8');
    assert.ok(html.includes('<!doctype html'));
    assert.ok(!/src=["']https?:|href=["']https?:/.test(html), 'no external references — offline readable');
    assert.ok(html.includes('do it'));
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(repo, { recursive: true, force: true });
  }
});

test('e2e F: interrupt during a write turn leaves the rewind chain usable', async () => {
  const mock = await startMock();
  const home = await mkdtemp(join(tmpdir(), 'tango-e2e-home-'));
  const repo = await freshRepo();
  await writeFile(join(home, 'models.json'), JSON.stringify([
    { id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(mock.server.address() as AddressInfo).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 200 }
  ]));
  // turn 1 completes; turn 2's tool call is answered, then we interrupt before the finisher
  let turnNo = 0; let pendingTool = false;
  mock.setScript(() => {
    if (!pendingTool) turnNo++;
    if (!pendingTool && turnNo <= 2) {
      pendingTool = true;
      return writeTurn(turnNo === 1 ? 'echo one > f.txt' : 'echo two > f.txt');
    }
    pendingTool = false;
    return plainTurn();
  });

  const s = await startServe(home, repo);
  try {
    await s.next();
    s.send({ id: 100, cmd: 'prompt', text: 'turn 1' });
    await s.until((f: any) => f.type === 'result' && f.ok, 'turn1');

    s.send({ id: 101, cmd: 'prompt', text: 'turn 2' });
    await s.until((f: any) => f.type === 'tool_result', 'turn2 tool ran (post-interrupt path)');
    s.send({ cmd: 'interrupt' });
    await s.until((f: any) => f.type === 'result' && f.id === 101, 'aborted result');

    // the chain must still rewind cleanly past the aborted turn
    s.send({ id: 102, cmd: 'undo' });
    const undo = await s.until((f: any) => f.type === 'result' && f.id === 102, 'undo');
    assert.ok(undo.ok);
    assert.equal((await readFile(join(repo, 'f.txt'), 'utf8')).trim(), 'one');
    // and redo back over the aborted turn
    s.send({ id: 103, cmd: 'redo' });
    const redo = await s.until((f: any) => f.type === 'result' && f.id === 103, 'redo');
    assert.ok(redo.ok);
    assert.equal((await readFile(join(repo, 'f.txt'), 'utf8')).trim(), 'two');
  } finally {
    s.child.kill();
    mock.server.close();
    await rm(home, { recursive: true, force: true });
    await rm(repo, { recursive: true, force: true });
  }
});
