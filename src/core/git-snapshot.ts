/**
 * Workspace snapshots for time travel. Git repos: a commit built on a TEMP index
 * (GIT_INDEX_FILE), so the user's branch, index and status are never touched.
 * Non-git dirs: a tar.gz under TANGO_HOME/snapshots, addressed as "tar:<abs path>".
 */
import { spawnSync } from 'node:child_process';
import { mkdir, mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { homedir } from 'node:os';

function git(cwd: string, args: string[], env: NodeJS.ProcessEnv = {}): string {
  const r = spawnSync('git', args, { cwd, encoding: 'utf8', env: { ...process.env, ...env } });
  if (r.status !== 0) throw new Error(`git ${args[0]}: ${(r.stderr || r.stdout).trim()}`);
  return r.stdout;
}

function gitOk(cwd: string, args: string[], env: NodeJS.ProcessEnv = {}): string | null {
  const r = spawnSync('git', args, { cwd, encoding: 'utf8', env: { ...process.env, ...env } });
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
        const hash = git(root, ['commit-tree', tree, '-m', 'tango checkpoint'], env).trim();
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
    const r = spawnSync('tar', ['-czf', path, '-C', cwd, '.'], { encoding: 'utf8' });
    if (r.status !== 0) return null;
    return `tar:${path}`;
  } catch {
    return null;
  }
}

/** Files inside a snapshot, repo/workspace-relative posix paths. */
export async function snapshotFileList(cwd: string, snap: string): Promise<Set<string>> {
  if (snap.startsWith('tar:')) {
    const r = spawnSync('tar', ['-tzf', snap.slice(4)], { encoding: 'utf8' });
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
