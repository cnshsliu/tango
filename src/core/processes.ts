import { spawn, type ChildProcess } from 'node:child_process';

/**
 * Background shell processes started by `bash` with background: true.
 * Each runs in its own process group (detached) so stop/exit reach grandchildren;
 * output is kept as a bounded tail. All of them die with the host process.
 */
export const MAX_RUNNING = 8;
const MAX_TAIL = 200_000;

export interface BgProcess {
  id: string;
  pid: number;
  command: string;
  cwd: string;
  startedAt: number;
  /** null while running */
  exitCode: number | null;
  signal: NodeJS.Signals | null;
  done: boolean;
  /** output tail (stdout+stderr interleaved), capped at MAX_TAIL chars */
  output: string;
  /** chars dropped from the front of `output` */
  dropped: number;
  child: ChildProcess;
  exited: Promise<void>;
}

/**
 * How long `bash background: true` waits before returning, so a command that fails right away
 * (typo, port in use) reports its error. A login shell alone takes ~0.8 s on a busy profile.
 * Mutable so tests on a loaded machine can wait longer without slowing every real call.
 */
export const startup = { graceMs: 2000 };

const procs = new Map<string, BgProcess>();
let seq = 0;
let hooked = false;

export function running(): BgProcess[] {
  return [...procs.values()].filter((p) => !p.done);
}

export function listProcesses(): BgProcess[] {
  return [...procs.values()];
}

export function getProcess(id: string): BgProcess | undefined {
  return procs.get(id.trim());
}

function killGroup(p: BgProcess, sig: NodeJS.Signals): void {
  try {
    process.kill(-p.pid, sig);
  } catch {
    try { p.child.kill(sig); } catch {}
  }
}

/** Start `command` in the background. Throws when MAX_RUNNING are already running. */
export function startProcess(command: string, cwd: string): BgProcess {
  if (running().length >= MAX_RUNNING) {
    throw new Error(`already ${MAX_RUNNING} background processes running; stop one with process_stop first`);
  }
  if (!hooked) {
    hooked = true;
    // background work never outlives tango: no orphaned dev servers
    process.on('exit', () => running().forEach((p) => killGroup(p, 'SIGKILL')));
  }
  const child = spawn('/bin/bash', ['-lc', command], {
    cwd,
    env: process.env,
    stdio: ['ignore', 'pipe', 'pipe'],
    detached: true
  });
  const id = `p${++seq}`;
  let resolveExit!: () => void;
  const exited = new Promise<void>((r) => (resolveExit = r));
  const p: BgProcess = {
    id, pid: child.pid ?? -1, command, cwd, startedAt: Date.now(),
    exitCode: null, signal: null, done: false, output: '', dropped: 0, child, exited
  };
  const push = (b: Buffer) => {
    p.output += b.toString();
    if (p.output.length > MAX_TAIL) {
      const cut = p.output.length - MAX_TAIL;
      p.output = p.output.slice(cut);
      p.dropped += cut;
    }
  };
  child.stdout!.on('data', push);
  child.stderr!.on('data', push);
  const finish = (code: number | null, sig: NodeJS.Signals | null) => {
    if (p.done) return;
    p.done = true;
    p.exitCode = code;
    p.signal = sig;
    resolveExit();
  };
  child.on('error', (e) => {
    p.output += `\n[spawn error: ${e.message}]`;
    finish(-1, null);
  });
  child.on('close', finish);
  procs.set(id, p);
  return p;
}

/** SIGTERM the group, then SIGKILL after graceMs if it is still alive. */
export async function stopProcess(p: BgProcess, graceMs = 3000): Promise<void> {
  if (p.done) return;
  killGroup(p, 'SIGTERM');
  const timer = setTimeout(() => killGroup(p, 'SIGKILL'), graceMs);
  await p.exited;
  clearTimeout(timer);
}

/** Last `lines` lines of output. */
export function tail(p: BgProcess, lines = 50): string {
  const all = p.output.replace(/\s+$/, '').split('\n');
  return all.slice(-Math.max(1, lines)).join('\n');
}

export function describe(p: BgProcess): string {
  const secs = Math.round((Date.now() - p.startedAt) / 1000);
  const state = !p.done ? `running ${secs}s` : p.signal ? `killed (${p.signal})` : `exited ${p.exitCode}`;
  return `${p.id} pid ${p.pid} [${state}] ${p.command}`;
}

/** test helper */
export function _resetProcesses(): void {
  running().forEach((p) => killGroup(p, 'SIGKILL'));
  procs.clear();
}
