import { mkdir, readdir, readFile, writeFile } from 'node:fs/promises';
import { readFileSync, unlinkSync } from 'node:fs';
import { join } from 'node:path';
import { tangoDir } from './config.js';
import { isSameProcess, processStarts } from './liveness.js';

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

const lockDir = () => join(tangoDir(), 'locks', 'sessions');
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
