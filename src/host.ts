import { spawn, spawnSync } from 'node:child_process';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
/**
 * Shared host wiring: every frontend (terminal REPL, serve protocol, task daemon)
 * assembles its agent here so tools, permissions and diagnostics stay identical.
 * Nothing in this file prints to stdout except where a caller passes a printer.
 */
import { Agent } from './core/agent.js';
import { checkpointEnabled, installCheckpoints } from './core/checkpoint.js';
import { keyProblem, loadModels, pickModel, sessionsDir, tangoDir } from './core/config.js';
import { installDiagnostics } from './core/diagnostics.js';
import { EventBus } from './core/events.js';
import { mergeHookConfigs, runHooks, type HookConfig } from './core/hooks.js';
import { resolveConfig, type Resolved } from './core/project-config.js';
import { worktreeRoot } from './core/worktrees.js';
import { installPermissionGate, parseRule, type AllowRule, type AskFn, type PermissionMode } from './core/permissions.js';
import { SessionStore } from './core/session.js';
import { ToolRegistry } from './core/tools/registry.js';
import { bashTool } from './core/tools/bash.js';
import { diffTool } from './core/tools/diff.js';
import { editTool, readTool, writeTool } from './core/tools/fs.js';
import { officeReadTool, officeWriteTool } from './core/tools/office/index.js';
import { notifyTool } from './core/tools/notify.js';
import { findTool, grepTool, lsTool } from './core/tools/search.js';
import { sessionSearchTool } from './core/tools/session-search.js';
import { webFetchTool } from './core/tools/web-fetch.js';
import { webSearchTool } from './core/tools/web-search.js';
import type { ModelSpec } from './core/types.js';

export const VERSION = '0.1.0';

export function buildTools(): ToolRegistry {
  const tools = new ToolRegistry();
  for (const t of [readTool, writeTool, editTool, bashTool, grepTool, findTool, lsTool, webSearchTool, webFetchTool, officeReadTool, officeWriteTool, notifyTool, diffTool, sessionSearchTool])
    tools.register(t);
  return tools;
}

/** Live permission state a host mutates (REPL /mode, serve set_mode, /permissions). */
export interface PermState {
  mode: PermissionMode;
  ask?: AskFn;
  /** live allow rules (parsed from config; /permissions mutates the array in place) */
  rules?: { allow: string[] };
  /** live extra write roots (/add_dir; inside() also accepts these) */
  extraDirs?: { dirs: string[] };
  /** plan mode: write-effect tools are vetoed (/plan) */
  plan?: { on: boolean };
}

/** Run a configured hook: `sh -c <cmd>` with the payload JSON on stdin; failures warn, never block. */
function runHook(script: string, payload: unknown): void {
  // async spawn keeps the event loop (and interrupts) alive during hooks
  const child = spawn('sh', ['-c', script], { stdio: ['pipe', 'ignore', 'pipe'] });
  const kill = setTimeout(() => child.kill('SIGKILL'), 5000);
  child.on('error', (e) => {
    clearTimeout(kill);
    process.stderr.write(`[hook] ${e.message}\n`);
  });
  let err = '';
  child.stderr?.on('data', (d) => (err += d));
  child.on('close', (code) => {
    clearTimeout(kill);
    if (code !== 0) process.stderr.write(`[hook] exit ${code}: ${err}`.slice(0, 500) + '\n');
  });
  child.stdin.write(JSON.stringify(payload));
  child.stdin.end();
}

