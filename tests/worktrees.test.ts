import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

// Removing a worktree a live session works in deletes that session's directory: every prompt
// is refused from then on. worktreeRemove must refuse instead.

const home = await mkdtemp(join(tmpdir(), 'tango-home-'));
process.env.TANGO_HOME = home;
const { sessionsInDir, worktreeCreate, worktreeList, worktreeRemove } = await import('../src/core/worktrees.js');
const { SessionStore } = await import('../src/core/session.js');
const { sessionsDir } = await import('../src/core/config.js');

const sh = (cwd: string, ...args: string[]) => spawnSync('git', args, { cwd, encoding: 'utf8' });
const repo = await mkdtemp(join(tmpdir(), 'tango-repo-'));
sh(repo, 'init', '-q', '-b', 'main');
sh(repo, 'config', 'user.email', 'tango@test');
sh(repo, 'config', 'user.name', 'tango');
await writeFile(join(repo, 'a.txt'), 'a\n');
sh(repo, 'add', '.');
sh(repo, 'commit', '-q', '-m', 'init');

const locks = join(home, 'locks', 'sessions');
await mkdir(locks, { recursive: true });
/** a session lock as serve writes it: the file is the session id, the pid says who holds it */
const lock = (id: string, pid: number) =>
  writeFile(join(locks, `${id}.json`), JSON.stringify({ pid, cwd: '', at: new Date().toISOString() }));
const unlock = (id: string) => rm(join(locks, `${id}.json`), { force: true });
/** what `/cd` appends: a new meta entry, the last one wins */
const cd = (store: SessionStore, cwd: string) =>
  store.append({ kind: 'meta', id: store.id, createdAt: new Date().toISOString(), cwd, name: '在 wt1 里' });

test.after(async () => {
  sh(repo, 'worktree', 'prune', '--expire=now');
  await rm(home, { recursive: true, force: true });
  await rm(repo, { recursive: true, force: true });
});

test('worktreeRemove refuses while a live session works in the worktree, and names it', async () => {
  const entry = await worktreeCreate(repo, 'wt1');
  const id = 'aaaa1111-2222-4333-8444-555566667777';
  const store = await SessionStore.create(sessionsDir(), entry.path, '在 wt1 里', id);

  // no lock: the session isn't open anywhere
  assert.deepEqual(await sessionsInDir(entry.path), []);

  // open in a live process (this test's pid stands in for serve)
  await lock(id, process.pid);
  assert.deepEqual(await sessionsInDir(entry.path), [{ id, pid: process.pid }]);
  await assert.rejects(
    () => worktreeRemove(repo, 'wt1'),
    /worktree "wt1" is in use: session 555566667777 \(pid \d+\) works in it .*\/cd that session/,
    'refused, with the short id and the way out'
  );
  await assert.rejects(() => worktreeRemove(repo, 'wt1', true), /is in use/, 'force does not override it');
  assert.ok(existsSync(entry.path), 'worktree still there');
  assert.ok((await worktreeList()).some((w) => w.name === 'wt1'), 'still registered');

  // the session /cd's out: its latest meta entry is the truth, the lock's cwd is not
  await cd(store, repo);
  assert.deepEqual(await sessionsInDir(entry.path), [], 'moved away → not in the worktree');
  await mkdir(join(entry.path, 'sub'), { recursive: true });
  await cd(store, join(entry.path, 'sub'));
  assert.equal((await sessionsInDir(entry.path)).length, 1, 'a subdirectory counts too');
  // a sibling whose name merely starts with the worktree's path is not inside it
  const sibling = `${entry.path}-x`;
  await mkdir(sibling, { recursive: true });
  await cd(store, sibling);
  assert.deepEqual(await sessionsInDir(entry.path), [], 'wt1-x is not inside wt1');

  // the process is gone: a stale lock counts as closed
  await unlock(id);
  await lock(id, 99999999);
  await cd(store, entry.path);
  assert.deepEqual(await sessionsInDir(entry.path), [], 'stale lock ignored');

  await worktreeRemove(repo, 'wt1');
  assert.ok(!existsSync(entry.path), 'worktree gone');
  assert.deepEqual(await worktreeList(), [], 'unregistered');
  assert.equal(sh(repo, 'branch', '--list', 'tango/wt1').stdout.trim(), '', 'merged branch deleted');
});

test('sessionsInDir: a worktree that is already gone has nobody in it', async () => {
  assert.deepEqual(await sessionsInDir(join(home, 'worktrees', 'never-existed')), []);
});
