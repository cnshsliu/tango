import { mkdir, readFile, readdir, rename, unlink, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { tangoDir } from './config.js';
import type { SessionStore } from './session.js';

/**
 * /handoff inbox. The sender never writes into another session's JSONL: that session may be
 * live in another window, whose in-memory entries (and rewind indices) would drift. Instead
 * each handoff is one file under <tango>/handoffs/<targetId>/, and whichever process owns the
 * target session merges it (on open, before each prompt, on history).
 */
export interface QueuedHandoff {
  from: string;
  summary: string;
  queuedAt: string;
}

export const handoffInbox = (targetId: string) => join(tangoDir(), 'handoffs', targetId);

export async function queueHandoff(targetId: string, h: Omit<QueuedHandoff, 'queuedAt'>): Promise<void> {
  const dir = handoffInbox(targetId);
  await mkdir(dir, { recursive: true });
  const name = `${Date.now()}-${randomUUID().slice(0, 8)}`;
  // write then rename: a draining reader never sees a half-written file
  await writeFile(join(dir, `${name}.tmp`), JSON.stringify({ ...h, queuedAt: new Date().toISOString() }), 'utf8');
  await rename(join(dir, `${name}.tmp`), join(dir, `${name}.json`));
}

/** Merge queued handoffs into `store` (oldest first); returns what was merged. */
export async function drainHandoffs(store: SessionStore): Promise<QueuedHandoff[]> {
  const dir = handoffInbox(store.id);
  if (!existsSync(dir)) return [];
  const merged: QueuedHandoff[] = [];
  for (const f of (await readdir(dir)).filter((n) => n.endsWith('.json')).sort()) {
    const path = join(dir, f);
    let h: QueuedHandoff;
    try {
      h = JSON.parse(await readFile(path, 'utf8'));
    } catch {
      continue; // unreadable: leave it for a human, never drop it silently
    }
    await store.addHandoff(h.from, h.summary, 'handoff');
    await unlink(path);
    merged.push(h);
  }
  return merged;
}
