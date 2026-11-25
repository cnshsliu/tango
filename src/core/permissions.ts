import { isAbsolute, relative, resolve } from 'node:path';
import type { EventBus } from './events.js';
import type { ToolCallBlock } from './types.js';

/**
 * - confirm: reads pass; every change asks first — file writes and every bash command
 *   (zcode's 变更前确认: "改文件前先问我", extended to commands)
 * - ask: reads always pass; writes inside cwd pass; risky bash and writes outside cwd ask first
 * - auto: everything passes (user opted in with --yes, after the risk acknowledgement)
 * - readonly: reads pass; writes and bash are blocked
 */
export type PermissionMode = 'confirm' | 'ask' | 'auto' | 'readonly';

export type Decision = { action: 'allow' } | { action: 'ask'; reason: string } | { action: 'deny'; reason: string };

const READ_TOOLS = new Set(['read', 'grep', 'find', 'ls', 'web_search', 'web_fetch', 'office_read', 'diff', 'session_search', 'process_logs', 'process_list']);
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
  // /dev/null is the universal sink — redirecting there is never a system write.
  // Raw devices (of=/dev/sda, > /dev/disk…) are the real danger.
  [/>\s*\/(etc|usr|bin|System)\//, 'writes to a system path'],
  [/>\s*\/dev\/(?!null\b)\S*/, 'writes to a device'],
  [/of=\/dev\/(?!null\b)\S*/, 'writes to a raw device']
];

/** Drop heredoc payloads from a command: the body is data written to a file,
 * never something the shell executes — risk rules must not scan it. The body runs
 * from the line after `<<WORD` to a line that is exactly WORD (tabs allowed for <<-),
 * wherever that is: commands may follow it. An unterminated body runs to the end. */
export function stripHeredocs(cmd: string): string {
  const lines = cmd.split('\n');
  const out: string[] = [];
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    out.push(line);
    // every <<WORD / <<'WORD' / <<-"WORD" on this line opens a body, in order
    // (<<< is a here-string: no body)
    const words = [...line.matchAll(/(?<!<)<<(?!<)(-?)\s*(['"]?)([\w.-]+)\2/g)].map((m) => ({ dash: m[1] === '-', word: m[3] }));
    for (const { dash, word } of words) {
      while (i + 1 < lines.length) {
        const body = lines[++i];
        if ((dash ? body.replace(/^\t+/, '') : body) === word) break;
      }
    }
  }
  return out.join('\n');
}

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
    let target = resolve(cwd, String(input.path ?? ''));
    let prefix = resolve(cwd, rule.value);
    if (process.platform === 'darwin') {
      // macOS filesystems are case-insensitive by default; rules should match like the fs does
      target = target.toLowerCase();
      prefix = prefix.toLowerCase();
    }
    return target === prefix || target.startsWith(prefix + '/');
  }
  // bash: the rule covers the leading command tokens before ; && || |
  if (call.name !== 'bash') return false;
  // same segmentation as the tail re-check: quotes respected, newlines separate
  const first = bashSegments(String(input.command ?? ''))[0] ?? '';
  const prefix = rule.value;
  return first === prefix || first.startsWith(prefix + ' ');
}

/** Tools whose second word is a subcommand (git branch …, npm publish …). */
const SUBCOMMAND_TOOLS = new Set([
  'git', 'npm', 'pnpm', 'yarn', 'bun', 'npx', 'pip', 'pip3', 'uv', 'brew', 'cargo',
  'go', 'docker', 'kubectl', 'helm', 'apt', 'apt-get', 'dnf', 'yum', 'systemctl',
  'service', 'gh', 'tango'
]);

/** The rule for one bash command segment: the command word plus its subcommand for
 * known multi-word tools (`git branch -D x` → `bash:git branch`; `rm -rf /tmp` → `bash:rm`).
 * Leading `NAME=value` assignments are skipped (`F="a.docx" && python3 …` is python3). */
/** Shell grammar words: they lead a segment but name no command (`do echo x` runs echo). */
const SHELL_KEYWORDS = new Set([
  'for', 'in', 'do', 'done', 'while', 'until', 'if', 'then', 'else', 'elif', 'fi',
  'case', 'esac', 'select', 'function', 'time', '!', '{', '}', '(', ')', '((', '))', '[[', ']]'
]);

function bashSegmentRule(seg: string): string | null {
  // "(cd x" / "{ ls" / "done)": grouping punctuation glued to a word is not part of it
  const toks = seg.replace(/^[({]+/, '').replace(/[)}]+$/, '').split(/\s+/).filter(Boolean);
  let cmd: string | undefined;
  for (let j = 0; j < toks.length; j++) {
    const t = toks[j];
    const name = t.split('=')[0];
    if (t.includes('=') && name && /^\w+$/.test(name)) continue;
    // `for x in a b` / `case $x in`: the whole header is grammar, no command in it
    if (t === 'for' || t === 'select' || t === 'case') return null;
    if (SHELL_KEYWORDS.has(t) || /^[)}]+$/.test(t)) continue;
    cmd = t;
    break;
  }
  if (!cmd) return null;
  // a word that cannot be a command name (quote fragments, code): nothing to remember
  if (!/^[\w./~+-][\w./~+@:-]*$/.test(cmd)) return null;
  const i = toks.indexOf(cmd);
  const sub = SUBCOMMAND_TOOLS.has(cmd) && /^\w[\w.-]*$/.test(toks[i + 1] ?? '') ? ` ${toks[i + 1]}` : '';
  return `bash:${cmd}${sub}`;
}

