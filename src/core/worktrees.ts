/**
 * Managed git worktrees: tango's isolation unit for background prompts and subagents
 * (design decision #5: the main workspace has exactly one writer).
 * Layout: ~/.tango/worktrees/<name>/ on branch tango/<name>, registered in worktrees.json.
 */
import { spawnSync } from 'node:child_process';
import { existsSync, realpathSync } from 'node:fs';
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import { renameRetry } from './fs-retry.js';
import { join, sep } from 'node:path';
import { sessionsDir, tangoDir } from './config.js';
import { liveSessionLocks } from './session-lock.js';
import { SessionStore, shortId } from './session.js';

export interface WorktreeEntry {
  name: string;
  path: string;
  branch: string;
  createdAt: string;
}

const registryFile = () => join(tangoDir(), 'worktrees.json');
/** Where managed worktrees live; writes under it never ask (tango's own isolation checkouts). */
export const worktreeRoot = () => join(tangoDir(), 'worktrees');

function git(cwd: string, args: string[]): { ok: boolean; out: string; err: string } {
  const r = spawnSync('git', args, { cwd, encoding: 'utf8', timeout: 15_000 });
  return { ok: r.status === 0, out: (r.stdout ?? '').trim(), err: (r.stderr ?? '').trim() };
}

async function readRegistry(): Promise<WorktreeEntry[]> {
  try {
    return JSON.parse(await readFile(registryFile(), 'utf8'));
  } catch {
    return [];
  }
}

/** atomic registry write (tmp + rename) */
async function writeRegistry(entries: WorktreeEntry[]): Promise<void> {
  await mkdir(tangoDir(), { recursive: true });
  const tmp = registryFile() + '.tmp';
  await writeFile(tmp, JSON.stringify(entries, null, 2), 'utf8');
  await renameRetry(tmp, registryFile());
}

export function isGitRepo(cwd: string): boolean {
  return git(cwd, ['rev-parse', '--git-dir']).ok;
}

export async function worktreeCreate(cwd: string, name: string, base?: string): Promise<WorktreeEntry> {
  if (!/^[a-zA-Z0-9._-]{1,40}$/.test(name)) throw new Error(`worktree name must be [a-zA-Z0-9._-]: ${name}`);
  const entries = await readRegistry();
  if (entries.some((e) => e.name === name)) throw new Error(`worktree already exists: ${name}`);
  const path = join(worktreeRoot(), name);
  if (existsSync(path)) throw new Error(`worktree path already taken: ${path}`);
  const branch = `tango/${name}`;
  const r = git(cwd, ['worktree', 'add', '-b', branch, path, base ?? 'HEAD']);
  if (!r.ok) throw new Error(`git worktree add: ${r.err || r.out}`);
  const entry = { name, path, branch, createdAt: new Date().toISOString() };
  await writeRegistry([...entries, entry]);
  return entry;
}

export async function worktreeList(): Promise<(WorktreeEntry & { dirty: number })[]> {
  const entries = await readRegistry();
  return entries.map((e) => {
    const status = git(e.path, ['status', '--porcelain']);
    const dirty = status.ok ? status.out.split('\n').filter(Boolean).length : -1;
    return { ...e, dirty };
  });
}

/** Live sessions whose working directory is inside `path`: pulling it away leaves them unable
 *  to run anything. A lock records where a session was opened, but /cd moves it, so the
 *  session's latest meta entry is the truth. */
export async function sessionsInDir(path: string): Promise<{ id: string; pid: number }[]> {
  let root: string;
  try {
    root = realpathSync(path);
  } catch {
    return []; // already gone: nothing can be working in it
  }
  const prefix = root.endsWith(sep) ? root : root + sep;
  const found: { id: string; pid: number }[] = [];
  for (const { sessionId, lock } of await liveSessionLocks()) {
    let cwd = lock.cwd;
    try {
      cwd = (await SessionStore.open(sessionsDir(), sessionId)).meta()?.cwd ?? cwd;
    } catch {}
    if (typeof cwd !== 'string' || !cwd) continue;
    let real: string;
    try {
      real = realpathSync(cwd);
    } catch {
      continue; // the session's own directory is gone: it isn't inside one that exists
    }
    if (real === root || real.startsWith(prefix)) found.push({ id: sessionId, pid: lock.pid });
  }
  return found;
}

export async function worktreeRemove(cwd: string, name: string, force = false): Promise<void> {
  const entries = await readRegistry();
  const entry = entries.find((e) => e.name === name);
  if (!entry) throw new Error(`no such worktree: ${name}`);
  // A live session working there would lose its directory, so this is refused with or without
  // force: force is about uncommitted files, this is about another running session.
  const inUse = await sessionsInDir(entry.path);
  if (inUse.length) {
    const who = inUse.map((s) => `${shortId(s.id)} (pid ${s.pid})`).join(', ');
    throw new Error(
      `worktree "${name}" is in use: session ${who} works in it — /cd that session to another directory (or close it) first`
    );
  }
  const status = git(entry.path, ['status', '--porcelain']);
  if (!force && status.ok && status.out.trim()) {
    throw new Error(`worktree "${name}" has uncommitted changes (commit/merge first, or force)`);
  }
  const r = git(cwd, ['worktree', 'remove', '--force', entry.path]);
  if (!r.ok && existsSync(entry.path)) throw new Error(`git worktree remove: ${r.err}`);
  // the branch survives if it has unmerged commits; delete only when fully merged
  git(cwd, ['branch', '-d', entry.branch]);
  await writeRegistry(entries.filter((e) => e.name !== name));
}

export async function worktreeMerge(cwd: string, name: string): Promise<{ merged: boolean; output: string }> {
  const entries = await readRegistry();
  const entry = entries.find((e) => e.name === name);
  if (!entry) throw new Error(`no such worktree: ${name}`);
  // merge the worktree branch into the current branch of the MAIN repo
  const mainStatus = git(cwd, ['status', '--porcelain']);
  if (mainStatus.ok && mainStatus.out.trim()) {
    throw new Error('main workspace has uncommitted changes — commit or stash before merging');
  }
  const r = git(cwd, ['merge', '--no-edit', entry.branch]);
  if (!r.ok) {
    // never leave the main repo mid-merge: abort when a merge actually started
    const aborted = git(cwd, ['merge', '--abort']).ok;
    const note = aborted ? '\n（已自动 git merge --abort，主工作区已恢复干净）' : '';
    return { merged: false, output: (r.out || r.err) + note };
  }
  return { merged: true, output: r.out };
}
