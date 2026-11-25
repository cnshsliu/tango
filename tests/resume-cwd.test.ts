import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { createServer } from 'node:http';
import { realpathSync } from 'node:fs';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';

// A session whose directory was deleted (its worktree was removed) used to keep that directory
// when resumed, so nothing could run there. It now runs where tango was started, and says so.

const home = await mkdtemp(join(tmpdir(), 'tango-home-'));
process.env.TANGO_HOME = home;
const { sessionsDir } = await import('../src/core/config.js');
const { SessionStore } = await import('../src/core/session.js');
const { openAgent } = await import('../src/host.js');
await mkdir(sessionsDir(), { recursive: true });

// a local stand-in model: answers every prompt with one word
const SSE = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;
let prompts = 0;
const mock = createServer((req, res) => {
  req.on('data', () => {});
  req.on('end', () => {
    prompts++;
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    res.end(
      SSE({ choices: [{ delta: { content: '好' } }] }) +
        SSE({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 } })
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

const perm = { mode: 'ask' as const };

test('openAgent: a resumed session whose directory is gone runs in the launch directory', async () => {
  // tango is "started" in a directory of its own, away from this repo's project config
  const launch = await mkdtemp(join(tmpdir(), 'tango-launch-'));
  const startCwd = process.cwd();
  process.chdir(launch);
  try {
    const gone = join(home, 'removed-worktree'); // never created
    const store = await SessionStore.create(sessionsDir(), gone, '在已删除的 worktree 里');
    const opened = await openAgent({ resume: store.id, model: 'mock/mock' }, perm);
    try {
      assert.equal(opened.movedFrom, gone, 'the host is told which directory was left');
      assert.equal(opened.cwd, realpathSync(launch));
      assert.equal(opened.agent.cwd, realpathSync(launch));
      // the move is for this run only: the session still records its own directory (/cd makes a
      // move permanent), so the next resume says so again instead of silently living elsewhere
      assert.equal(opened.store.meta()?.cwd, gone);
    } finally {
      await opened.agent.end('exit');
    }

    // a session whose directory exists opens there, untouched
    const there = await mkdtemp(join(tmpdir(), 'tango-there-'));
    const kept = await openAgent({ resume: (await SessionStore.create(sessionsDir(), there, '目录还在')).id, model: 'mock/mock' }, perm);
    try {
      assert.equal(kept.movedFrom, undefined);
      assert.equal(kept.cwd, there);
    } finally {
      await kept.agent.end('exit');
      await rm(there, { recursive: true, force: true });
    }
  } finally {
    process.chdir(startCwd);
    await rm(launch, { recursive: true, force: true });
  }
});

test('serve: resuming a session whose directory is gone → one notice, and a prompt runs', async () => {
  const gone = join(home, 'removed-worktree-2'); // never created
  const store = await SessionStore.create(sessionsDir(), gone, '在已删除的 worktree 里');
  const launch = await mkdtemp(join(tmpdir(), 'tango-launch-'));
  const env: NodeJS.ProcessEnv = { ...process.env, TANGO_HOME: home, MOCK_SERVE_KEY: 'sk-test', TANGO_RESUME: store.id };
  for (const k of ['TANGO_MODE', 'TANGO_MODEL', 'TANGO_LAUNCH_NOTICE']) delete env[k];
  // `--import tsx` is resolved against the working directory, which is not the repo here
  const tsx = pathToFileURL(join(import.meta.dirname, '..', 'node_modules', 'tsx', 'dist', 'loader.mjs')).href;
  const child = spawn(
    process.execPath,
    ['--import', tsx, join(import.meta.dirname, '..', 'src', 'cli.ts'), 'serve', '-m', 'mock/mock'],
    { cwd: launch, env }
  ) as ChildProcessWithoutNullStreams;
  const frames: any[] = [];
  let stderr = '';
  createInterface({ input: child.stdout }).on('line', (line) => {
    try {
      frames.push(JSON.parse(line));
    } catch {
      frames.push({ raw: line });
    }
  });
  createInterface({ input: child.stderr }).on('line', (line) => (stderr += line + '\n'));
  const until = async (pred: (f: any) => boolean) => {
    for (let i = 0; i < 400 && !frames.some(pred) && child.exitCode === null; i++) await new Promise((r) => setTimeout(r, 50));
    return frames.find(pred);
  };
  try {
    const hello: any = await until((f) => f.type === 'hello');
    assert.ok(hello, `hello arrived; stderr=${stderr}`);
    const notice: any = await until((f) => f.type === 'notice');
    assert.equal(hello.cwd, realpathSync(launch), 'the session runs in the launch directory');
    assert.ok(notice, 'the move is announced');
    assert.ok(String(notice.text).includes(`会话目录 ${gone} 已不存在`), notice.text);
    assert.ok(String(notice.text).includes(`本次改用 ${realpathSync(launch)}`), notice.text);
    assert.ok(String(notice.text).includes('/cd'), notice.text);
    assert.equal(frames.filter((f) => f.type === 'notice').length, 1, 'one line, not one per cause');
    // and the session is usable right away
    child.stdin.write(JSON.stringify({ id: 1, cmd: 'prompt', text: '在吗' }) + '\n');
    const result: any = await until((f) => f.type === 'result' && f.id === 1);
    assert.ok(result?.ok, `the prompt ran: ${JSON.stringify(result)} stderr=${stderr}`);
    assert.ok(prompts >= 1, 'the model was reached');
  } finally {
    child.kill();
    await rm(launch, { recursive: true, force: true });
  }
});
