import { mkdir, readdir, readFile, unlink, writeFile } from 'node:fs/promises';
import { readFileSync, unlinkSync } from 'node:fs';
import { join } from 'node:path';
import { tangoDir } from './config.js';
import { isSameProcess, processStarts, stillHolds } from './liveness.js';

/**
 * Per-session liveness: one file per open session under [tango]/locks/sessions/[id].json,
 * holding the pid of the serve process that has it open, and when it took it. Lets /handoff
 * tell whether the target is live in another window. A lock counts only while that pid is still
 * the same process: a pid the kernel recycled onto something else (an editor, a simulator) is
 * not a tango holding the session. (The cwd-keyed locks in serve.ts answer a different
 * question: is another tango writing to this directory.)
 */
export interface SessionLock {
  pid: number;
  cwd: string;
  at: string;
}

/** [tango]/locks holds the cwd-keyed locks (written by serve.ts) and, in sessions/, these. */
const lockRoot = () => join(tangoDir(), 'locks');
const lockDir = () => join(lockRoot(), 'sessions');
const lockFile = (id: string) => join(lockDir(), `${id}.json`);

/** Mark `sessionId` as open by this process; the file is removed on exit (if still ours). */
export async function holdSessionLock(sessionId: string, cwd: string): Promise<void> {
  const file = lockFile(sessionId);
  await mkdir(lockDir(), { recursive: true });
  const lock: SessionLock = { pid: process.pid, cwd, at: new Date().toISOString() };
  await writeFile(file, JSON.stringify(lock), 'utf8');
  process.on('exit', () => {
    try {
      if (JSON.parse(readFileSync(file, 'utf8')).pid === process.pid) unlinkSync(file);
    } catch {}
  });
}

/** The live process (other than this one) that has `sessionId` open, or null. Stale locks count as closed. */
export async function sessionHolder(sessionId: string): Promise<SessionLock | null> {
  try {
    const lock = JSON.parse(await readFile(lockFile(sessionId), 'utf8')) as SessionLock;
    if (!(lock.pid > 1) || lock.pid === process.pid) return null;
    return isSameProcess(lock.pid, lock.at, processStarts([lock.pid]).get(lock.pid)) ? lock : null;
  } catch {
    return null;
  }
}

/** One live lock: which session, and the process that holds it open. */
export interface LiveSessionLock {
  sessionId: string;
  lock: SessionLock;
}

/** Every session a live process holds open, this one included: stale locks count as closed. */
export async function liveSessionLocks(): Promise<LiveSessionLock[]> {
  let files: string[];
  try {
    files = (await readdir(lockDir())).filter((f) => f.endsWith('.json'));
  } catch {
    return [];
  }
  const found: LiveSessionLock[] = [];
  for (const f of files) {
    try {
      const lock = JSON.parse(await readFile(join(lockDir(), f), 'utf8')) as SessionLock;
      if (lock.pid > 1) found.push({ sessionId: f.slice(0, -'.json'.length), lock });
    } catch {}
  }
  // one `ps` for every lock, however many there are
  const starts = processStarts(found.map(({ lock }) => lock.pid));
  return found.filter(({ lock }) => isSameProcess(lock.pid, lock.at, starts.get(lock.pid)));
}

/**
 * Remove the lock files nobody holds any more: their process died, or its pid was recycled onto
 * something else. A killed process cannot take its lock with it, so a serve sweeps both kinds
 * when it starts. Each file is judged again right before it is unlinked, so a lock a process
 * takes during the sweep stays; a file that cannot be read is left alone. Returns how many went.
 */
export async function cleanStaleLocks(): Promise<number> {
  let removed = 0;
  for (const dir of [lockRoot(), lockDir()]) {
    let files: string[];
    try {
      files = (await readdir(dir)).filter((f) => f.endsWith('.json'));
    } catch {
      continue;
    }
    const locks: { file: string; pid: number; at?: string }[] = [];
    for (const f of files) {
      try {
        const lock = JSON.parse(await readFile(join(dir, f), 'utf8'));
        if (typeof lock?.pid === 'number' && lock.pid > 1) locks.push({ file: join(dir, f), pid: lock.pid, at: lock.at });
      } catch {} // unreadable: not ours to judge
    }
    const starts = processStarts(locks.map((l) => l.pid)); // one `ps` for the whole directory
    for (const l of locks) {
      if (isSameProcess(l.pid, l.at, starts.get(l.pid))) continue;
      let again: any;
      try {
        again = JSON.parse(await readFile(l.file, 'utf8'));
      } catch {
        continue;
      }
      if (!(again?.pid > 1) || stillHolds(again.pid, again.at)) continue;
      try {
        await unlink(l.file);
        removed++;
      } catch {} // already gone: another sweep got there first
    }
  }
  return removed;
}
