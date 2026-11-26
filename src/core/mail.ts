import { mkdir, readFile, readdir, rename, unlink, writeFile } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { randomUUID } from 'node:crypto';
import { tangoDir } from './config.js';
import type { SessionStore } from './session.js';

/**
 * /send inbox: short messages between live tango sessions, unlike /handoff's work briefs.
 * The sender never writes into another session's JSONL (that session may be open in another
 * window, whose in-memory entries would drift): each message is one file under
 * [tango]/messages/<targetId>/, and whichever process owns the target session merges it —
 * on open, before each prompt, or within ~2s while idle (a poll; see serve.ts).
 */
export interface QueuedMail {
  from: string;
  text: string;
  queuedAt: string;
}

/** Messages are capped (the /send command refuses longer text); a brief is /handoff's job. */
export const MAIL_MAX_CHARS = 4000;

export const mailInbox = (targetId: string) => join(tangoDir(), 'messages', targetId);

/** The time stamp of the last queued message's name: names must increase even within one millisecond. */
let lastStamp = 0;

/**
 * Queue a message for `targetId`. The file name starts with the time, and a drain merges oldest
 * first, so two messages queued in the same millisecond would otherwise trade places with every
 * other run (their names differ only by the random suffix). The stamp therefore never goes back:
 * within this process it is "now", or one more than the last one. `queuedAt` keeps the real time.
 */
export async function sendMail(targetId: string, m: Omit<QueuedMail, 'queuedAt'>): Promise<void> {
  const dir = mailInbox(targetId);
  await mkdir(dir, { recursive: true });
  const stamp = (lastStamp = Date.now() > lastStamp ? Date.now() : lastStamp + 1);
  const name = `${stamp}-${randomUUID().slice(0, 8)}`;
  // write then rename: a draining reader never sees a half-written file
  await writeFile(join(dir, `${name}.tmp`), JSON.stringify({ ...m, queuedAt: new Date().toISOString() }), 'utf8');
  await rename(join(dir, `${name}.tmp`), join(dir, `${name}.json`));
}

/** A message older than this is discarded (with a notice) instead of merged. */
export const MAIL_TTL_MS = 7 * 24 * 3600 * 1000;

const expired = (m: QueuedMail, now: number) => now - Date.parse(m.queuedAt) > MAIL_TTL_MS;

export interface DrainMailResult {
  received: QueuedMail[];
  expired: QueuedMail[];
}

/**
 * Merge queued messages into `store` (oldest first); expired ones are deleted, not merged.
 * Arriving mail is stored as a `mail` entry — shown in the chat and part of the model's
 * context from the next request on, but it never starts a turn by itself (no agent loops).
 */
export async function drainMail(store: SessionStore, now = Date.now()): Promise<DrainMailResult> {
  const dir = mailInbox(store.id);
  const res: DrainMailResult = { received: [], expired: [] };
  if (!existsSync(dir)) return res;
  for (const f of (await readdir(dir)).filter((n) => n.endsWith('.json')).sort()) {
    const path = join(dir, f);
    // claim first: a concurrent drain (same session opened twice) skips what we hold
    const claimed = path.replace(/\.json$/, '.claimed');
    try {
      await rename(path, claimed);
    } catch {
      continue;
    }
    let m: QueuedMail;
    try {
      m = JSON.parse(await readFile(claimed, 'utf8'));
    } catch {
      await rename(claimed, path).catch(() => {}); // unreadable: leave it for a human
      continue;
    }
    if (expired(m, now)) {
      res.expired.push(m);
    } else {
      await store.addMail(m.from, String(m.text ?? '').slice(0, MAIL_MAX_CHARS));
      res.received.push(m);
    }
    await unlink(claimed).catch(() => {});
  }
  return res;
}

/** Every message still waiting in any inbox, oldest first (for /send list-style debugging). */
export async function listMail(now = Date.now()): Promise<(QueuedMail & { target: string })[]> {
  const root = join(tangoDir(), 'messages');
  if (!existsSync(root)) return [];
  const out: (QueuedMail & { target: string })[] = [];
  for (const target of await readdir(root)) {
    const files = await readdir(join(root, target)).catch(() => [] as string[]);
    for (const f of files.filter((n) => n.endsWith('.json'))) {
      try {
        const m: QueuedMail = JSON.parse(await readFile(join(root, target, f), 'utf8'));
        out.push({ ...m, target, queuedAt: m.queuedAt });
      } catch {}
    }
  }
  return out.sort((a, b) => a.queuedAt.localeCompare(b.queuedAt)).filter((m) => !expired(m, now));
}
