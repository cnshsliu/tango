import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, realpathSync } from 'node:fs';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { describeSources, mergeLayer, projectRoots, resolveConfig, setTrust, trustOf, updateLocalConfig } from '../src/core/project-config.js';

const sh = (cwd: string, ...args: string[]) => spawnSync('git', args, { cwd, encoding: 'utf8' });

/** isolated TANGO_HOME + a git repo, with optional global / shared / personal files */
async function world(files: { global?: object; project?: object; local?: object } = {}) {
  const home = realpathSync(await mkdtemp(join(tmpdir(), 'tango-pc-home-')));
  const repo = realpathSync(await mkdtemp(join(tmpdir(), 'tango-pc-repo-')));
  sh(repo, 'init', '-q', '-b', 'main');
  const prev = process.env.TANGO_HOME;
  process.env.TANGO_HOME = home;
  if (files.global) await writeFile(join(home, 'config.json'), JSON.stringify(files.global));
  await mkdir(join(repo, '.tango'), { recursive: true });
  if (files.project) await writeFile(join(repo, '.tango', 'config.json'), JSON.stringify(files.project));
  if (files.local) await writeFile(join(repo, '.tango', 'config.local.json'), JSON.stringify(files.local));
  const done = async () => {
    process.env.TANGO_HOME = prev;
    await rm(home, { recursive: true, force: true });
    await rm(repo, { recursive: true, force: true });
  };
  return { home, repo, done };
}

test('merge: nearer scalars win, lists add up, maps merge by key', () => {
  const g = { model: 'a', theme: 'dark' as const, permissions: { allow: ['tool:diff'] }, keys: { x: 'ctrl+x' }, mcpServers: { fs: { command: 'g' } } };
  const p = { model: 'b', permissions: { allow: ['bash:npm test', 'tool:diff'] }, keys: { y: 'ctrl+y' }, mcpServers: { fs: { command: 'p' }, db: { command: 'd' } } };
  const m = mergeLayer(g, p);
  assert.equal(m.model, 'b');
  assert.equal(m.theme, 'dark', 'unset keys keep the lower value');
  assert.deepEqual(m.permissions?.allow, ['tool:diff', 'bash:npm test'], 'union, no duplicates');
  assert.deepEqual(m.keys, { x: 'ctrl+x', y: 'ctrl+y' });
  assert.equal(m.mcpServers?.fs.command, 'p', 'same server name: the project entry replaces it whole');
  assert.ok(m.mcpServers?.db);
});

test('merge: hook events concatenate; project extraDirs resolve against the project', () => {
  const m = mergeLayer(
    { hooks: { Stop: [{ command: 'g' }] }, extraDirs: ['/abs'] },
    { hooks: { Stop: [{ command: 'p' }], PreToolUse: [{ command: 'q' }] }, extraDirs: ['vendor', '/abs'] },
    '/repo'
  );
  assert.deepEqual((m.hooks as any).Stop.map((h: any) => h.command), ['g', 'p']);
  assert.equal((m.hooks as any).PreToolUse.length, 1);
  assert.deepEqual(m.extraDirs, ['/abs', '/repo/vendor']);
});

test('resolve: three layers in order; global-only keys never come from a project', async () => {
  const w = await world({
    global: { model: 'g', theme: 'dark', feishuWebhook: 'https://g' },
    project: { model: 'p', theme: 'light', feishuWebhook: 'https://evil' },
    local: { model: 'l' }
  });
  try {
    const r = await resolveConfig(w.repo);
    assert.equal(r.config.model, 'l', 'personal beats shared beats global');
    assert.equal(r.config.theme, 'light');
    assert.equal(r.config.feishuWebhook, 'https://g', 'a repo cannot redirect notifications');
    assert.deepEqual(r.sources.map((s) => [s.scope, s.exists]), [['global', true], ['project', true], ['local', true]]);
    assert.deepEqual(r.sources[1].ignored, [{ key: 'feishuWebhook', why: 'global-only' }]);
    assert.equal(r.project?.needsTrust, false, 'nothing gated: no question to ask');
  } finally {
    await w.done();
  }
});

test('resolve: gated keys from the shared file wait for trust; deny keeps them out', async () => {
  const w = await world({
    global: { permissions: { allow: ['tool:diff'] } },
    project: { hooks: { PreToolUse: [{ command: 'evil' }] }, permissions: { allow: ['bash:rm'] }, mcpServers: { x: { command: 'x' } }, extraDirs: ['/'], model: 'p' }
  });
  try {
    let r = await resolveConfig(w.repo);
    assert.equal(r.project?.needsTrust, true);
    assert.deepEqual(r.project?.gated.sort(), ['extraDirs', 'hooks', 'mcpServers', 'permissions']);
    assert.equal(r.config.hooks, undefined, 'no project hooks before trust');
    assert.deepEqual(r.config.permissions?.allow, ['tool:diff'], 'only the global rules');
    assert.equal(r.config.mcpServers, undefined);
    assert.equal(r.config.extraDirs, undefined);
    assert.equal(r.config.model, 'p', 'ungated keys apply right away');

    setTrust(r.project!.localRoot, false);
    r = await resolveConfig(w.repo);
    assert.equal(r.project?.needsTrust, false, 'a recorded "no" is not asked again');
    assert.equal(r.config.hooks, undefined);

    setTrust(r.project!.localRoot, true);
    r = await resolveConfig(w.repo);
    assert.deepEqual(r.config.permissions?.allow, ['tool:diff', 'bash:rm']);
    assert.equal((r.config.hooks as any).PreToolUse[0].command, 'evil');
    assert.ok(r.config.mcpServers?.x);
    assert.equal(trustOf(r.project!.localRoot), true);
  } finally {
    await w.done();
  }
});

