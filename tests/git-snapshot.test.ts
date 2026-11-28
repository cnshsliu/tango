import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, writeFile, readFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { takeSnapshot, snapshotFileList, restoreSnapshot } from '../src/core/git-snapshot.js';

/** Fresh git repo helper: init, identity, one commit, returns repo path. */
async function freshRepo(): Promise<string> {
  const repo = await mkdtemp(join(tmpdir(), 'tango-snap-'));
  const git = (args: string[]) => spawnSync('git', args, { cwd: repo, encoding: 'utf8' });
  git(['init', '-q', '-b', 'main']);
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
  const tangoHome = await mkdtemp(join(tmpdir(), 'tango-tar-home-'));
  const prevHome = process.env.TANGO_HOME;
  process.env.TANGO_HOME = tangoHome;
  const dir = await mkdtemp(join(tmpdir(), 'tango-tar-'));
  try {
    await writeFile(join(dir, 'plain.txt'), 'hello\n');
    const snap = await takeSnapshot(dir);
    assert.ok(snap?.startsWith('tar:'), `expected tar: id, got ${snap}`);
    const files = await snapshotFileList(dir, snap!);
    assert.ok(files.has('plain.txt'));
  } finally {
    process.env.TANGO_HOME = prevHome;
    await rm(dir, { recursive: true, force: true });
    await rm(tangoHome, { recursive: true, force: true });
  }
});

test('restoreSnapshot: smart delete — agent leftovers removed, user files kept, target content restored', async () => {
  const repo = await freshRepo();
  try {
    await writeFile(join(repo, '.gitignore'), 'ignored.txt\n.env\n');
    const t1 = await takeSnapshot(repo); // v1 world
    assert.ok(t1);

    // the "agent turn": modify tracked file, create a leftover, snapshot after
    await writeFile(join(repo, 'tracked.txt'), 'v2-mutated\n');
    await writeFile(join(repo, 'leftover.txt'), 'agent made this\n');
    const t2 = await takeSnapshot(repo);
    assert.ok(t2);

    // the user's own hand-made file, never snapshotted
    await writeFile(join(repo, 'handmade.txt'), 'user made this\n');
    await writeFile(join(repo, '.env'), 'SECRET=1\n');

    const report = await restoreSnapshot(repo, t1!, [t2!]);

    // target content restored
    assert.equal(await readFile(join(repo, 'tracked.txt'), 'utf8'), 'v1\n');
    // agent leftover deleted
    assert.ok(!existsSync(join(repo, 'leftover.txt')));
    assert.ok(report.deleted.includes('leftover.txt'));
    // user's hand-made file kept on disk and reported
    assert.equal(await readFile(join(repo, 'handmade.txt'), 'utf8'), 'user made this\n');
    assert.ok(report.kept.includes('handmade.txt'), JSON.stringify(report));
    // ignored file untouched
    assert.equal(await readFile(join(repo, '.env'), 'utf8'), 'SECRET=1\n');
  } finally {
    await rm(repo, { recursive: true, force: true });
  }
});

test('restoreSnapshot: tar snapshots get the same smart-delete semantics', async () => {
  const tangoHome = await mkdtemp(join(tmpdir(), 'tango-tar2-home-'));
  const prevHome = process.env.TANGO_HOME;
  process.env.TANGO_HOME = tangoHome;
  const dir = await mkdtemp(join(tmpdir(), 'tango-tar2-'));
  try {
    await writeFile(join(dir, 'a.txt'), 'one\n');
    const t1 = await takeSnapshot(dir);
    await writeFile(join(dir, 'a.txt'), 'one-changed\n');
    await writeFile(join(dir, 'b.txt'), 'agent\n');
    const t2 = await takeSnapshot(dir);
    await writeFile(join(dir, 'mine.txt'), 'user\n');

    const report = await restoreSnapshot(dir, t1!, [t2!]);
    assert.equal(await readFile(join(dir, 'a.txt'), 'utf8'), 'one\n');
    assert.ok(!existsSync(join(dir, 'b.txt')));
    assert.ok(report.kept.includes('mine.txt'), JSON.stringify(report));
    assert.equal(await readFile(join(dir, 'mine.txt'), 'utf8'), 'user\n');
  } finally {
    process.env.TANGO_HOME = prevHome;
    await rm(dir, { recursive: true, force: true });
    await rm(tangoHome, { recursive: true, force: true });
  }
});

