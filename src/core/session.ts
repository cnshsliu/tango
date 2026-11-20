import { appendFile, mkdir, readFile, readdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import type { Message, SessionEntry } from './types.js';

export interface SessionMeta {
  id: string;
  createdAt: string;
  cwd: string;
  name?: string;
  forkOf?: { sessionId: string; entryIndex: number };
}

export interface SessionSummary extends SessionMeta {
  messages: number;
  /** first user text prompt, truncated — the human-meaningful identity in lists */
  topic: string;
}

/**
 * Append-only JSONL session. One line per entry. Simple, greppable, hand-fixable.
 * Compaction appends a summary entry; messages before it are dropped from model context.
 * Layout: <dir>/<id>.jsonl
 */
export class SessionStore {
  private entries: SessionEntry[] = [];
  /** all timestamps come from here; scheduled runs inject the daemon's notion of "now" */
  private clock: () => Date = () => new Date();

  private constructor(
    readonly dir: string,
    readonly id: string
  ) {}

  get file(): string {
    return join(this.dir, `${this.id}.jsonl`);
  }

  private now(): string {
    return this.clock().toISOString();
  }

  static async create(dir: string, cwd: string, name?: string, id?: string, clock?: () => Date): Promise<SessionStore> {
    await mkdir(dir, { recursive: true });
    const store = new SessionStore(dir, id ?? randomUUID());
    if (clock) store.clock = clock;
    await store.append({ kind: 'meta', id: store.id, createdAt: store.now(), cwd, name });
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
      let topic = '';
      for (const line of (await readFile(join(dir, f), 'utf8')).split('\n')) {
        if (!line.trim()) continue;
        try {
          const e: SessionEntry = JSON.parse(line);
          if (e.kind === 'meta') meta = e;
          if (e.kind === 'message') {
            messages++;
            // the topic is the first user *text* prompt — tool results also come
            // as user-role messages but carry no intent
            if (!topic && e.message.role === 'user') {
              const block: any = e.message.content.find((b) => b.type === 'text');
              const text: string = block?.text ?? '';
              if (text.trim()) topic = text.trim().slice(0, 80);
            }
          }
        } catch {}
      }
      if (meta)
        out.push({ ...meta, id: meta.id ?? f.slice(0, -'.jsonl'.length), messages, topic });
    }
    return out.sort((a, b) => b.createdAt.localeCompare(a.createdAt));
  }

  async append(entry: SessionEntry): Promise<void> {
    this.entries.push(entry);
    await appendFile(this.file, JSON.stringify(entry) + '\n', 'utf8');
  }

  appendMessage(message: Message): Promise<void> {
    return this.append({ kind: 'message', ts: this.now(), message });
  }

  setModel(modelId: string): Promise<void> {
    return this.append({ kind: 'model', ts: this.now(), modelId });
  }

  /**
   * Rewind the visible projection to `entryIndex` (inclusive) by appending a
   * rewind entry — the log itself never loses the abandoned future.
   */
  rewindTo(entryIndex: number): Promise<void> {
    return this.append({ kind: 'rewind', ts: this.now(), to: entryIndex });
  }

  /**
   * Watermark projection: the last rewind entry (at index R, target T) abandons
   * entries (T, R] — the discarded future, kept in the log but invisible to the
   * model. Entries appended after R are new-timeline work and visible again.
   * A rewind "forward" (redo) re-includes a previously abandoned range.
   */
  visibleEntries(): SessionEntry[] {
    for (let i = this.entries.length - 1; i >= 0; i--) {
      if (this.entries[i].kind === 'rewind') {
        const to = (this.entries[i] as { to: number }).to;
        return [...this.entries.slice(0, to + 1), ...this.entries.slice(i + 1)];
      }
    }
    return this.entries;
  }

  /** Copy entries[0..toIndex] into a brand-new fork session file. */
  async forkTo(dir: string, newId: string, toIndex: number, name?: string): Promise<SessionStore> {
    const meta = this.meta();
    // rewinds stay: dropping them would resurrect abandoned futures in the fork (I2)
    const prefix = this.entries.slice(0, toIndex + 1).filter((e) => e.kind !== 'meta');
    const metaEntry: SessionEntry = {
      kind: 'meta',
      id: newId,
      createdAt: this.now(),
      cwd: meta?.cwd ?? '',
      name: name ?? meta?.name,
      forkOf: { sessionId: this.id, entryIndex: toIndex }
    };
    const fork = new SessionStore(dir, newId);
    fork.entries = [metaEntry, ...prefix];
    await mkdir(dir, { recursive: true });
    await appendFile(fork.file, fork.entries.map((e) => JSON.stringify(e) + '\n').join(''), 'utf8');
    return fork;
  }

  /** Merge a brief of session `from` into this session's context. */
  addHandoff(from: string, summary: string, via: 'handoff' | 'takeover'): Promise<void> {
    return this.append({ kind: 'handoff', ts: this.now(), from, summary, via });
  }

  /** Messages currently visible to the model (summary + everything after last compaction). */
  messageCount(): number {
    return this.messages().length;
  }

  async compact(summary: string): Promise<void> {
    const droppedCount = this.messageCount();
    await this.append({ kind: 'compaction', ts: this.now(), summary, droppedCount });
  }

  /** All messages of a given entry kind (e.g. task_run records). */
  entriesOf<K extends SessionEntry['kind']>(kind: K): Extract<SessionEntry, { kind: K }>[] {
    return this.entries.filter((e): e is Extract<SessionEntry, { kind: K }> => e.kind === kind);
  }

  /** Raw entries with their stable append-order index (rewind targets reference these). */
  indexedEntries(): { entry: SessionEntry; index: number }[] {
    return this.entries.map((entry, index) => ({ entry, index }));
  }

  /** Full history, ignoring compaction (used by /save-as-skill). */
  allMessages(): Message[] {
    return this.entries.filter((e) => e.kind === 'message').map((e) => (e as { message: Message }).message);
  }

  /** Full history of the CURRENT projection, ignoring compaction (what the UI shows). */
  allVisibleMessages(): Message[] {
    return this.visibleEntries().filter((e) => e.kind === 'message').map((e) => (e as { message: Message }).message);
  }

  meta(): SessionMeta | undefined {
    // the LAST meta wins: /rename appends a new one and list()/tree must agree
    let meta: SessionMeta | undefined;
    for (const e of this.entries) if (e.kind === 'meta') meta = e;
    return meta;
  }

  /** Most recent explicit model switch, if any. */
  lastModel(): string | undefined {
    for (let i = this.entries.length - 1; i >= 0; i--) {
      const e = this.entries[i];
      if (e.kind === 'model') return e.modelId;
      // sessions started with -m never record a switch; use the model that last answered
      if (e.kind === 'message' && e.message.role === 'assistant' && e.message.model) return e.message.model;
    }
  }

  /**
   * Messages for model context: everything after the latest compaction,
   * with its summary injected as a leading user message. Runs on the
   * watermark projection, so a rewind past a compaction brings the
   * pre-compaction messages back.
   */
  messages(): Message[] {
    const visible = this.visibleEntries();
    let start = -1;
    for (let i = visible.length - 1; i >= 0; i--) {
      if (visible[i].kind === 'compaction') {
        start = i;
        break;
      }
    }
    const out: Message[] = [];
    if (start >= 0) {
      const e = visible[start];
      if (e.kind === 'compaction') {
        out.push({
          role: 'user',
          content: [{ type: 'text', text: `<summary of earlier conversation>\n${e.summary}\n</summary>` }]
        });
      }
    }
    for (let i = start + 1; i < visible.length; i++) {
      const e = visible[i];
      if (e.kind === 'message') out.push(e.message);
      else if (e.kind === 'handoff')
        out.push({
          role: 'user',
          content: [{ type: 'text', text: `<handoff from session ${e.from}>\n${e.summary}\n</handoff>` }]
        });
    }
    return out;
  }
}
