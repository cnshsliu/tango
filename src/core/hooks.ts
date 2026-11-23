import { spawn } from 'node:child_process';
/**
 * Lifecycle hooks (Claude Code / grok-cli style). Config:
 *   "hooks": { "PreToolUse": [{ "matcher": "bash|edit", "command": "./check.sh", "timeout": 10 }] }
 * The nested form { matcher, hooks: [{ type: "command", command, timeout }] } works too.
 * Each command runs as `sh -c`, gets the event JSON on stdin. Exit 0 = ok, exit 2 = block
 * (stderr is the reason), anything else = a warning that never blocks. On exit 0 a JSON
 * object on stdout can say more: { decision: "block", reason } blocks like exit 2,
 * { additionalContext } adds text for the model, { continue: false, stopReason } ends the turn.
 */
export const HOOK_EVENTS = ['PreToolUse', 'PostToolUse', 'UserPromptSubmit', 'Stop', 'SessionStart', 'PreCompact', 'PostCompact'] as const;
export type HookEvent = (typeof HOOK_EVENTS)[number];

export interface HookCommand {
  /** regex over the tool name (tool events only); missing, '' or '*' = every tool */
  matcher?: string;
  command: string;
  /** seconds, default 10 */
  timeout?: number;
}

export type HookConfig = Partial<Record<HookEvent, HookCommand[]>>;

export interface HookOutcome {
  /** exit 2 or decision "block": the event is blocked; reason = stderr / JSON reason (or a default) */
  block?: string;
  /** additionalContext from every hook that ran, in order */
  context?: string[];
  /** continue: false: end the turn; the text is stopReason (or a default) */
  stop?: string;
}

/** What a hook said on stdout as JSON; anything that is not a JSON object says nothing. */
export interface HookJson {
  decision?: 'approve' | 'block';
  reason?: string;
  additionalContext?: string;
  continue?: boolean;
  stopReason?: string;
}

export function parseHookJson(stdout: string): HookJson {
  const t = stdout.trim();
  if (!t.startsWith('{')) return {};
  let o: any;
  try {
    o = JSON.parse(t);
  } catch {
    return {};
  }
  if (!o || typeof o !== 'object' || Array.isArray(o)) return {};
  const out: HookJson = {};
  if (o.decision === 'approve' || o.decision === 'block') out.decision = o.decision;
  if (typeof o.reason === 'string') out.reason = o.reason;
  if (typeof o.additionalContext === 'string' && o.additionalContext.trim()) out.additionalContext = o.additionalContext;
  if (typeof o.continue === 'boolean') out.continue = o.continue;
  if (typeof o.stopReason === 'string') out.stopReason = o.stopReason;
  return out;
}

/** Flatten one event's raw config (flat or nested entries) into commands; junk is skipped. */
export function parseHookEntries(raw: unknown): HookCommand[] {
  if (!Array.isArray(raw)) return [];
  const out: HookCommand[] = [];
  for (const e of raw) {
    if (!e || typeof e !== 'object') continue;
    const matcher = typeof e.matcher === 'string' ? e.matcher : undefined;
    const inner = Array.isArray(e.hooks) ? e.hooks : [e];
    for (const h of inner) {
      if (!h || typeof h.command !== 'string' || !h.command.trim()) continue;
      if (h.type !== undefined && h.type !== 'command') continue;
      const timeout = typeof h.timeout === 'number' && h.timeout > 0 ? h.timeout : undefined;
      out.push({ matcher, command: h.command, timeout });
    }
  }
  return out;
}

/** Merge several hook sources (config first, then plugins) into one config. */
export function mergeHookConfigs(sources: unknown[]): HookConfig {
  const merged: HookConfig = {};
  for (const src of sources) {
    if (!src || typeof src !== 'object') continue;
    for (const ev of HOOK_EVENTS) {
      const cmds = parseHookEntries((src as Record<string, unknown>)[ev]);
      if (cmds.length) (merged[ev] ??= []).push(...cmds);
    }
  }
  return merged;
}

