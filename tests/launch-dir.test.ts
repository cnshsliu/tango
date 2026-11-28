import { test } from 'node:test';
import assert from 'node:assert/strict';
import { realpathSync } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

// A terminal still sitting in a deleted directory (a removed worktree) starts tango without a
// working directory. node's cwd() then throws and serve used to die at startup.

const { rescueLaunchDir } = await import('../src/core/launch-dir.js');

const fakeHome = await mkdtemp(join(tmpdir(), 'tango-fakehome-'));
// process.cwd() answers with the real path (macOS: /var → /private/var)
const realFakeHome = realpathSync(fakeHome);
const realCwd = process.cwd();
const realCwdFn = process.cwd;
const realHome = process.env.HOME;
const notice = () => rescueLaunchDir();

test.after(async () => {
  process.cwd = realCwdFn;
  if (realHome === undefined) delete process.env.HOME;
  else process.env.HOME = realHome;
  process.chdir(realCwd);
  await rm(fakeHome, { recursive: true, force: true });
});

test('launch directory fine: no move, no notice', () => {
  assert.equal(notice(), null);
  assert.equal(process.cwd(), realCwd);
});

test('launch directory deleted (macOS: cwd() throws): move to the home directory and say so', { skip: process.platform === 'win32' && 'POSIX cwd semantics' }, () => {
  process.env.HOME = fakeHome;
  process.cwd = (() => {
    throw Object.assign(new Error('ENOENT: no such file or directory, uv_cwd'), { code: 'ENOENT' });
  }) as typeof process.cwd;
  const text = notice();
  assert.ok(text, 'a notice is returned');
  assert.ok(text.includes('已不存在的目录里启动'), `says what happened: ${text}`);
  assert.ok(!text.includes('改用'), `what is used instead is the caller's to say: ${text}`);
  process.cwd = realCwdFn;
  assert.equal(process.cwd(), realFakeHome, 'the process runs in the home directory from now on');
  // from there everything is ordinary again
  process.chdir(realCwd);
});

test('launch directory deleted (Linux: cwd() still answers with a path that is gone)', { skip: process.platform === 'win32' && 'POSIX cwd semantics' }, () => {
  process.env.HOME = fakeHome;
  process.cwd = () => join(fakeHome, 'gone');
  const text = notice();
  assert.ok(text, 'a notice is returned');
  process.cwd = realCwdFn;
  assert.equal(process.cwd(), realFakeHome);
  process.chdir(realCwd);
});
