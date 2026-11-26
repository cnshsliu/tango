import { spawnSync } from 'node:child_process';

/**
 * Who is really behind a recorded pid. A pid alone is not an identity: once a process dies the
 * kernel hands its number to the next process, so "the pid is alive" can be true for a complete
 * stranger (a stale lock then makes a closed session look open, or an idle directory look busy).
 * The record also carries when it was written, and whoever wrote it was running at the time, so a
 * process that started afterwards is not that process.
 */

/** Does `pid` exist right now? Signal 0 only probes; EPERM means it exists but belongs to someone else. */
export function pidAlive(pid: number): boolean {
  if (!(pid > 1)) return false; // 0 and -1 would signal our own process group; 1 is init
  try {
    process.kill(pid, 0);
    return true;
  } catch (e: any) {
    return e?.code === 'EPERM';
  }
}

/** `ps -o etime=` → seconds: `05`, `1:03`, `21-04:51:59`, `0:00.13`. NaN when it isn't one of those. */
export function parseEtime(s: string): number {
  let t = s.trim();
  let days = 0;
  const d = /^(\d+)-(.*)$/.exec(t);
  if (d) {
    days = Number(d[1]);
    t = d[2];
  }
  const parts = t.split(':').map(Number);
  if (!parts.length || parts.some((n) => !Number.isFinite(n))) return NaN;
  let sec = 0;
  for (const n of parts) sec = sec * 60 + n;
  return days * 86400 + sec;
}

/**
 * When each of these processes started (ms since epoch), from one `ps` call. A pid that is gone,
 * or whose age could not be read (no `ps` on this OS), is left out.
 */
export function processStarts(pids: number[]): Map<number, number> {
  const out = new Map<number, number>();
  const want = [...new Set(pids)].filter(pidAlive);
  if (!want.length) return out;
  const ask = (list: number[]) => {
    try {
      const r = spawnSync('ps', ['-o', 'pid=,etime=', '-p', list.join(',')], { encoding: 'utf8', timeout: 3000 });
      if (r.status !== 0 && list.length > 1) return; // one bad entry can make ps refuse the whole list: ask one by one
      const now = Date.now();
      for (const line of (r.stdout ?? '').split('\n')) {
        const m = /^\s*(\d+)\s+(\S+)\s*$/.exec(line);
        if (!m) continue;
        const sec = parseEtime(m[2]);
        if (Number.isFinite(sec)) out.set(Number(m[1]), now - sec * 1000);
      }
    } catch {}
  };
  ask(want);
  for (const p of want.filter((p) => !out.has(p))) ask([p]);
  return out;
}

/** `ps` reports whole seconds, so an estimated start can land up to a second late. */
const START_SLACK_MS = 2_000;

/**
 * Is `pid` still the process that wrote a record at `at` (an ISO timestamp)? False when the pid is
 * gone or belongs to a process that started after the record. When the age cannot be read, or the
 * record predates the timestamp, the answer stays the plain "pid alive".
 */
export function isSameProcess(pid: number, at: string | undefined, startedMs: number | undefined): boolean {
  if (!pidAlive(pid)) return false;
  const written = Date.parse(at ?? '');
  if (!Number.isFinite(written) || startedMs === undefined) return true;
  return startedMs <= written + START_SLACK_MS;
}

/** `isSameProcess` for one pid, reading its start time here. */
export function stillHolds(pid: number, at: string | undefined): boolean {
  return isSameProcess(pid, at, processStarts([pid]).get(pid));
}
