/**
 * Per-turn workspace checkpoints (design spec §3.1): one git snapshot before a
 * prompt's FIRST write-effect tool call. Installed on the shared event bus, so
 * REPL, serve and the task daemon all checkpoint identically.
 */
import { takeSnapshot } from './git-snapshot.js';
import type { EventBus } from './events.js';
import type { SessionStore } from './session.js';
import { writesWorkspace } from './types.js';

export interface CheckpointOpts {
  /** live: /cd moves the working directory mid-session */
  cwd: string | (() => string);
  store: SessionStore;
  /** injectable for tests */
  snapshot?: (cwd: string, sessionId: string) => Promise<string | null>;
}

/** Config gate: checkpoints are on unless `checkpoint: "off"`. */
export function checkpointEnabled(cfg: { checkpoint?: string }): boolean {
  return cfg.checkpoint !== 'off';
}

export function installCheckpoints(events: EventBus, opts: CheckpointOpts): void {
  const snap = opts.snapshot ?? ((cwd: string, sessionId: string) => takeSnapshot(cwd, sessionId));
  const cwdNow = () => (typeof opts.cwd === 'function' ? opts.cwd() : opts.cwd);
  let armed = false;
  let dirty = false; // a write happened this turn: close it with a post-turn snapshot
  events.on('prompt_start', () => {
    armed = true; // a fresh prompt re-arms; its first write tool snapshots
  });
  events.on('before_tool', async ({ call, tool }) => {
    if (!armed || !writesWorkspace(tool, call.input)) return;
    armed = false;
    dirty = true;
    try {
      const hash = await snap(cwdNow(), opts.store.id);
      if (hash) await opts.store.append({ kind: 'checkpoint', ts: new Date().toISOString(), hash });
    } catch {
      // a failed snapshot must never veto the tool or break the turn (spec RF#1)
    }
  });
  events.on('prompt_end', async () => {
    armed = false;
    if (!dirty) return;
    dirty = false;
    try {
      // the post-turn state: rewinding ACROSS this turn needs it to tell agent
      // leftovers (in this snapshot) from the user's own between-turn files
      const hash = await snap(cwdNow(), opts.store.id);
      if (hash) await opts.store.append({ kind: 'checkpoint', ts: new Date().toISOString(), hash, note: 'post' });
    } catch {
      // best effort only
    }
  });
}

/** Ordered checkpoint chain with the raw entry index each one landed at. */
export function latestCheckpoints(store: SessionStore): { hash: string; entryIndex: number }[] {
  return store
    .indexedEntries()
    .filter(({ entry }) => entry.kind === 'checkpoint')
    .map(({ entry, index }) => ({ hash: (entry as { hash: string }).hash, entryIndex: index }));
}