test('snapshots hang off a per-session ref chain so gc cannot prune them', async () => {
  const repo = await freshRepo();
  try {
    const h1 = await takeSnapshot(repo, 'sess-a');
    const h2 = await takeSnapshot(repo, 'sess-a');
    assert.ok(h1 && h2 && h1 !== h2);
    const ref = spawnSync('git', ['rev-parse', '--verify', 'refs/tango/checkpoints/sess-a'], { cwd: repo, encoding: 'utf8' });
    assert.equal(ref.status, 0, 'ref exists');
    assert.equal(ref.stdout.trim(), h2, 'ref points at the newest snapshot');
    const parent = spawnSync('git', ['rev-parse', `${h2}^`], { cwd: repo, encoding: 'utf8' });
    assert.equal(parent.status, 0);
    assert.equal(parent.stdout.trim(), h1, 'snapshots chain: h1 is h2 parent');
  } finally {
    await rm(repo, { recursive: true, force: true });
  }
});

test('restoreSnapshot throws on a missing snapshot hash instead of silent no-op', async () => {
  const repo = await freshRepo();
  try {
    await assert.rejects(() => restoreSnapshot(repo, '0'.repeat(40), []));
  } finally {
    await rm(repo, { recursive: true, force: true });
  }
});

test('M4: tar fallback honors .gitignore, keeps exec bits, prunes old snapshots', { skip: process.platform === 'win32' && 'an executable bit is a POSIX thing' }, async () => {
  const tangoHome = await mkdtemp(join(tmpdir(), 'tango-tar3-home-'));
  const prevHome = process.env.TANGO_HOME;
  process.env.TANGO_HOME = tangoHome;
  const dir = await mkdtemp(join(tmpdir(), 'tango-tar3-'));
  try {
    await writeFile(join(dir, 'keep.txt'), 'a\n');
    await writeFile(join(dir, '.gitignore'), 'ignored.txt\n');
    await writeFile(join(dir, 'ignored.txt'), 'x\n');
    await writeFile(join(dir, 'run.sh'), '#!/bin/sh\necho hi\n');
    spawnSync('chmod', ['+x', join(dir, 'run.sh')]);

    const t1 = await takeSnapshot(dir, 'sess-tar');
    assert.ok(t1?.startsWith('tar:'));
    const files = await snapshotFileList(dir, t1!);
    assert.ok(files.has('keep.txt'));
    assert.ok(files.has('run.sh'));
    assert.ok(!files.has('ignored.txt'), 'M4: tar respects .gitignore when present');

    // cleanup policy: only the newest snapshots survive (keep-last-N)
    for (let i = 0; i < 60; i++) await takeSnapshot(dir, 'sess-tar');
    const snapDir = join(tangoHome, 'snapshots', 'sess-tar');
    const { readdirSync, statSync } = await import('node:fs');
    const remaining = readdirSync(snapDir).filter((f) => f.endsWith('.tar.gz'));
    assert.ok(remaining.length <= 50, `M4: snapshot cleanup keeps ≤50, got ${remaining.length}`);

    // restore preserves the executable bit
    await spawnSync('rm', [join(dir, 'run.sh')]);
    const latest = remaining.sort().at(-1)!;
    await restoreSnapshot(dir, `tar:${join(snapDir, latest)}`, []);
    assert.ok(existsSync(join(dir, 'run.sh')));
    const mode = (statSync(join(dir, 'run.sh')).mode & 0o111) !== 0;
    assert.ok(mode, 'M4: restored script keeps its exec bit');
  } finally {
    process.env.TANGO_HOME = prevHome;
    await rm(dir, { recursive: true, force: true });
    await rm(tangoHome, { recursive: true, force: true });
  }
});

test('M5+M6: kept list is bounded; gitlink submodules are reported', async () => {
  const repo = await freshRepo();
  try {
    // a fake submodule: gitlink entry pointing at a commit that exists in a child repo
    const sub = join(repo, 'sub');
    const g = (args: string[], cwd = repo) => spawnSync('git', args, { cwd, encoding: 'utf8' });
    g(['init', '-q', sub]); g(['config', 'user.email', 't@t'], sub); g(['config', 'user.name', 't'], sub);
    await writeFile(join(sub, 'x.txt'), 'x\n');
    g(['add', '.'], sub); g(['commit', '-q', '-m', 's'], sub);
    const subHead = g(['rev-parse', 'HEAD'], sub).stdout.trim();
    g(['add', 'sub']); // stages a gitlink
    // fake an agent change on top
    await writeFile(join(repo, 'top.txt'), 't\n');
    const h1 = await takeSnapshot(repo, 'sess-m56');
    assert.ok(h1);
    const report = await restoreSnapshot(repo, h1!, []);
    assert.ok((report.submodules ?? 0) >= 1, 'M6: gitlink count reported');
    assert.ok(report.kept.length <= 200, 'M5: kept list bounded');
  } finally {
    await rm(repo, { recursive: true, force: true });
  }
});
