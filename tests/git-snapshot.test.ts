import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, writeFile, readFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { takeSnapshot, snapshotFileList } from '../src/core/git-snapshot.js';

/** Fresh git repo helper: init, identity, one commit, returns repo path. */
async function freshRepo(): Promise<string> {
  const repo = await mkdtemp(join(tmpdir(), 'tango-snap-'));
  const git = (args: string[]) => spawnSync('git', args, { cwd: repo, encoding: 'utf8' });
  git(['init', '-q']);
  git(['config', 'user.email', 't@tango.dev']);
  git(['config', 'user.name', 'tango test']);
  await writeFile(join(repo, 'tracked.txt'), 'v1\n');
  await writeFile(join(repo, '.gitignore'), 'ignored.txt\n');
  git(['add', '.']);
  git(['commit', '-q', '-m', 'init']);
  return repo;
}

test('takeSnapshot captures untracked files, skips ignored, never touches user branch or index', async () => {
  const repo = await freshRepo();
  try {
    const headBefore = spawnSync('git', ['rev-parse', 'HEAD'], { cwd: repo, encoding: 'utf8' }).stdout.trim();
    await writeFile(join(repo, 'untracked.txt'), 'new\n');
    await writeFile(join(repo, 'ignored.txt'), 'secret\n');

    const hash = await takeSnapshot(repo);
    assert.ok(hash && !hash.startsWith('tar:'), 'git repo must produce a commit hash');

    // user branch and index untouched
    assert.equal(
      spawnSync('git', ['rev-parse', 'HEAD'], { cwd: repo, encoding: 'utf8' }).stdout.trim(),
      headBefore
    );
    const status = spawnSync('git', ['status', '--porcelain'], { cwd: repo, encoding: 'utf8' }).stdout;
    assert.ok(status.includes('untracked.txt'), 'untracked file stays untracked for the user');

    // snapshot content: tracked + untracked, no ignored
    const files = await snapshotFileList(repo, hash);
    assert.ok(files.has('tracked.txt'));
    assert.ok(files.has('untracked.txt'));
    assert.ok(!files.has('ignored.txt'));

    // the hash is a real commit object
    const type = spawnSync('git', ['cat-file', '-t', hash], { cwd: repo, encoding: 'utf8' }).stdout.trim();
    assert.equal(type, 'commit');
  } finally {
    await rm(repo, { recursive: true, force: true });
  }
});

test('snapshot taken from a subdirectory still covers the whole repository', async () => {
  const repo = await freshRepo();
  try {
    await mkdir(join(repo, 'sub'));
    await writeFile(join(repo, 'sub', 'nested.txt'), 'nested\n');
    const hash = await takeSnapshot(join(repo, 'sub'));
    assert.ok(hash && !hash.startsWith('tar:'));
    const files = await snapshotFileList(repo, hash);
    assert.ok(files.has('tracked.txt'), 'repo-root file present in a snapshot taken from a subdir');
    assert.ok(files.has('sub/nested.txt'));
  } finally {
    await rm(repo, { recursive: true, force: true });
  }
});

test('non-git directories fall back to a tar snapshot', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-tar-'));
  try {
    await writeFile(join(dir, 'plain.txt'), 'hello\n');
    const snap = await takeSnapshot(dir);
    assert.ok(snap?.startsWith('tar:'), `expected tar: id, got ${snap}`);
    const files = await snapshotFileList(dir, snap!);
    assert.ok(files.has('plain.txt'));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
