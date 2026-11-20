import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { readFileSync, unlinkSync } from 'node:fs';
import { join } from 'node:path';
import { tangoDir } from './config.js';

/**
 * Per-session liveness: one file per open session under [tango]/locks/sessions/[id].json,
 * holding the pid of the serve process that has it open. Lets /handoff tell whether the
 * target is live in another window. (The cwd-keyed locks in serve.ts answer a different
 * question: is another tango writing to this directory.)
 */
export interface SessionLock {
  pid: number;
  cwd: string;
  at: string;
}

const lockDir = () => join(tangoDir(), 'locks', 'sessions');
const lockFile = (id: string) => join(lockDir(), `${id}.json`);

const alive = (pid: number) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e: any) {
    return e?.code === 'EPERM'; // exists, owned by someone else
  }
};

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
    return alive(lock.pid) ? lock : null;
  } catch {
    return null;
  }
}
