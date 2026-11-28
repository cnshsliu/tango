/**
 * Workspace snapshots for time travel. Git repos: a commit built on a TEMP index
 * (GIT_INDEX_FILE), so the user's branch, index and status are never touched.
 * Non-git dirs: a tar.gz under TANGO_HOME/snapshots, addressed as "tar:<abs path>".
 */
import { spawnSync } from 'node:child_process';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { existsSync, readdirSync } from 'node:fs';

/**
 * Every file under `root`, relative to it, without `.git` — what `find root -type f` listed
 * before (Windows' find is a different program: there the list came back empty).
 */
function listFiles(root: string, dir: string = root, out: string[] = []): string[] {
  let entries;
  try {
    entries = readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const e of entries) {
    const abs = join(dir, e.name);
    if (e.isDirectory()) {
      if (e.name === '.git' || e.name === 'node_modules') continue;
      listFiles(root, abs, out);
    } else if (e.isFile()) {
      out.push(relative(root, abs));
    }
  }
  return out;
}
import { tmpdir } from 'node:os';
import { dirname, join, relative } from 'node:path';
import { homedir } from 'node:os';

function git(cwd: string, args: string[], env: NodeJS.ProcessEnv = {}): string {
  const r = spawnSync('git', args, { cwd, encoding: 'utf8', env: { ...process.env, ...env }, timeout: 10_000 });
  if (r.status !== 0) throw new Error(`git ${args[0]}: ${(r.stderr || r.stdout).trim()}`);
  return r.stdout;
}

function gitOk(cwd: string, args: string[], env: NodeJS.ProcessEnv = {}): string | null {
  const r = spawnSync('git', args, { cwd, encoding: 'utf8', env: { ...process.env, ...env }, timeout: 10_000 });
  return r.status === 0 ? r.stdout : null;
}

/** Repo root for cwd, or null when not inside a git repository. */
function repoRoot(cwd: string): string | null {
  return gitOk(cwd, ['rev-parse', '--show-toplevel'])?.trim() ?? null;
}

function tarDir(): string {
  return join(process.env.TANGO_HOME ?? join(homedir(), '.tango'), 'snapshots');
}

let tarSeq = 0;

/**
 * Snapshot the workspace at cwd. Returns a commit hash (git repos) or
 * "tar:<path>" (plain directories); null when even the tar fallback fails.
 */
export async function takeSnapshot(cwd: string, sessionId = 'adhoc'): Promise<string | null> {
  const root = repoRoot(cwd);
  if (root) {
    try {
      const tmpIndex = await mkdtemp(join(tmpdir(), 'tango-idx-'));
      try {
        const env = { GIT_INDEX_FILE: join(tmpIndex, 'index') };
        git(root, ['add', '-A', '--'], env); // populates the temp index only
        const tree = git(root, ['write-tree'], env).trim();
        // chain onto the session's ref so every snapshot stays reachable —
        // unreachable commits would be pruned by a routine `git gc`
        const ref = `refs/tango/checkpoints/${sessionId}`;
        const prev = gitOk(root, ['rev-parse', '--verify', ref]);
        const hash = git(
          root,
          prev
            ? ['commit-tree', tree, '-p', prev.trim(), '-m', 'tango checkpoint']
            : ['commit-tree', tree, '-m', 'tango checkpoint'],
          env
        ).trim();
        git(root, ['update-ref', ref, hash]);
        return hash;
      } finally {
        await rm(tmpIndex, { recursive: true, force: true });
      }
    } catch {
      return null;
    }
  }
  // tar fallback for non-git directories
  try {
    const dir = join(tarDir(), sessionId);
    await mkdir(dir, { recursive: true });
    const path = join(dir, `${Date.now()}-${tarSeq++}.tar.gz`);
    // honor a .gitignore when one exists (best-effort: globs, no negation support)
    const args = ['-czf', path];
    const ignoreFile = join(cwd, '.gitignore');
    if (existsSync(ignoreFile)) {
      for (const line of (await readFile(ignoreFile, 'utf8')).split('\n')) {
        const pat = line.trim().replace(/\/$/, '');
        if (pat && !pat.startsWith('#') && !pat.startsWith('!')) args.push(`--exclude=${pat}`);
      }
    }
    args.push('-C', cwd, '.');
    const r = spawnSync('tar', args, { encoding: 'utf8', timeout: 60_000 });
    if (r.status !== 0) return null;
    // cleanup policy: keep the newest 50 snapshots per session
    const { readdirSync, statSync } = await import('node:fs');
    const tars = readdirSync(dir)
      .filter((f) => f.endsWith('.tar.gz'))
      .map((f) => ({ f, m: statSync(join(dir, f)).mtimeMs }))
      .sort((a, b) => b.m - a.m);
    for (const old of tars.slice(50)) await rm(join(dir, old.f), { force: true });
    return `tar:${path}`;
  } catch {
    return null;
  }
}

