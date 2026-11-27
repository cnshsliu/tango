import { spawn, spawnSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { homedir } from 'node:os';
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
import { installLspDiagnostics } from './core/lsp-diagnostics.js';
import { EventBus } from './core/events.js';
import { mergeHookConfigs, runHooks, type HookConfig, type HookOutcome } from './core/hooks.js';
import { resolveConfig, type Resolved } from './core/project-config.js';
import { worktreeRoot } from './core/worktrees.js';
import { READ_TOOLS, installPermissionGate, parseRule, type AllowRule, type AskFn, type PermissionMode } from './core/permissions.js';
import { SessionStore, shortId } from './core/session.js';
import { ToolRegistry } from './core/tools/registry.js';
import { bashTool } from './core/tools/bash.js';
import { diffTool } from './core/tools/diff.js';
import { processListTool, processLogsTool, processStopTool } from './core/tools/process.js';
import { editTool, readTool, writeTool } from './core/tools/fs.js';
import { officeReadTool, officeWriteTool } from './core/tools/office/index.js';
import { notifyTool } from './core/tools/notify.js';
import { lspTool } from './core/tools/lsp.js';
import { scheduleCreateTool, scheduleListTool, scheduleRemoveTool } from './core/tools/schedule.js';
import { findTool, grepTool, lsTool } from './core/tools/search.js';
import { sessionSearchTool } from './core/tools/session-search.js';
import { webFetchTool } from './core/tools/web-fetch.js';
import { webSearchTool } from './core/tools/web-search.js';
import { makeTaskTool, type SubAgentDef, type TaskDeps, type TaskInput } from './core/tools/task.js';
import type { Message, ModelSpec } from './core/types.js';

export const VERSION = '0.1.0';

export function buildTools(): ToolRegistry {
  const tools = new ToolRegistry();
  for (const t of [readTool, writeTool, editTool, bashTool, grepTool, findTool, lsTool, webSearchTool, webFetchTool, officeReadTool, officeWriteTool, notifyTool, diffTool, sessionSearchTool, processLogsTool, processStopTool, processListTool, scheduleListTool, scheduleCreateTool, scheduleRemoveTool, lspTool])
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
  /** re-reads the rules and extra roots before a decision, so they follow the config files and
   *  the current directory; serve installs it */
  refreshRules?: () => unknown;
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

/**
 * Wire the lifecycle hooks onto the event bus. Besides exit 2, a hook's stdout JSON can block
 * (decision "block"), add context for the model (additionalContext) or end the turn
 * (continue: false); `ctx.addContext` / `ctx.stop` carry the last two to the agent.
 */
function installLifecycleHooks(
  events: EventBus,
  hooks: HookConfig,
  ctx: { cwd: () => string; sessionId: () => string; projectDir?: string; resumed: () => boolean; addContext: (t: string) => void; stop: (reason: string) => void; toolsOnly?: boolean }
): void {
  const base = () => ({ session_id: ctx.sessionId(), cwd: ctx.cwd() });
  const run = (ev: keyof HookConfig, payload: Record<string, unknown>, tool?: string) =>
    runHooks(hooks, ev, { ...base(), ...payload }, { cwd: ctx.cwd(), tool, projectDir: ctx.projectDir ?? ctx.cwd() });
  const later = (r: HookOutcome) => r.context?.forEach(ctx.addContext);
  if (hooks.SessionStart) {
    // once per opened session, just before its first prompt; its context rides that prompt
    let fired = false;
    events.on('before_prompt', async () => {
      if (fired) return;
      fired = true;
      const r = await run('SessionStart', { source: ctx.resumed() ? 'resume' : 'startup' });
      if (r.block) process.stderr.write(`[hook SessionStart] ${r.block}\n`.slice(0, 500));
      later(r);
    });
  }
  if (hooks.PreToolUse) {
    events.on('before_tool', async ({ call, tool }) => {
      const r = await run('PreToolUse', { tool_name: tool.name, tool_input: call.input }, tool.name);
      later(r);
      if (r.stop) ctx.stop(r.stop);
      if (r.block ?? r.stop) return { block: true, reason: `PreToolUse hook: ${r.block ?? r.stop}` };
    });
  }
  if (hooks.PostToolUse) {
    events.on('after_tool', async ({ call, result }) => {
      const text = result.content.map((b) => (b.type === 'text' ? b.text : '')).join('').slice(0, 20_000);
      const r = await run('PostToolUse', { tool_name: call.name, tool_input: call.input, tool_response: { text, is_error: !!result.isError } }, call.name);
      // exit 2 after the fact: the tool already ran, so the hook's word goes to the model
      if (r.block) result.content.push({ type: 'text', text: `\n[PostToolUse hook] ${r.block}` });
      for (const c of r.context ?? []) result.content.push({ type: 'text', text: `\n[PostToolUse hook context] ${c}` });
      if (r.stop) ctx.stop(r.stop);
    });
  }
  if (hooks.PostToolUseFailure) {
    // in addition to PostToolUse: only calls that failed (refused, crashed, or reported an error)
    events.on('after_tool', async ({ call, result }) => {
      if (!result.isError) return;
      const error = result.content.map((b) => (b.type === 'text' ? b.text : '')).join('').slice(0, 20_000);
      const r = await run('PostToolUseFailure', { tool_name: call.name, tool_input: call.input, error }, call.name);
      if (r.block) result.content.push({ type: 'text', text: `\n[PostToolUseFailure hook] ${r.block}` });
      for (const c of r.context ?? []) result.content.push({ type: 'text', text: `\n[PostToolUseFailure hook context] ${c}` });
      if (r.stop) ctx.stop(r.stop);
    });
  }
  // a `task` sub-agent: only the tool hooks; session, prompt and Stop hooks belong to the main turn
  if (ctx.toolsOnly) return;
  if (hooks.StopFailure) {
    // the turn died on an error (model/API failure, the tool-round cap): observe only
    events.on('error', async ({ error }) => {
      const r = await run('StopFailure', { error: String(error?.message ?? error).slice(0, 20_000) });
      if (r.block) process.stderr.write(`[hook StopFailure] ${r.block}\n`.slice(0, 500));
      later(r);
    });
  }
  if (hooks.SessionEnd) {
    events.on('session_end', async ({ reason }) => {
      const r = await run('SessionEnd', { reason }, reason);
      if (r.block) process.stderr.write(`[hook SessionEnd] ${r.block}\n`.slice(0, 500));
    });
  }
  if (hooks.InstructionsLoaded) {
    events.on('instructions_loaded', async ({ files }) => {
      const r = await run('InstructionsLoaded', { files_loaded: files.length, files });
      if (r.block) process.stderr.write(`[hook InstructionsLoaded] ${r.block}\n`.slice(0, 500));
      later(r);
      if (r.stop) ctx.stop(r.stop);
    });
  }
  if (hooks.CwdChanged) {
    events.on('cwd_changed', async ({ oldCwd, newCwd }) => {
      const r = await run('CwdChanged', { old_cwd: oldCwd, new_cwd: newCwd });
      if (r.block) process.stderr.write(`[hook CwdChanged] ${r.block}\n`.slice(0, 500));
      later(r);
    });
  }
  if (hooks.Notification) {
    events.on('notification', async ({ message, type }) => {
      const r = await run('Notification', { message, notification_type: type }, type);
      if (r.block) process.stderr.write(`[hook Notification] ${r.block}\n`.slice(0, 500));
    });
  }
  // /subagent and /bg: the Start/Created hooks can refuse the launch; the Stop/Completed ones observe
  const launch = (ev: 'SubagentStart' | 'TaskCreated') => async ({ agentType, description }: { agentType: string; description: string }) => {
    const r = await run(ev, { agent_type: agentType, description: description.slice(0, 20_000) }, agentType);
    later(r);
    if (r.block ?? r.stop) return { block: true as const, reason: (r.block ?? r.stop)! };
  };
  const finish = (ev: 'SubagentStop' | 'TaskCompleted') => async (p: { agentType: string; description: string; taskId: string; success: boolean; status: string }) => {
    const r = await run(ev, { agent_type: p.agentType, description: p.description.slice(0, 20_000), task_id: p.taskId, success: p.success, status: p.status }, p.agentType);
    if (r.block) process.stderr.write(`[hook ${ev}] ${r.block}\n`.slice(0, 500));
    later(r);
  };
  if (hooks.SubagentStart) events.on('subagent_start', launch('SubagentStart'));
  if (hooks.TaskCreated) events.on('task_created', launch('TaskCreated'));
  if (hooks.SubagentStop) events.on('subagent_stop', finish('SubagentStop'));
  if (hooks.TaskCompleted) events.on('task_completed', finish('TaskCompleted'));
  if (hooks.UserPromptSubmit) {
    events.on('before_prompt', async ({ text }) => {
      const r = await run('UserPromptSubmit', { prompt: text });
      if (r.block ?? r.stop) return { block: true, reason: (r.block ?? r.stop)! };
      later(r);
    });
  }
  if (hooks.Stop) {
    events.on('before_stop', async ({ message, continued }) => {
      const last = message.content.map((b) => (b.type === 'text' ? b.text : '')).join('');
      const r = await run('Stop', { stop_hook_active: continued, last_assistant_message: last.slice(0, 20_000) });
      // continue: false wins: the model is finishing anyway, so nothing sends it back
      if (r.block && !r.stop) return { block: true, reason: r.block };
      later(r);
    });
  }
  if (hooks.PreCompact) {
    events.on('before_compact', async ({ trigger }) => {
      const r = await run('PreCompact', { trigger }, trigger);
      later(r);
      if (r.stop) ctx.stop(r.stop);
      if (r.block) return { block: true, reason: r.block };
    });
  }
  if (hooks.PostCompact) {
    events.on('after_compact', async ({ trigger, summary }) => {
      const r = await run('PostCompact', { trigger, summary: summary.slice(0, 20_000) }, trigger);
      if (r.block) process.stderr.write(`[hook PostCompact] ${r.block}\n`.slice(0, 500));
      later(r);
      if (r.stop) ctx.stop(r.stop);
    });
  }
}

type AgentCfg = { checkpoint?: string; permissions?: { allow: string[] }; extraDirs?: string[]; hooks?: { beforeTool?: string; afterTool?: string } & Record<string, unknown>; maxTurns?: number; retries?: number; lspDiagnostics?: boolean; projectDir?: string; roles?: { subagent?: string }; subAgents?: SubAgentDef[] };

/** `sub`: a `task` sub-agent — given tools, tool hooks only, no checkpoints (the parent's covers it). */
export function makeAgent(model: ModelSpec, store: SessionStore, cwd: string, perm: PermState, cfg: AgentCfg = {}, sub?: { tools: ToolRegistry; readonly: boolean }): Agent {
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
  const retries = Number.isFinite(cfg.retries) ? Math.max(0, Math.min(10, Math.floor(cfg.retries as number))) : undefined;
  // decided at assembly: the first prompt's user message is already stored when SessionStart runs
  const resumed = store.messageCount() > 0;
  const agent = new Agent({ store, tools: sub?.tools ?? buildTools(), model, cwd, events, maxTurns, retries });
  if (!sub) agent.tools.register(makeTaskTool(taskDeps(agent, store, perm, cfg)));
  const plugins = loadPluginHooks();
  // lifecycle hooks before the gate: a policy hook can refuse a call before anyone is asked
  installLifecycleHooks(events, mergeHookConfigs([cfg.hooks, ...plugins.lifecycle]), {
    cwd: () => agent.cwd,
    projectDir: cfg.projectDir,
    sessionId: () => store.id,
    resumed: () => resumed,
    addContext: (t) => agent.addContext(t),
    stop: (reason) => agent.requestStop(reason),
    toolsOnly: !!sub
  });
  installPermissionGate(events, {
    mode: () => (sub?.readonly ? 'readonly' : perm.mode),
    cwd: () => agent.cwd,
    ask: async (c, r) => {
      if (!perm.ask) return false;
      // the user is needed: Notification hooks (a desktop ping, a chat message) go first
      await events.emit('notification', { message: `tango needs your permission to use ${c.name}: ${r}`.slice(0, 2000), type: 'permission_prompt' });
      return perm.ask(c, r);
    },
    rules: parsedRules,
    refresh: () => perm.refreshRules?.(),
    // managed worktrees (~/.tango/worktrees) are tango's own isolation checkouts:
    // writing there is the point of creating one, so it never asks
    extraDirs: () => [...extraDirs.dirs, worktreeRoot()],
    plan: () => plan.on
  });
  installDiagnostics(events, () => agent.cwd); // /cd moves the checker root too
  installLspDiagnostics(events, () => agent.cwd, () => cfg.lspDiagnostics !== false); // the TUI's /problems
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
  if (!sub && checkpointEnabled(cfg)) installCheckpoints(events, { cwd: () => agent.cwd, store });
  return agent;
}

/** One-line gist of a tool call for progress lines: its main argument, clipped. */
function callGist(input: unknown): string {
  const o = (input && typeof input === 'object' ? input : {}) as Record<string, unknown>;
  const v = ['command', 'path', 'pattern', 'query', 'url'].map((k) => o[k]).find((x) => typeof x === 'string');
  return String(v ?? '').replace(/\s+/g, ' ').slice(0, 120);
}

/**
 * What an interrupted sub-agent had already: its last words and its latest tool results, so the
 * parent keeps the work done so far instead of a bare "aborted". Capped; newest results first.
 */
export function partialReport(messages: Message[], cap = 8_000): string {
  const texts: string[] = [];
  const results: string[] = [];
  const names = new Map<string, string>();
  for (const m of messages) {
    for (const b of m.content) {
      if (b.type === 'tool_call') names.set(b.id, `${b.name} ${callGist(b.input)}`.trim());
      if (m.role === 'assistant' && b.type === 'text' && b.text.trim()) texts.push(b.text.trim());
      if (b.type === 'tool_result') {
        const body = b.content.map((c) => (c.type === 'text' ? c.text : '')).join('\n').trim();
        if (body && body !== 'aborted') results.push(`[${names.get(b.toolCallId) ?? 'tool'}]\n${body}`);
      }
    }
  }
  const parts: string[] = [];
  if (texts.length) parts.push(`last note: ${texts[texts.length - 1]}`);
  let room = cap - parts.join('\n\n').length;
  for (const r of results.reverse()) {
    if (room <= 200) break;
    const piece = r.length > room ? `${r.slice(0, room - 20)}\n…(truncated)` : r;
    parts.push(piece);
    room -= piece.length + 2;
  }
  return parts.join('\n\n');
}

/** at most this many background sub-agents per session at once */
export const MAX_BG_TASKS = 4;

function subAgentPrompt(def: SubAgentDef): string {
  return (
    `\n\n# Sub-agent\nYou are the \`${def.name}\` sub-agent. Another agent delegated ONE task to you; you do not ` +
    'see its conversation, only the task below. You cannot delegate further (there is no task tool). ' +
    'Work on your own: never ask the user anything (no ?choice, no ?suggest). ' +
    'End with a self-contained report for that agent: what you found or did, exact paths and ' +
    'line numbers, commands and their results, and anything you could not verify. ' +
    (def.readonly
      ? 'You are read-only: you can read, search and browse, but not edit files or run commands (no bash). ' +
        'If the task asks for a command, do the closest thing with your tools and say so in the report. '
      : '') +
    (def.instruction ? `\n\n${def.instruction}` : '')
  );
}

/** Wires the parent's `task` tool: sub-agents share its permission state, cwd and tool hooks. */
function taskDeps(parent: Agent, store: SessionStore, perm: PermState, cfg: AgentCfg): TaskDeps {
  let seq = 0;
  let running = 0;
  const run = async (def: SubAgentDef, input: TaskInput, signal: AbortSignal, id: string, background: boolean, callId?: string) => {
    // the sub-agent's own model, else roles.subagent (read now: /agents set applies at once), else the parent's
    const models = await loadModels();
    const roles = (await resolveConfig(parent.cwd).catch(() => undefined))?.config.roles ?? cfg.roles;
    const wanted = def.model ?? roles?.subagent;
    const model = (wanted && models.find((m) => m.id === wanted)) || parent.model;
    const label = `task:${def.name} ${String(input.description ?? '')}`.slice(0, 80);
    const subStore = await SessionStore.create(sessionsDir(), parent.cwd, label, undefined, undefined, store.meta()?.branch, store.id);
    // the parent's live registry (MCP bridges included), never `task` itself: no recursion
    const tools = new ToolRegistry();
    for (const name of parent.tools.names()) {
      if (name === 'task' || (def.readonly && !READ_TOOLS.has(name))) continue;
      tools.register(parent.tools.get(name)!);
    }
    const sub = makeAgent(model, subStore, parent.cwd, perm, cfg, { tools, readonly: !!def.readonly });
    sub.systemSuffix = subAgentPrompt(def) + (parent.systemSuffix ?? '');
    // each tool the sub-agent runs shows up under the parent's task line (foreground only:
    // a background run's line has already finished)
    let step = 0;
    if (!background) {
      sub.events.on('before_tool', ({ call }) => {
        void parent.events.emit('task_progress', { callId, agent: def.name, step: ++step, tool: call.name, detail: callGist(call.input) });
      });
    }
    const stop = () => sub.abort();
    signal.addEventListener('abort', stop);
    let text = '';
    let ok = false;
    try {
      const msg = await sub.prompt(String(input.prompt));
      text = msg.content.map((b) => (b.type === 'text' ? b.text : '')).join('').trim();
      ok = msg.stopReason !== 'aborted' && msg.stopReason !== 'error';
      if (!ok && !text) {
        // interrupted: hand back what it had found so far
        const partial = partialReport(subStore.allMessages());
        text = partial ? `interrupted before its report; work so far:\n\n${partial}` : 'aborted';
      }
    } catch (e: any) {
      text = `sub-agent failed: ${e?.message ?? e}`;
    } finally {
      signal.removeEventListener('abort', stop);
    }
    parent.totalCostUsd += sub.totalCostUsd;
    const done = { id, agent: def.name, description: String(input.description ?? ''), ok, sessionId: subStore.id, costUsd: sub.totalCostUsd, background };
    return { text, ok, sessionId: subStore.id, done };
  };
  return {
    subAgents: () => (Array.isArray(cfg.subAgents) ? cfg.subAgents : []),
    run: async (def, input, ctx) => {
      const r = await run(def, input, ctx.signal, `t${++seq}`, false, ctx.callId);
      await parent.events.emit('task_done', r.done);
      return r;
    },
    start: (def, input) => {
      if (running >= MAX_BG_TASKS) return { error: `${MAX_BG_TASKS} background tasks already running; wait for one to report` };
      const id = `t${++seq}`;
      running++;
      // its own signal: interrupting the parent turn does not cancel background work
      void run(def, input, new AbortController().signal, id, true)
        .then(async (r) => {
          await parent.deliver(
            `（后台任务 ${id} · ${def.name}：${input.description} ${r.ok ? '完成' : '失败'} · session ${shortId(r.sessionId)}）\n${r.text}`
          );
          await parent.events.emit('task_done', r.done);
        })
        .catch(() => {})
        .finally(() => running--);
      return { id };
    }
  };
}

export async function resolveSessionId(prefix: string): Promise<string> {
  const sessions = await SessionStore.list(sessionsDir());
  const hits = sessions.filter(
    // an id prefix, or the id's last '-' part (a uuid's final 12 hex digits, as the TUI shows it)
    (s) => s.id.startsWith(prefix) || (!!prefix && !prefix.includes('-') && s.id.endsWith(`-${prefix}`))
  );
  if (hits.length === 1) return hits[0].id;
  if (!hits.length) throw new Error(`no session matches "${prefix}" (tango --sessions)`);
  throw new Error(`ambiguous "${prefix}": ${hits.map((s) => shortId(s.id)).join(', ')}`);
}

export interface HostArgs {
  model?: string;
  cwd?: string;
  resume?: string;
  continue?: boolean;
  /** --max-tool-rounds: beats config maxTurns for this process */
  maxToolRounds?: number;
}

export interface Opened {
  agent: Agent;
  store: SessionStore;
  cwd: string;
  /** the session's recorded directory, when it no longer exists and the session was moved to
   *  `cwd` instead (a removed worktree); the host tells the user */
  movedFrom?: string;
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
    // --continue means the user's last conversation, never a task sub-agent's
    const [latest] = (await SessionStore.list(sessionsDir())).filter((x) => !x.parent);
    if (!latest) throw new Error('no sessions to continue yet');
    store = await SessionStore.open(sessionsDir(), latest.id);
  } else {
    // record the branch so session lists can group/filter by it later
    const branch = String(
      spawnSync('git', ['rev-parse', '--abbrev-ref', 'HEAD'], { cwd: args.cwd ?? process.cwd() }).stdout ?? ''
    ).trim();
    store = await SessionStore.create(sessionsDir(), args.cwd ?? process.cwd(), undefined, undefined, undefined, branch || undefined);
  }

  // A resumed session keeps its recorded directory — unless that is gone (the worktree was
  // removed): then it runs where tango was started (after the launch rescue: the home directory
  // at worst) instead of sitting in a directory where nothing can run.
  let movedFrom: string | undefined;
  let cwd = args.cwd ?? store.meta()?.cwd ?? process.cwd();
  if (!args.cwd && !existsSync(cwd)) {
    movedFrom = cwd;
    cwd = existsSync(process.cwd()) ? process.cwd() : homedir();
  }
  // config depends on where the session runs: global, then that project's layers
  const resolved = await resolveConfig(cwd);
  const cfg = resolved.config;
  const models = await loadModels();
  const model = pickModel(models, cfg, args.model);
  // honour a model switch made earlier in a resumed session unless the user asked explicitly
  const stored = !args.model && !cfg.model ? store.lastModel() : undefined;
  const effective = stored ? models.find((m) => m.id === stored) ?? model : model;
  const maxTurns = args.maxToolRounds ?? cfg.maxTurns;
  const agent = makeAgent(effective, store, cwd, perm, { ...cfg, maxTurns, projectDir: resolved.project?.root });
  return { agent, store, cwd, movedFrom, warning: keyProblem(effective), resolved };
}