/** The rules a permission dialog's "a = always allow" saves for a call:
 * - bash: one rule per `; && || |` segment (heredoc bodies dropped, duplicates collapsed)
 *   — `osascript … && rm -f x` saves `bash:osascript` and `bash:rm`. The first rule alone
 *   would leave the risky tail uncovered, so the same command would ask again every time.
 * - write tools: the written file's directory (`path:<dir>`)
 * - anything else: the tool itself (`tool:<name>`)
 * Empty when there is nothing sensible to remember. */
export function suggestRules(call: ToolCallBlock): string[] {
  const name = call.name;
  const input = (call.input ?? {}) as Record<string, unknown>;
  if (name === 'bash') {
    const out: string[] = [];
    for (const seg of bashSegments(String(input.command ?? ''))) {
      const r = bashSegmentRule(seg);
      if (r && !out.includes(r)) out.push(r);
    }
    return out;
  }
  if (WRITE_TOOLS.has(name)) {
    const path = String(input.path ?? '').trim();
    if (!path) return [];
    const dir = path.includes('/') ? path.replace(/\/[^/]*$/, '') : '.';
    return [`path:${dir || '/'}`];
  }
  return [`tool:${name}`];
}

/** The first (leading-command) rule "always allow" remembers — `suggestRules(call)[0]`. */
export function suggestRule(call: ToolCallBlock): string | null {
  return suggestRules(call)[0] ?? null;
}

/** Segment a bash command into its ; && || | and newline parts, for tail re-checks.
 * Separators inside '…', "…", `…` or after a backslash are text, not separators
 * (`grep "a|b; c" f` is one segment). An `&` right after `>` belongs to the redirect
 * (`cmd 2>&1` is one segment: it used to leave a stray `1`). Heredoc bodies are dropped first. */
function bashSegments(command: string): string[] {
  const cmd = stripHeredocs(command);
  const out: string[] = [];
  let cur = '';
  let quote: string | null = null;
  for (let i = 0; i < cmd.length; i++) {
    const c = cmd[i];
    if (quote) {
      if (c === '\\' && quote !== "'" && i + 1 < cmd.length) { cur += c + cmd[++i]; continue; }
      if (c === quote) quote = null;
      cur += c;
      continue;
    }
    if (c === '\\' && i + 1 < cmd.length) { cur += c + cmd[++i]; continue; }
    if (c === "'" || c === '"' || c === '`') { quote = c; cur += c; continue; }
    if (c === '&' && cmd[i - 1] === '>') { cur += c; continue; }
    if (c === ';' || c === '|' || c === '&' || c === '\n') { out.push(cur); cur = ''; continue; }
    cur += c;
  }
  out.push(cur);
  return out.map((s) => s.trim()).filter(Boolean);
}