/** Files inside a snapshot, repo/workspace-relative posix paths. */
export async function snapshotFileList(cwd: string, snap: string): Promise<Set<string>> {
  if (snap.startsWith('tar:')) {
    const r = spawnSync('tar', ['-tzf', snap.slice(4)], { encoding: 'utf8', timeout: 10_000 });
    if (r.status !== 0) return new Set();
    return new Set(
      r.stdout
        .split('\n')
        .map((l) => l.trim().replace(/^\.\//, ''))
        .filter((l) => l && !l.endsWith('/'))
    );
  }
  const root = repoRoot(cwd) ?? cwd;
  const out = gitOk(root, ['ls-tree', '-r', '--name-only', snap]) ?? '';
  return new Set(out.split('\n').filter(Boolean));
}

export interface RestoreReport {
  restored: number;
  deleted: string[];
  kept: string[];
  /** tracked files whose on-disk (user-edited) content is being replaced by the snapshot */
  overwritten: string[];
  /** gitlink (submodule) entries in the snapshot — restored as pointers, contents not snapshotted */
  submodules: number;
}

/** Restore a snapshot with smart-delete semantics (design spec §3.3).
 * `protect`: files never deleted even if a later snapshot knows them (e.g. user files
 * created between two dropped turns — detected by checkpoint pair-diffs in serve). */
export async function restoreSnapshot(cwd: string, target: string, later: string[], protect: Set<string> = new Set()): Promise<RestoreReport> {
  const root = repoRoot(cwd) ?? cwd;
  let overwritten: string[] = [];
  const targetFiles = await snapshotFileList(cwd, target);
  // union of files present in any LATER snapshot — the candidates for deletion
  const laterFiles = new Set<string>();
  for (const h of later) for (const f of await snapshotFileList(cwd, h)) laterFiles.add(f);

  // 1. put every target-tree file back on disk
  if (target.startsWith('tar:')) {
    const tmp = await mkdtemp(join(tmpdir(), 'tango-restore-'));
    try {
      const ex = spawnSync('tar', ['-xzf', target.slice(4), '-C', tmp], { encoding: 'utf8', timeout: 60_000 });
      if (ex.status !== 0) throw new Error(`tar extract failed: ${(ex.stderr || '').trim()}`);
      // cp -p keeps executable bits and mtimes
      for (const f of targetFiles) {
        const src = join(tmp, f);
        if (!existsSync(src)) continue;
        const dest = join(root, f);
        await mkdir(dirname(dest), { recursive: true });
        const cp = spawnSync('cp', ['-p', src, dest], { encoding: 'utf8', timeout: 10_000 });
        if (cp.status !== 0) throw new Error(`cp failed for ${f}: ${(cp.stderr || '').trim()}`);
      }
    } finally {
      await rm(tmp, { recursive: true, force: true });
    }
  } else {
    const tmpIndex = await mkdtemp(join(tmpdir(), 'tango-idx-'));
    try {
      const env = { GIT_INDEX_FILE: join(tmpIndex, 'index') };
      git(root, ['read-tree', target], env); // throws when the snapshot was pruned
      // report user edits about to be clobbered: worktree files differing from the target
      const diff = gitOk(root, ['diff', '--name-only', '--'], env);
      overwritten = (diff ?? '')
        .split('\n')
        .map((l) => l.trim())
        .filter(Boolean);
      git(root, ['checkout-index', '-a', '-f', '--'], env); // writes worktree files
    } finally {
      await rm(tmpIndex, { recursive: true, force: true });
    }
  }

  // 2. smart delete: on disk, not in the target tree, but in a later snapshot → agent leftover
  const deleted: string[] = [];
  for (const f of laterFiles) {
    if (targetFiles.has(f)) continue;
    if (protect.has(f)) continue; // user-created between dropped turns — never delete
    const abs = join(root, f);
    if (!existsSync(abs)) continue;
    await rm(abs, { force: true });
    deleted.push(f);
  }

  // 3. report files on disk that no snapshot knows about (the user's own work) — untouched
  const kept: string[] = [];
  const known = new Set([...targetFiles, ...laterFiles]);
  for (const rel of listFiles(root)) {
    if (!known.has(rel)) kept.push(rel);
    if (kept.length >= 200) break; // big repos would flood the result frame
  }

  // 4. gitlink (submodule) entries snapshot as pointers only — surface that
  let submodules = 0;
  if (!target.startsWith('tar:')) {
    const full = gitOk(root, ['ls-tree', '-r', target], { }) ?? '';
    for (const line of full.split('\n')) if (line.startsWith('160000')) submodules++;
  }

  return { restored: targetFiles.size, deleted, kept, overwritten, submodules };
}
