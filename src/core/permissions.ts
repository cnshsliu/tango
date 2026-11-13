import { isAbsolute, relative, resolve } from 'node:path';
import type { EventBus } from './events.js';
import type { ToolCallBlock } from './types.js';

/**
 * - ask: reads always pass; writes inside cwd pass; risky bash and writes outside cwd ask first
 * - auto: everything passes (user opted in with --yes)
 * - readonly: reads pass; writes and bash are blocked
 */
export type PermissionMode = 'ask' | 'auto' | 'readonly';

export type Decision = { action: 'allow' } | { action: 'ask'; reason: string } | { action: 'deny'; reason: string };

const READ_TOOLS = new Set(['read', 'grep', 'find', 'ls']);
const WRITE_TOOLS = new Set(['write', 'edit']);

/** Commands that delete, rewrite history, publish, escalate, or run fetched code. */
const RISKY_BASH: [RegExp, string][] = [
  [/(^|[\s;&|(])rm\s/, 'deletes files'],
  [/(^|[\s;&|(])rmdir\s/, 'deletes directories'],
  [/(^|[\s;&|(])(mv|truncate|shred|dd|mkfs\S*)\s/, 'moves or overwrites data'],
  [/(^|[\s;&|(])sudo\s/, 'runs as root'],
  [/(^|[\s;&|(])(chmod|chown)\s/, 'changes permissions'],
  [/(^|[\s;&|(])(kill|pkill|killall)\s/, 'kills processes'],
  [/git\s+push/, 'publishes to a remote'],
  [/git\s+reset\s+--hard/, 'discards uncommitted work'],
  [/git\s+(clean|checkout\s+--|restore)\b/, 'discards local changes'],
  [/git\s+branch\s+-D/, 'deletes a branch'],
  [/(npm|pnpm|yarn)\s+publish/, 'publishes a package'],
  [/(curl|wget)[^|]*\|\s*(ba|z)?sh/, 'runs a downloaded script'],
  [/>\s*\/(etc|dev|usr|bin|System)\//, 'writes to a system path']
];

export function inside(cwd: string, path: string): boolean {
  const rel = relative(resolve(cwd), resolve(cwd, path));
  return rel === '' || (!rel.startsWith('..') && !isAbsolute(rel));
}

export function decide(mode: PermissionMode, call: ToolCallBlock, cwd: string): Decision {
  if (mode === 'auto' || READ_TOOLS.has(call.name)) return { action: 'allow' };
  const input = (call.input ?? {}) as Record<string, unknown>;

  if (WRITE_TOOLS.has(call.name)) {
    if (mode === 'readonly') return { action: 'deny', reason: 'read-only mode' };
    const path = String(input.path ?? '');
    if (inside(cwd, path)) return { action: 'allow' };
    return { action: 'ask', reason: `writes outside the working directory: ${resolve(cwd, path)}` };
  }

  if (call.name === 'bash') {
    if (mode === 'readonly') return { action: 'deny', reason: 'read-only mode' };
    const cmd = String(input.command ?? '');
    for (const [re, why] of RISKY_BASH) if (re.test(cmd)) return { action: 'ask', reason: why };
    return { action: 'allow' };
  }

  // tools added later default to asking until they declare otherwise
  return mode === 'readonly' ? { action: 'deny', reason: 'read-only mode' } : { action: 'ask', reason: `runs ${call.name}` };
}

/** Return true to allow. Undefined means no one can answer (non-interactive), which denies. */
export type AskFn = (call: ToolCallBlock, reason: string) => Promise<boolean>;

export function installPermissionGate(
  events: EventBus,
  opts: { mode: () => PermissionMode; cwd: string; ask?: AskFn }
): () => void {
  return events.on('before_tool', async ({ call }) => {
    const d = decide(opts.mode(), call, opts.cwd);
    if (d.action === 'allow') return;
    if (d.action === 'deny') return { block: true, reason: d.reason };
    if (!opts.ask) return { block: true, reason: `${d.reason}; needs confirmation (rerun with --yes to allow)` };
    if (await opts.ask(call, d.reason)) return;
    return { block: true, reason: `user declined (${d.reason})` };
  });
}
