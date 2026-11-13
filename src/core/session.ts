import { appendFile, mkdir, readFile, readdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import type { Message, SessionEntry } from './types.js';

const now = () => new Date().toISOString();

export interface SessionMeta {
  id: string;
  createdAt: string;
  cwd: string;
  name?: string;
}

export interface SessionSummary extends SessionMeta {
  messages: number;
}

/**
 * Append-only JSONL session. One line per entry. Simple, greppable, hand-fixable.
 * Compaction appends a summary entry; messages before it are dropped from model context.
 * Layout: <dir>/<id>.jsonl
 */
export class SessionStore {
  private entries: SessionEntry[] = [];

  private constructor(
    readonly dir: string,
    readonly id: string
  ) {}

  get file(): string {
    return join(this.dir, `${this.id}.jsonl`);
  }

  static async create(dir: string, cwd: string, name?: string): Promise<SessionStore> {
    await mkdir(dir, { recursive: true });
    const store = new SessionStore(dir, randomUUID());
    await store.append({ kind: 'meta', id: store.id, createdAt: now(), cwd, name });
    return store;
  }

  static async open(dir: string, id: string): Promise<SessionStore> {
    const store = new SessionStore(dir, id);
    if (!existsSync(store.file)) throw new Error(`session not found: ${store.file}`);
    const raw = await readFile(store.file, 'utf8');
    for (const line of raw.split('\n')) {
      if (!line.trim()) continue;
      // tolerate a torn last line from a killed process
      try {
        store.entries.push(JSON.parse(line));
      } catch {}
    }
    return store;
  }

  /** All sessions, newest first. Skips files without a valid meta line. */
  static async list(dir: string): Promise<SessionSummary[]> {
    if (!existsSync(dir)) return [];
    const out: SessionSummary[] = [];
    for (const f of await readdir(dir)) {
      if (!f.endsWith('.jsonl')) continue;
      let meta: SessionMeta | undefined;
      let messages = 0;
      for (const line of (await readFile(join(dir, f), 'utf8')).split('\n')) {
        if (!line.trim()) continue;
        try {
          const e: SessionEntry = JSON.parse(line);
          if (e.kind === 'meta') meta = e;
          if (e.kind === 'message') messages++;
        } catch {}
      }
      if (meta) out.push({ ...meta, id: meta.id ?? f.slice(0, -'.jsonl'.length), messages });
    }
    return out.sort((a, b) => b.createdAt.localeCompare(a.createdAt));
  }

  async append(entry: SessionEntry): Promise<void> {
    this.entries.push(entry);
    await appendFile(this.file, JSON.stringify(entry) + '\n', 'utf8');
  }

  appendMessage(message: Message): Promise<void> {
    return this.append({ kind: 'message', ts: now(), message });
  }

  setModel(modelId: string): Promise<void> {
    return this.append({ kind: 'model', ts: now(), modelId });
  }

  /** Messages currently visible to the model (summary + everything after last compaction). */
  messageCount(): number {
    return this.messages().length;
  }

  async compact(summary: string): Promise<void> {
    const droppedCount = this.messageCount();
    await this.append({ kind: 'compaction', ts: now(), summary, droppedCount });
  }

  meta(): SessionMeta | undefined {
    return this.entries.find((e) => e.kind === 'meta');
  }

  /** Most recent explicit model switch, if any. */
  lastModel(): string | undefined {
    for (let i = this.entries.length - 1; i >= 0; i--) {
      const e = this.entries[i];
      if (e.kind === 'model') return e.modelId;
    }
  }

  /**
   * Messages for model context: everything after the latest compaction,
   * with its summary injected as a leading user message.
   */
  messages(): Message[] {
    let start = -1;
    for (let i = this.entries.length - 1; i >= 0; i--) {
      if (this.entries[i].kind === 'compaction') {
        start = i;
        break;
      }
    }
    const out: Message[] = [];
    if (start >= 0) {
      const e = this.entries[start];
      if (e.kind === 'compaction') {
        out.push({
          role: 'user',
          content: [{ type: 'text', text: `<summary of earlier conversation>\n${e.summary}\n</summary>` }]
        });
      }
    }
    for (let i = start + 1; i < this.entries.length; i++) {
      const e = this.entries[i];
      if (e.kind === 'message') out.push(e.message);
    }
    return out;
  }
}