/** Does `matcher` select `tool`? Anchored regex; a bad pattern matches nothing. */
export function hookMatches(matcher: string | undefined, tool: string | undefined): boolean {
  if (!matcher || matcher === '*' || tool === undefined) return true;
  try {
    return new RegExp(`^(?:${matcher})$`).test(tool);
  } catch {
    return false;
  }
}

/** Run one hook command; resolves with its exit code (null = killed or failed to start), stdout and stderr. */
function runOne(cmd: HookCommand, payload: unknown, cwd: string, env: NodeJS.ProcessEnv): Promise<{ code: number | null; stdout: string; stderr: string }> {
  return new Promise((resolve) => {
    // own process group: on timeout the whole tree dies, not just sh (a leftover child
    // would hold the stderr pipe open and stall the turn until it exits on its own)
    const child = spawn('sh', ['-c', cmd.command], { cwd, env, stdio: ['pipe', 'pipe', 'pipe'], detached: true });
    let stderr = '';
    let stdout = '';
    let settled = false;
    const finish = (code: number | null) => {
      if (settled) return;
      settled = true;
      clearTimeout(kill);
      resolve({ code, stdout, stderr });
    };
    const kill = setTimeout(() => {
      try {
        if (child.pid) process.kill(-child.pid, 'SIGKILL');
      } catch {
        child.kill('SIGKILL');
      }
      stderr += `timed out after ${cmd.timeout ?? 10}s`;
      finish(null);
    }, (cmd.timeout ?? 10) * 1000);
    child.stderr?.on('data', (d) => (stderr += d));
    // capped: only a small JSON object is meaningful here
    child.stdout?.on('data', (d) => {
      if (stdout.length < 100_000) stdout += d;
    });
    child.on('error', (e) => {
      stderr += e.message;
      finish(null);
    });
    child.on('close', (code) => finish(code));
    child.stdin.on('error', () => {}); // a hook that never reads stdin must not crash us
    child.stdin.end(JSON.stringify(payload));
  });
}

/**
 * Run every matching command for `event` in order. The first block (exit 2 or decision
 * "block") or continue: false stops the chain; other failures warn on stderr and the chain
 * goes on. additionalContext is collected from every hook that ran.
 */
export async function runHooks(
  cfg: HookConfig,
  event: HookEvent,
  payload: Record<string, unknown>,
  opts: { cwd: string; tool?: string; projectDir?: string }
): Promise<HookOutcome> {
  // $TANGO_PROJECT_DIR: project-relative hook scripts work from any subdirectory
  const env = { ...process.env, TANGO_PROJECT_DIR: opts.projectDir ?? opts.cwd };
  const context: string[] = [];
  for (const cmd of cfg[event] ?? []) {
    if (!hookMatches(cmd.matcher, opts.tool)) continue;
    const { code, stdout, stderr } = await runOne(cmd, { hook_event_name: event, ...payload }, opts.cwd, env);
    const withContext = (o: HookOutcome): HookOutcome => (context.length ? { ...o, context } : o);
    if (code === 2) return withContext({ block: stderr.trim().slice(0, 2000) || `blocked by ${event} hook` });
    if (code !== 0) {
      process.stderr.write(`[hook ${event}] exit ${code}: ${stderr}`.slice(0, 500) + '\n');
      continue;
    }
    const j = parseHookJson(stdout);
    if (j.additionalContext) context.push(j.additionalContext.trim().slice(0, 10_000));
    if (j.decision === 'block') return withContext({ block: (j.reason ?? '').trim().slice(0, 2000) || `blocked by ${event} hook` });
    if (j.continue === false) return withContext({ stop: (j.stopReason ?? '').trim().slice(0, 2000) || `stopped by ${event} hook` });
  }
  return context.length ? { context } : {};
}