export function decide(mode: PermissionMode, call: ToolCallBlock, cwd: string, rules?: AllowRule[], extraDirs?: string[]): Decision {
  if (mode === 'readonly') return readonlyDecision(call);
  if (mode === 'auto' || READ_TOOLS.has(call.name)) return { action: 'allow' };
  // persistent allow rules never override readonly (checked above), but beat ask.
  // bash rules cover only the segments they match: a risky tail still asks (no smuggling).
  if (rules?.some((r) => ruleMatches(r, call, cwd))) {
    if (call.name === 'bash') {
      const segs = bashSegments(String((call.input as Record<string, unknown>)?.command ?? ''));
      const covered = (seg: string) => rules.some((r) => r.kind === 'bash' && (seg === r.value || seg.startsWith(r.value + ' ')));
      const riskyTail = segs.find(
        (seg, i) => i > 0 && !covered(seg) && RISKY_BASH.some(([re]) => re.test(stripHeredocs(seg)))
      );
      if (riskyTail) {
        const why = RISKY_BASH.find(([re]) => re.test(riskyTail))![1];
        return { action: 'ask', reason: `rule covers the prefix, but a later command ${why}: ${riskyTail.slice(0, 60)}` };
      }
    }
    return { action: 'allow' };
  }
  return decideBase(mode, call, cwd, extraDirs);
}

function readonlyDecision(call: ToolCallBlock): Decision {
  if (READ_TOOLS.has(call.name)) return { action: 'allow' };
  return { action: 'deny', reason: 'read-only mode' };
}

function decideBase(mode: PermissionMode, call: ToolCallBlock, cwd: string, extraDirs?: string[]): Decision {
  const input = (call.input ?? {}) as Record<string, unknown>;

  if (WRITE_TOOLS.has(call.name)) {
    if (mode === 'readonly') return { action: 'deny', reason: 'read-only mode' };
    const path = String(input.path ?? '');
    if (mode === 'confirm') return { action: 'ask', reason: `变更前确认：写入 ${resolve(cwd, path)}` };
    if (inside(cwd, path)) return { action: 'allow' };
    if (extraDirs?.some((d) => inside(d, path))) return { action: 'allow' };
    return { action: 'ask', reason: `writes outside the working directory: ${resolve(cwd, path)}` };
  }

  if (call.name === 'bash') {
    if (mode === 'readonly') return { action: 'deny', reason: 'read-only mode' };
    const cmd = String(input.command ?? '');
    // risk rules read the executable part only — heredoc bodies are file data
    const scanned = stripHeredocs(cmd);
    for (const [re, why] of RISKY_BASH) if (re.test(scanned)) return { action: 'ask', reason: why };
    if (mode === 'confirm') return { action: 'ask', reason: '变更前确认：每条命令都先问' };
    return { action: 'allow' };
  }

  // process_stop only ever signals processes this tango started (bash background: true)
  if (call.name === 'process_stop') return { action: 'allow' };

  // tools added later default to asking until they declare otherwise
  return mode === 'readonly' ? { action: 'deny', reason: 'read-only mode' } : { action: 'ask', reason: `runs ${call.name}` };
}

/** Return true to allow. Undefined means no one can answer (non-interactive), which denies. */
export type AskFn = (call: ToolCallBlock, reason: string) => Promise<boolean>;

export function installPermissionGate(
  events: EventBus,
  opts: {
    mode: () => PermissionMode;
    cwd: string | (() => string);
    ask?: AskFn;
    rules?: () => AllowRule[];
    extraDirs?: () => string[];
    plan?: () => boolean;
    /** runs before every decision, so rules can follow the config files and the current directory */
    refresh?: () => unknown;
  }
): () => void {
  const cwdNow = () => (typeof opts.cwd === 'function' ? opts.cwd() : opts.cwd);
  return events.on('before_tool', async ({ call, tool }) => {
    // plan mode vetoes ALL write-effect tools before rules can widen anything
    if (opts.plan?.() && tool.writeEffect) {
      return { block: true, reason: 'plan mode — research only; /plan off to execute' };
    }
    await opts.refresh?.();
    const d = decide(opts.mode(), call, cwdNow(), opts.rules?.(), opts.extraDirs?.());
    if (d.action === 'allow') return;
    if (d.action === 'deny') return { block: true, reason: d.reason };
    if (!opts.ask) return { block: true, reason: `${d.reason}; needs confirmation (rerun with --yes to allow)` };
    if (await opts.ask(call, d.reason)) return;
    return { block: true, reason: `user declined (${d.reason})` };
  });
}
