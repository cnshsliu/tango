import { existsSync } from 'node:fs';
import { mkdir, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { loadConfig, sessionsDir, tangoDir } from './config.js';
import { SessionStore } from './session.js';
import type { PermissionMode } from './permissions.js';

export interface Task {
  name: string;
  prompt: string;
  /** minutes between runs */
  everyMinutes: number;
  cwd?: string;
  /** headless: "ask" degrades to "auto"; defaults to "readonly" for safety */
  mode?: PermissionMode;
}

const FILE = () => join(tangoDir(), 'tasks.json');

export async function loadTasks(): Promise<Task[]> {
  if (!existsSync(FILE())) return [];
  try {
    return JSON.parse(await readFileText(FILE(), 'utf8')) as Task[];
  } catch (e: any) {
    throw new Error(`${FILE()}: invalid JSON — ${e.message}`);
  }
}
import { readFile as readFileText } from 'node:fs/promises';

async function save(tasks: Task[]): Promise<void> {
  await mkdir(tangoDir(), { recursive: true });
  await writeFile(FILE(), JSON.stringify(tasks, null, 2) + '\n', 'utf8');
}

/** "90m" | "2h" | "36h" | "45" (minutes) → minutes */
export function parseInterval(s: string): number {
  const m = /^(\d+(?:\.\d+)?)\s*(m|h|d)?$/i.exec(s.trim());
  if (!m) throw new Error(`bad interval "${s}" — use e.g. 90m, 2h, 36h or a plain minute count`);
  const n = Number(m[1]);
  return Math.max(1, Math.round(n * (m[2]?.toLowerCase() === 'h' ? 60 : m[2]?.toLowerCase() === 'd' ? 1440 : 1)));
}

export async function addTask(t: Task): Promise<void> {
  const tasks = await loadTasks();
  if (tasks.some((x) => x.name === t.name)) throw new Error(`task "${t.name}" already exists (tango task rm ${t.name})`);
  tasks.push(t);
  await save(tasks);
}

export async function removeTask(name: string): Promise<Task> {
  const tasks = await loadTasks();
  const i = tasks.findIndex((t) => t.name === name);
  if (i < 0) throw new Error(`no task "${name}" (tango task)`);
  const [gone] = tasks.splice(i, 1);
  await save(tasks);
  return gone;
}

/** Last run from the task's own session files; undefined when it never ran. */
export async function lastRunAt(name: string): Promise<{ ts: string; ok: boolean } | undefined> {
  const dir = sessionsDir();
  if (!existsSync(dir)) return;
  const prefix = `task-${slug(name)}-`;
  let last: { ts: string; ok: boolean } | undefined;
  for (const f of (await readdirNames(dir)).filter((f) => f.startsWith(prefix) && f.endsWith('.jsonl')).sort()) {
    const store = await SessionStore.open(dir, f.slice(0, -'.jsonl'.length));
    for (const e of store.entriesOf('task_run')) last = { ts: e.ts, ok: e.ok };
  }
  return last;
}

/** Sessions need a stable filesystem-safe name. */
export function slug(name: string): string {
  return name.toLowerCase().replace(/[^a-z0-9\u4e00-\u9fff_-]+/g, '-').replace(/^-+|-+$/g, '') || 'task';
}

async function readdirNames(dir: string): Promise<string[]> {
  const { readdir } = await import('node:fs/promises');
  return readdir(dir);
}

/** Tasks whose last run is older than their interval (never-run tasks are always due). */
export async function dueTasks(now = Date.now()): Promise<Task[]> {
  const out: Task[] = [];
  for (const t of await loadTasks()) {
    const last = await lastRunAt(t.name);
    if (!last || now - Date.parse(last.ts) >= t.everyMinutes * 60_000) out.push(t);
  }
  return out;
}

/** One line of the log, one line of the Feishu message. */
export function summarizeOutput(text: string, max = 800): string {
  const t = text.trim();
  return t.length > max ? t.slice(0, max) + '…' : t;
}

export interface RunResult {
  ok: boolean;
  output: string;
}

export interface TaskRunner {
  (task: Task): Promise<RunResult>;
}

/** Run every due task once; each run is recorded in its own session (clock injectable for tests). */
export async function runDueOnce(run: TaskRunner, now = Date.now(), clock = () => new Date(now)): Promise<number> {
  let ran = 0;
  for (const task of await dueTasks(now)) {
    let res: RunResult;
    try {
      res = await run(task);
    } catch (e: any) {
      res = { ok: false, output: String(e?.message ?? e) };
    }
    const dir = sessionsDir();
    await mkdir(dir, { recursive: true });
    const stamp = clock().toISOString().slice(0, 19).replace(/[:T]/g, '-');
    const store = await SessionStore.create(dir, task.cwd ?? process.cwd(), `task:${task.name}`, `task-${slug(task.name)}-${stamp}`, clock);
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: `[scheduled task "${task.name}"] ${task.prompt}` }] });
    await store.appendMessage({ role: 'assistant', content: [{ type: 'text', text: res.output }] });
    await store.append({ kind: 'task_run', ts: clock().toISOString(), ok: res.ok });
    ran++;
  }
  return ran;
}

// ---------- Feishu ----------

/** Send a markdown message to a Feishu/Lark custom-bot webhook. China-first: this is how most users get alerts. */
export async function sendFeishu(webhook: string, title: string, text: string, signal?: AbortSignal): Promise<void> {
  const res = await fetch(webhook, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    signal,
    body: JSON.stringify({ msg_type: 'interactive', card: { header: { title: { tag: 'plain_text', content: title } }, elements: [{ tag: 'markdown', content: text }] } })
  });
  const body = await res.text().catch(() => '');
  // Feishu answers 200 with {"code":N} even on failure
  let code = 0;
  try {
    code = JSON.parse(body).code ?? 0;
  } catch {}
  if (!res.ok || code) throw new Error(`feishu webhook failed (HTTP ${res.status}, code ${code}): ${body.slice(0, 200)}`);
}

export async function feishuWebhook(): Promise<string | undefined> {
  return (await loadConfig()).feishuWebhook;
}

/** Daemon body: run due tasks every pollMinutes until the signal fires. Returns when aborted. */
export async function daemonLoop(run: TaskRunner, pollMinutes: number, signal: AbortSignal): Promise<void> {
  while (!signal.aborted) {
    try {
      await runDueOnce(run);
    } catch (e: any) {
      console.error(`[tango task] ${e.message}`);
    }
    await new Promise<void>((done) => {
      const t = setTimeout(done, Math.max(5_000, pollMinutes * 60_000));
      signal.addEventListener('abort', () => (clearTimeout(t), done()), { once: true });
    });
  }
}
