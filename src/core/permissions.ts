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

const READ_TOOLS = new Set(['read', 'grep', 'find', 'ls', 'web_search', 'web_fetch', 'office_read', 'diff']);
const WRITE_TOOLS = new Set(['write', 'edit', 'office_write']);

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

// ---------- persistent allow rules (/permissions, design decision #6) ----------

/** A parsed allow rule: tool:<name> | path:<prefix> | bash:<prefix> */
export type AllowRule =
  | { kind: 'tool'; value: string }
  | { kind: 'path'; value: string }
  | { kind: 'bash'; value: string };

export type ParseResult = { ok: true; rule: AllowRule } | { ok: false; error: string };

export function parseRule(str: string): ParseResult {
  const s = str.trim();
  const m = /^(tool|path|bash):(.*)$/.exec(s);
  if (!m) return { ok: false, error: `rule must be tool:/path:/bash: — got "${s}"` };
  const value = m[2].trim();
  if (!value) return { ok: false, error: `empty prefix in "${s}"` };
  if (m[1] === 'tool') return { ok: true, rule: { kind: 'tool', value } };
  if (m[1] === 'bash') return { ok: true, rule: { kind: 'bash', value } };
  // '..' allowed: a path rule is an explicit user grant, including outside-cwd subtrees
  return { ok: true, rule: { kind: 'path', value: value.replace(/\/$/, '') } };
}

/** Does one rule allow this call? Path and bash match on segment/token boundaries. */
export function ruleMatches(rule: AllowRule, call: ToolCallBlock, cwd: string): boolean {
  const input = (call.input ?? {}) as Record<string, unknown>;
  if (rule.kind === 'tool') return call.name === rule.value;
  if (rule.kind === 'path') {
    if (!WRITE_TOOLS.has(call.name)) return false;
    // the rule prefix resolves against cwd (so ../sibling or absolute paths work too);
    // segment boundary: src matches src/a.ts, never src-evil/
    const target = resolve(cwd, String(input.path ?? ''));
    const prefix = resolve(cwd, rule.value);
    return target === prefix || target.startsWith(prefix + '/');
  }
  // bash: match the leading command tokens before ; && || |
  if (call.name !== 'bash') return false;
  const first = String(input.command ?? '').split(/[;|&]/)[0].trim();
  const prefix = rule.value;
  return first === prefix || first.startsWith(prefix + ' ');
}

export function decide(mode: PermissionMode, call: ToolCallBlock, cwd: string, rules?: AllowRule[]): Decision {
  if (mode === 'readonly') return readonlyDecision(call);
  if (mode === 'auto' || READ_TOOLS.has(call.name)) return { action: 'allow' };
  // persistent allow rules never override readonly (checked above), but beat ask
  if (rules?.some((r) => ruleMatches(r, call, cwd))) return { action: 'allow' };
  return decideBase(mode, call, cwd);
}

function readonlyDecision(call: ToolCallBlock): Decision {
  if (READ_TOOLS.has(call.name)) return { action: 'allow' };
  return { action: 'deny', reason: 'read-only mode' };
}

function decideBase(mode: PermissionMode, call: ToolCallBlock, cwd: string): Decision {
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
  opts: { mode: () => PermissionMode; cwd: string; ask?: AskFn; rules?: () => AllowRule[] }
): () => void {
  return events.on('before_tool', async ({ call }) => {
    const d = decide(opts.mode(), call, opts.cwd, opts.rules?.());
    if (d.action === 'allow') return;
    if (d.action === 'deny') return { block: true, reason: d.reason };
    if (!opts.ask) return { block: true, reason: `${d.reason}; needs confirmation (rerun with --yes to allow)` };
    if (await opts.ask(call, d.reason)) return;
    return { block: true, reason: `user declined (${d.reason})` };
  });
}
