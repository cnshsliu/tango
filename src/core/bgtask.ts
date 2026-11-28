/**
 * Background prompts: a dedicated worktree + an independent serve child that runs
 * exactly one prompt and exits on stdin EOF (the serve EOF contract: finish queued
 * work, then leave). The session file lands in the shared TANGO_HOME sessions dir,
 * so /resume can open any completed background task.
 */
import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';
import { createReadStream, createWriteStream, readFileSync } from 'node:fs';
import { mkdir, readFile, rename, writeFile } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import { tangoDir } from './config.js';
import { worktreeCreate, worktreeRemove, type WorktreeEntry } from './worktrees.js';
import { processCommandLine } from './liveness.js';
import { DETACHED, killProcessTree } from './shell.js';

export interface BgTask {
  id: string;
  name: string;
  prompt: string;
  worktree: string; // path
  worktreeName: string;
  sessionId?: string;
  /** full spawn command; kill refuses pids whose live command no longer matches (pid reuse) */
  command?: string;
  pid: number;
  logFile: string;
  status: 'running' | 'done' | 'failed' | 'stopped';
  startedAt: string;
  endedAt?: string;
  model?: string;
  /** /subagent: the session its summary goes back to, and whether that write-back happened */
  parent?: string;
  reported?: boolean;
}

const registryFile = () => join(tangoDir(), 'tasks-bg.json');
const logsDir = () => join(tangoDir(), 'bg-logs');

async function readAll(): Promise<BgTask[]> {
  try {
    return JSON.parse(await readFile(registryFile(), 'utf8'));
  } catch {
    return [];
  }
}

async function writeAll(tasks: BgTask[]): Promise<void> {
  await mkdir(tangoDir(), { recursive: true });
  // unique per write: two reconciles in flight (the 2s TaskCompleted poll and a `tasks`
  // command) must not rename each other's temp file away
  const tmp = `${registryFile()}.${process.pid}.${Math.random().toString(36).slice(2)}.tmp`;
  await writeFile(tmp, JSON.stringify(tasks, null, 2), 'utf8');
  await rename(tmp, registryFile());
}

export async function findTask(idOrName: string): Promise<BgTask | undefined> {
  const tasks = await readAll();
  return tasks.find((t) => t.id === idOrName || t.name === idOrName || t.id.startsWith(idOrName));
}

export async function updateTask(id: string, patch: Partial<BgTask>): Promise<void> {
  const tasks = await readAll();
  const t = tasks.find((x) => x.id === id);
  if (!t) return;
  Object.assign(t, patch);
  await writeAll(tasks);
}