/** Local plugins (~/.tango/plugins/<name>/plugin.json): hooks chain in, enabled:false skips. */
export function loadPluginHooks(): {
  plugins: { name: string; description: string; enabled: boolean }[];
  hooks: { beforeTool?: string[]; afterTool?: string[] };
  /** lifecycle hooks (PreToolUse, Stop, …) from each enabled plugin, in order */
  lifecycle: unknown[];
} {
  const plugins: { name: string; description: string; enabled: boolean }[] = [];
  const hooks: { beforeTool?: string[]; afterTool?: string[] } = {};
  const lifecycle: unknown[] = [];
  const dir = join(tangoDir(), 'plugins');
  let names: string[] = [];
  try {
    names = readdirSync(dir).filter((d) => d !== '.DS_Store').sort(); // deterministic hook merge order
  } catch {
    return { plugins, hooks, lifecycle };
  }
  for (const name of names) {
    let meta: any;
    try {
      meta = JSON.parse(readFileSync(join(dir, name, 'plugin.json'), 'utf8'));
    } catch {
      plugins.push({ name, description: 'plugin.json 解析失败', enabled: false }); // visible, not silent
      continue;
    }
    const enabled = meta.enabled !== false;
    plugins.push({ name: meta.name ?? name, description: meta.description ?? '', enabled });
    if (!enabled) continue;
    // every enabled plugin's hook runs — a chain, not first-wins (limitation D)
    if (meta.hooks?.beforeTool) (hooks.beforeTool ??= []).push(meta.hooks.beforeTool);
    if (meta.hooks?.afterTool) (hooks.afterTool ??= []).push(meta.hooks.afterTool);
    if (meta.hooks) lifecycle.push(meta.hooks);
  }
  return { plugins, hooks, lifecycle };
}

/** Wire PreToolUse / PostToolUse / UserPromptSubmit / Stop hooks onto the event bus. */
function installLifecycleHooks(events: EventBus, hooks: HookConfig, ctx: { cwd: () => string; sessionId: () => string; projectDir?: string }): void {
  const base = () => ({ session_id: ctx.sessionId(), cwd: ctx.cwd() });
  const run = (ev: keyof HookConfig, payload: Record<string, unknown>, tool?: string) =>
    runHooks(hooks, ev, { ...base(), ...payload }, { cwd: ctx.cwd(), tool, projectDir: ctx.projectDir ?? ctx.cwd() });
  if (hooks.PreToolUse) {
    events.on('before_tool', async ({ call, tool }) => {
      const r = await run('PreToolUse', { tool_name: tool.name, tool_input: call.input }, tool.name);
      if (r.block) return { block: true, reason: `PreToolUse hook: ${r.block}` };
    });
  }
  if (hooks.PostToolUse) {
    events.on('after_tool', async ({ call, result }) => {
      const text = result.content.map((b) => (b.type === 'text' ? b.text : '')).join('').slice(0, 20_000);
      const r = await run('PostToolUse', { tool_name: call.name, tool_input: call.input, tool_response: { text, is_error: !!result.isError } }, call.name);
      // exit 2 after the fact: the tool already ran, so the hook's word goes to the model
      if (r.block) result.content.push({ type: 'text', text: `\n[PostToolUse hook] ${r.block}` });
    });
  }
  if (hooks.UserPromptSubmit) {
    events.on('before_prompt', async ({ text }) => {
      const r = await run('UserPromptSubmit', { prompt: text });
      if (r.block) return { block: true, reason: r.block };
    });
  }
  if (hooks.Stop) {
    events.on('before_stop', async ({ message, continued }) => {
      const last = message.content.map((b) => (b.type === 'text' ? b.text : '')).join('');
      const r = await run('Stop', { stop_hook_active: continued, last_assistant_message: last.slice(0, 20_000) });
      if (r.block) return { block: true, reason: r.block };
    });
  }
}

