/**
 * Shared host wiring: every frontend (terminal REPL, serve protocol, task daemon)
 * assembles its agent here so tools, permissions and diagnostics stay identical.
 * Nothing in this file prints to stdout except where a caller passes a printer.
 */
import { Agent } from './core/agent.js';
import { keyProblem, loadConfig, loadModels, pickModel, sessionsDir } from './core/config.js';
import { installDiagnostics } from './core/diagnostics.js';
import { EventBus } from './core/events.js';
import { installPermissionGate, type AskFn, type PermissionMode } from './core/permissions.js';
import { SessionStore } from './core/session.js';
import { ToolRegistry } from './core/tools/registry.js';
import { bashTool } from './core/tools/bash.js';
import { diffTool } from './core/tools/diff.js';
import { editTool, readTool, writeTool } from './core/tools/fs.js';
import { officeReadTool, officeWriteTool } from './core/tools/office/index.js';
import { notifyTool } from './core/tools/notify.js';
import { findTool, grepTool, lsTool } from './core/tools/search.js';
import { webFetchTool } from './core/tools/web-fetch.js';
import { webSearchTool } from './core/tools/web-search.js';
import type { ModelSpec } from './core/types.js';

export const VERSION = '0.0.1';

/** Live permission state a host mutates (REPL /mode, serve set_mode, tasks --mode). */
export interface PermState {
  mode: PermissionMode;
  ask?: AskFn;
}

export function buildTools(): ToolRegistry {
  const tools = new ToolRegistry();
  for (const t of [readTool, writeTool, editTool, bashTool, grepTool, findTool, lsTool, webSearchTool, webFetchTool, officeReadTool, officeWriteTool, notifyTool, diffTool])
    tools.register(t);
  return tools;
}

export function makeAgent(model: ModelSpec, store: SessionStore, cwd: string, perm: PermState): Agent {
  const events = new EventBus();
  // gate first: a vetoed call is never announced as running
  installPermissionGate(events, { mode: () => perm.mode, cwd, ask: (c, r) => (perm.ask ? perm.ask(c, r) : Promise.resolve(false)) });
  installDiagnostics(events, cwd);
  return new Agent({ store, tools: buildTools(), model, cwd, events });
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
}

export async function openAgent(args: HostArgs, perm: PermState): Promise<Opened> {
  const cfg = await loadConfig();
  const models = await loadModels();
  const model = pickModel(models, cfg, args.model);

  let store: SessionStore;
  if (args.resume) {
    store = await SessionStore.open(sessionsDir(), await resolveSessionId(args.resume));
  } else if (args.continue) {
    const [latest] = await SessionStore.list(sessionsDir());
    if (!latest) throw new Error('no sessions to continue yet');
    store = await SessionStore.open(sessionsDir(), latest.id);
  } else {
    store = await SessionStore.create(sessionsDir(), args.cwd ?? process.cwd());
  }

  const cwd = args.cwd ?? store.meta()?.cwd ?? process.cwd();
  // honour a model switch made earlier in a resumed session unless the user asked explicitly
  const stored = !args.model && !cfg.model ? store.lastModel() : undefined;
  const effective = stored ? models.find((m) => m.id === stored) ?? model : model;
  const agent = makeAgent(effective, store, cwd, perm);
  return { agent, store, cwd, warning: keyProblem(effective) };
}