function alive(pid: number): boolean {
  if (!(pid > 1)) return false; // -1/0 sentinel or a pid the kernel could recycle onto init
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/** True when the live process still looks like OUR child: the recorded spawn command must
 *  appear in `ps -o command=`. A recycled pid running someone else's program never matches,
 *  so kill(-pid) can never hit an unrelated process group (limitation B). */
function isOurChild(task: BgTask): boolean {
  if (!task.command) return false; // no recorded identity (legacy entry): refuse to group-kill
  return processCommandLine(task.pid).some((cmd) => cmd.includes('serve') && cmd.includes(task.worktree));
}

/** Parse the child's log for its session id and final result. */
function parseLog(logFile: string): { sessionId?: string; ok?: boolean } {
  try {
    let sessionId: string | undefined;
    let ok: boolean | undefined;
    for (const line of readFileSync(logFile, 'utf8').split('\n')) {
      if (!line.trim()) continue;
      try {
        const f = JSON.parse(line);
        if (f.type === 'hello' && f.sessionId) sessionId = f.sessionId;
        if (f.type === 'result' && f.id === 1) ok = f.ok === true;
      } catch {}
    }
    return { sessionId, ok };
  } catch {
    return {};
  }
}

/** Reconcile: mark dead running tasks done/failed and capture their session ids. */
export async function reconcileTasks(): Promise<BgTask[]> {
  const tasks = await readAll();
  let changed = false;
  for (const t of tasks) {
    if (t.status !== 'running') continue;
    if (!alive(t.pid)) {
      const { sessionId, ok } = parseLog(t.logFile);
      // no result frame at all: the child crashed or was killed — 'failed', not 'stopped'
      t.status = ok === true ? 'done' : ok === false ? 'failed' : 'failed';
      if (sessionId) t.sessionId = sessionId;
      t.endedAt = new Date().toISOString();
      changed = true;
    }
  }
  if (changed) await writeAll(tasks);
  return tasks;
}

export async function startBgTask(args: {
  prompt: string;
  name: string;
  model?: string;
  roleModel?: string; // resolved role model (subagent/longContext)
  mainCwd: string;
  currentModel: string;
  parent?: string;
}): Promise<BgTask> {
  const { worktreeCreate: create } = await import('./worktrees.js');
  const seq = Date.now().toString(36) + Math.random().toString(36).slice(2, 5);
  const wtName = `bg-${seq}`;
  const entry: WorktreeEntry = await create(args.mainCwd, wtName);
  await mkdir(logsDir(), { recursive: true });
  const logFile = join(logsDir(), `${wtName}.jsonl`);
  const model = args.roleModel ?? args.currentModel;

  // spawn a fresh serve in the worktree; TANGO_HOME inherits so sessions are shared
  // dev runs need the tsx loader; a bare `--import tsx` would resolve against the child's
  // cwd, so resolve it to an absolute path here. Built-bin runs need no loader.
  const cliEntry = resolve(process.argv[1]); // absolute: the child runs with cwd=worktree
  const interp: string[] = [];
  try {
    // a file:// URL, not a path: node reads --import as a URL, and `D:\…` is a scheme there
    interp.push('--import', pathToFileURL(createRequire(import.meta.url).resolve('tsx')).href);
  } catch {
    /* built bin: plain node runs the compiled output */
  }
  const env = { ...process.env };
  // never leak the parent's session identity into the child: TANGO_RESUME would make the
  // bg serve append to the MAIN session (two writers), TANGO_MODE/TANGO_MODEL override it (C2)
  for (const k of ['TANGO_RESUME', 'TANGO_MODE', 'TANGO_MODEL']) delete env[k];
  // the child runs alone in its own disposable worktree: auto mode is safe there and keeps
  // permission asks from hanging a task forever (I1)
  env.TANGO_MODE = 'auto';
  const childCommand = `${process.execPath} serve --cwd ${entry.path}`; // identity for kill validation
  const child = spawn(process.execPath, [...interp, cliEntry, 'serve', '-m', model, '--cwd', entry.path], {
    cwd: args.mainCwd,
    detached: DETACHED, // survives the parent serve; reaped via pid checks
    windowsHide: true,
    stdio: ['pipe', 'pipe', 'pipe'],
    env
  });
  child.on('error', () => {}); // spawn failure: the log stays empty, reconcile marks failed
  child.stdin.on('error', () => {}); // child died before our write flushed (C3)
  const out = createWriteStream(logFile);
  child.stdout.pipe(out);
  child.stderr.pipe(out); // diagnostics land in the same log (parseLog skips non-JSON)
  void createReadStream;
  child.stdin.write(JSON.stringify({ id: 1, cmd: 'prompt', text: args.prompt }) + '\n');
  child.stdin.end(); // EOF: the child finishes the prompt, then exits

  const task: BgTask = {
    id: seq,
    name: args.name || wtName,
    prompt: args.prompt,
    worktree: entry.path,
    worktreeName: wtName,
    pid: child.pid ?? -1,
    logFile,
    status: 'running',
    startedAt: new Date().toISOString(),
    model,
    command: childCommand,
    ...(args.parent ? { parent: args.parent } : {})
  };
  await writeAll([...(await readAll()), task]);
  return task;
}

export async function stopTask(id: string, mainCwd: string): Promise<BgTask> {
  void mainCwd;
  const task = await findTask(id);
  if (!task) throw new Error(`no such task: ${id}`);
  if (task.status !== 'running') return task; // done/failed history is not rewritten
  if (alive(task.pid)) {
    if (isOurChild(task)) killProcessTree(task.pid, 'SIGTERM'); // the process group, the tree on Windows
    // identity mismatch (pid reuse / hand-written registry entry): never group-kill strangers
  }
  await updateTask(task.id, { status: 'stopped', endedAt: new Date().toISOString() });
  // keep the session file; drop the worktree (it may be dirty — leave it for inspection)
  return (await findTask(task.id))!;
}

