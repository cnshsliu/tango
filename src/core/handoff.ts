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

/** Queue a brief for `targetId`; returns its queue id (for /handoff cancel). */
export async function queueHandoff(targetId: string, h: Omit<QueuedHandoff, 'queuedAt'>): Promise<string> {
  const dir = handoffInbox(targetId);
  await mkdir(dir, { recursive: true });
  const queueId = randomUUID().slice(0, 8);
  const name = `${Date.now()}-${queueId}`;
  // write then rename: a draining reader never sees a half-written file
  await writeFile(join(dir, `${name}.tmp`), JSON.stringify({ ...h, queuedAt: new Date().toISOString() }), 'utf8');
  await rename(join(dir, `${name}.tmp`), join(dir, `${name}.json`));
  return queueId;
}

/** A queued brief older than this is discarded (with a notice) instead of merged. */
export const HANDOFF_TTL_MS = 7 * 24 * 3600 * 1000;

const expired = (h: QueuedHandoff, now: number) => now - Date.parse(h.queuedAt) > HANDOFF_TTL_MS;

/** Queue id = the random part of the file name (`[ms]-[id].json`). */
const queueIdOf = (file: string) => file.replace(/\.json$/, '').split('-').slice(1).join('-');

export interface DrainResult {
  merged: QueuedHandoff[];
  expired: QueuedHandoff[];
}

/** Merge queued handoffs into `store` (oldest first); expired ones are deleted, not merged. */
export async function drainHandoffs(store: SessionStore, now = Date.now()): Promise<DrainResult> {
  const dir = handoffInbox(store.id);
  const res: DrainResult = { merged: [], expired: [] };
  if (!existsSync(dir)) return res;
  for (const f of (await readdir(dir)).filter((n) => n.endsWith('.json')).sort()) {
    const path = join(dir, f);
    // claim first: a concurrent /handoff cancel either wins (we skip) or loses (ENOENT for it)
    const claimed = path.replace(/\.json$/, '.claimed');
    try {
      await rename(path, claimed);
    } catch {
      continue;
    }
    let h: QueuedHandoff;
    try {
      h = JSON.parse(await readFile(claimed, 'utf8'));
    } catch {
      await rename(claimed, path).catch(() => {}); // unreadable: leave it for a human
      continue;
    }
    if (expired(h, now)) {
      res.expired.push(h);
    } else {
      await store.addHandoff(h.from, h.summary, 'handoff');
      res.merged.push(h);
    }
    await unlink(claimed).catch(() => {});
  }
  return res;
}

export interface PendingHandoff extends QueuedHandoff {
  queueId: string;
  target: string;
  expired: boolean;
}

/** Every brief still waiting in any inbox, oldest first. */
export async function listHandoffs(now = Date.now()): Promise<PendingHandoff[]> {
  const root = join(tangoDir(), 'handoffs');
  if (!existsSync(root)) return [];
  const out: PendingHandoff[] = [];
  for (const target of await readdir(root)) {
    const files = await readdir(join(root, target)).catch(() => [] as string[]);
    for (const f of files.filter((n) => n.endsWith('.json'))) {
      try {
        const h: QueuedHandoff = JSON.parse(await readFile(join(root, target, f), 'utf8'));
        out.push({ ...h, queueId: queueIdOf(f), target, expired: expired(h, now) });
      } catch {}
    }
  }
  return out.sort((a, b) => a.queuedAt.localeCompare(b.queuedAt));
}

/** Delete one queued brief by queue id (or unique prefix); returns what was removed. */
export async function cancelHandoff(ref: string): Promise<PendingHandoff> {
  const hits = (await listHandoffs()).filter((h) => h.queueId.startsWith(ref));
  if (!hits.length) throw new Error(`no queued handoff matches "${ref}" (already merged or cancelled?)`);
  if (hits.length > 1) throw new Error(`ambiguous "${ref}": ${hits.map((h) => h.queueId).join(', ')}`);
  const h = hits[0];
  const dir = handoffInbox(h.target);
  const file = (await readdir(dir)).find((n) => n.endsWith(`-${h.queueId}.json`));
  try {
    await unlink(join(dir, file ?? ''));
  } catch {
    throw new Error(`handoff ${h.queueId} was merged just now`);
  }
  return h;
}