export function makeAgent(model: ModelSpec, store: SessionStore, cwd: string, perm: PermState, cfg: { checkpoint?: string; permissions?: { allow: string[] }; extraDirs?: string[]; hooks?: { beforeTool?: string; afterTool?: string } & Record<string, unknown>; maxTurns?: number; projectDir?: string } = {}): Agent {
  const events = new EventBus();
  // gate first: a vetoed call is never announced as running
  const rules = perm.rules ?? { allow: cfg.permissions?.allow ? [...cfg.permissions.allow] : [] };
  perm.rules = rules;
  const extraDirs = perm.extraDirs ?? { dirs: cfg.extraDirs ? [...cfg.extraDirs] : [] };
  perm.extraDirs = extraDirs;
  const plan = perm.plan ?? { on: false };
  perm.plan = plan;
  const parsedRules = () =>
    rules.allow
      .map((r) => parseRule(r))
      .filter((r): r is { ok: true; rule: AllowRule } => r.ok)
      .map((r) => r.rule);
  const maxTurns = Number.isFinite(cfg.maxTurns) && (cfg.maxTurns as number) >= 1 && (cfg.maxTurns as number) <= 500
    ? Math.floor(cfg.maxTurns as number)
    : undefined;
  const agent = new Agent({ store, tools: buildTools(), model, cwd, events, maxTurns });
  const plugins = loadPluginHooks();
  // lifecycle hooks before the gate: a policy hook can refuse a call before anyone is asked
  installLifecycleHooks(events, mergeHookConfigs([cfg.hooks, ...plugins.lifecycle]), {
    cwd: () => agent.cwd,
    projectDir: cfg.projectDir,
    sessionId: () => store.id
  });
  installPermissionGate(events, {
    mode: () => perm.mode,
    cwd: () => agent.cwd,
    ask: (c, r) => (perm.ask ? perm.ask(c, r) : Promise.resolve(false)),
    rules: parsedRules,
    // managed worktrees (~/.tango/worktrees) are tango's own isolation checkouts:
    // writing there is the point of creating one, so it never asks
    extraDirs: () => [...extraDirs.dirs, worktreeRoot()],
    plan: () => plan.on
  });
  installDiagnostics(events, () => agent.cwd); // /cd moves the checker root too
  // hook chains: config first, then every enabled plugin in sorted order
  const beforeChain = [cfg.hooks?.beforeTool, ...(plugins.hooks.beforeTool ?? [])].filter(Boolean) as string[];
  const afterChain = [cfg.hooks?.afterTool, ...(plugins.hooks.afterTool ?? [])].filter(Boolean) as string[];
  if (beforeChain.length) {
    events.on('before_tool', ({ call, tool }) => {
      for (const script of beforeChain) runHook(script, { phase: 'before', name: call.name, tool: tool.name, input: call.input });
    });
  }
  if (afterChain.length) {
    events.on('after_tool', ({ call, result, ms }) => {
      for (const script of afterChain) runHook(script, { phase: 'after', name: call.name, isError: !!result.isError, ms });
    });
  }
  if (checkpointEnabled(cfg)) installCheckpoints(events, { cwd: () => agent.cwd, store });
  return agent;
}

export async function resolveSessionId(prefix: string): Promise<string> {
  const sessions = await SessionStore.list(sessionsDir());
  const hits = sessions.filter((s) => s.id.startsWith(prefix));
  if (hits.length === 1) return hits[0].id;
  if (!hits.length) throw new Error(`no session matches "${prefix}" (tango --sessions)`);
  throw new Error(`ambiguous "${prefix}": ${hits.map((s) => s.id.slice(0, 8)).join(', ')}`);
}

export interface HostArgs {
  model?: string;
  cwd?: string;
  resume?: string;
  continue?: boolean;
}

export interface Opened {
  agent: Agent;
  store: SessionStore;
  cwd: string;
  /** why the current model can't be called right now (missing key); the host decides how to show it */
  warning?: string;
  /** the layered config this session was assembled from (resolved once, for the session cwd) */
  resolved: Resolved;
}

export async function openAgent(args: HostArgs, perm: PermState): Promise<Opened> {
  let store: SessionStore;
  if (args.resume) {
    store = await SessionStore.open(sessionsDir(), await resolveSessionId(args.resume));
  } else if (args.continue) {
    const [latest] = await SessionStore.list(sessionsDir());
    if (!latest) throw new Error('no sessions to continue yet');
    store = await SessionStore.open(sessionsDir(), latest.id);
  } else {
    // record the branch so session lists can group/filter by it later
    const branch = String(
      spawnSync('git', ['rev-parse', '--abbrev-ref', 'HEAD'], { cwd: args.cwd ?? process.cwd() }).stdout ?? ''
    ).trim();
    store = await SessionStore.create(sessionsDir(), args.cwd ?? process.cwd(), undefined, undefined, undefined, branch || undefined);
  }

  const cwd = args.cwd ?? store.meta()?.cwd ?? process.cwd();
  // config depends on where the session runs: global, then that project's layers
  const resolved = await resolveConfig(cwd);
  const cfg = resolved.config;
  const models = await loadModels();
  const model = pickModel(models, cfg, args.model);
  // honour a model switch made earlier in a resumed session unless the user asked explicitly
  const stored = !args.model && !cfg.model ? store.lastModel() : undefined;
  const effective = stored ? models.find((m) => m.id === stored) ?? model : model;
  const agent = makeAgent(effective, store, cwd, perm, { ...cfg, projectDir: resolved.project?.root });
  return { agent, store, cwd, warning: keyProblem(effective), resolved };
}
