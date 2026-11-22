import { spawn } from 'node:child_process';
/**
 * Lifecycle hooks (Claude Code / grok-cli style). Config:
 *   "hooks": { "PreToolUse": [{ "matcher": "bash|edit", "command": "./check.sh", "timeout": 10 }] }
 * The nested form { matcher, hooks: [{ type: "command", command, timeout }] } works too.
 * Each command runs as `sh -c`, gets the event JSON on stdin. Exit 0 = ok, exit 2 = block
 * (stderr is the reason), anything else = a warning that never blocks.
 */
export const HOOK_EVENTS = ['PreToolUse', 'PostToolUse', 'UserPromptSubmit', 'Stop'] as const;
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
  /** exit 2: the event is blocked; reason = stderr (or a default) */
  block?: string;
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

/** Run one hook command; resolves with its exit code (null = killed or failed to start) and stderr. */
function runOne(cmd: HookCommand, payload: unknown, cwd: string, env: NodeJS.ProcessEnv): Promise<{ code: number | null; stderr: string }> {
  return new Promise((resolve) => {
    // own process group: on timeout the whole tree dies, not just sh (a leftover child
    // would hold the stderr pipe open and stall the turn until it exits on its own)
    const child = spawn('sh', ['-c', cmd.command], { cwd, env, stdio: ['pipe', 'ignore', 'pipe'], detached: true });
    let stderr = '';
    let settled = false;
    const finish = (code: number | null) => {
      if (settled) return;
      settled = true;
      clearTimeout(kill);
      resolve({ code, stderr });
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
 * Run every matching command for `event` in order. The first exit 2 blocks and stops
 * the chain; other failures warn on stderr and the chain goes on.
 */
export async function runHooks(
  cfg: HookConfig,
  event: HookEvent,
  payload: Record<string, unknown>,
  opts: { cwd: string; tool?: string; projectDir?: string }
): Promise<HookOutcome> {
  // $TANGO_PROJECT_DIR: project-relative hook scripts work from any subdirectory
  const env = { ...process.env, TANGO_PROJECT_DIR: opts.projectDir ?? opts.cwd };
  for (const cmd of cfg[event] ?? []) {
    if (!hookMatches(cmd.matcher, opts.tool)) continue;
    const { code, stderr } = await runOne(cmd, { hook_event_name: event, ...payload }, opts.cwd, env);
    if (code === 2) return { block: stderr.trim().slice(0, 2000) || `blocked by ${event} hook` };
    if (code !== 0) process.stderr.write(`[hook ${event}] exit ${code}: ${stderr}`.slice(0, 500) + '\n');
  }
  return {};
}