test('resolve: an untracked personal file is yours and needs no trust; a tracked one does', async () => {
  const w = await world({ local: { permissions: { allow: ['bash:make'] } } });
  try {
    let r = await resolveConfig(w.repo);
    assert.deepEqual(r.config.permissions?.allow, ['bash:make']);
    assert.equal(r.project?.needsTrust, false);
    sh(w.repo, 'add', '-f', '.tango/config.local.json');
    r = await resolveConfig(w.repo);
    assert.equal(r.config.permissions, undefined, 'came with the repo: treated like the shared file');
    assert.equal(r.project?.needsTrust, true);
  } finally {
    await w.done();
  }
});

test('roots: worktrees share the main checkout for trust and the personal file', async () => {
  const w = await world();
  try {
    sh(w.repo, '-c', 'user.email=t@t', '-c', 'user.name=t', 'commit', '-q', '--allow-empty', '-m', 'x');
    const wt = join(w.repo, '..', `${w.repo.split('/').pop()}-wt`);
    sh(w.repo, 'worktree', 'add', '-q', wt);
    try {
      const r = projectRoots(join(wt));
      assert.equal(r?.root, realpathSync(wt), 'shared file: the worktree itself');
      assert.equal(r?.localRoot, w.repo, 'personal file + trust: the main checkout');
      const sub = join(w.repo, 'src', 'deep');
      await mkdir(sub, { recursive: true });
      assert.equal(projectRoots(sub)?.root, w.repo, 'a subdirectory finds the repo root');
    } finally {
      await rm(wt, { recursive: true, force: true });
    }
  } finally {
    await w.done();
  }
});

test('roots: no project layer when the project .tango is the global dir (cwd = home)', async () => {
  const home = realpathSync(await mkdtemp(join(tmpdir(), 'tango-pc-h-')));
  const prev = process.env.TANGO_HOME;
  process.env.TANGO_HOME = join(home, '.tango');
  try {
    assert.equal(projectRoots(home), undefined);
    const r = await resolveConfig(home);
    assert.equal(r.sources.length, 1);
    assert.equal(r.project, undefined);
  } finally {
    process.env.TANGO_HOME = prev;
    await rm(home, { recursive: true, force: true });
  }
});

test('broken project JSON is skipped and reported, not fatal', async () => {
  const w = await world({ global: { model: 'g' } });
  try {
    await writeFile(join(w.repo, '.tango', 'config.json'), '{ nope');
    const r = await resolveConfig(w.repo);
    assert.equal(r.config.model, 'g');
    assert.match(r.sources[1].error ?? '', /invalid JSON/);
    assert.match(describeSources(r), /项目 ✗ invalid JSON/);
  } finally {
    await w.done();
  }
});

test('personal writes: file created, kept out of git via .git/info/exclude, once', async () => {
  const w = await world();
  try {
    const path = updateLocalConfig(w.repo, (c) => ({ ...c, permissions: { allow: ['bash:make'] } }));
    updateLocalConfig(w.repo, (c) => ({ ...c, model: 'x' }));
    assert.deepEqual(JSON.parse(await readFile(path, 'utf8')), { permissions: { allow: ['bash:make'] }, model: 'x' });
    assert.equal(sh(w.repo, 'status', '--porcelain', '--untracked-files=all').stdout.includes('config.local.json'), false, 'git does not see it');
    const exclude = await readFile(join(w.repo, '.git', 'info', 'exclude'), 'utf8');
    assert.equal(exclude.split('/.tango/config.local.json').length - 1, 1, 'excluded exactly once');
  } finally {
    await w.done();
  }
});

test('describeSources: one readable line with what was dropped and the trust state', async () => {
  const w = await world({ global: {}, project: { hooks: { Stop: [] }, feishuWebhook: 'x' } });
  try {
    const line = describeSources(await resolveConfig(w.repo));
    assert.equal(line, '配置：全局 ✓ · 项目 ✓（忽略 feishuWebhook, hooks） · 个人 — · 信任 待确认');
    assert.ok(!existsSync(join(w.repo, '.tango', 'config.local.json')), 'reading never creates files');
  } finally {
    await w.done();
  }
});
