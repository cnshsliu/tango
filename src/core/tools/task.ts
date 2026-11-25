/**
 * `task`: the model delegates a focused job to a sub-agent (grok-cli parity).
 * Built-ins: `explore` (read-only tools, fast investigation) and `general` (every tool
 * but `task`, same permission gate as the parent). Config `subAgents` adds named ones.
 * Foreground calls return the sub-agent's final report as the tool result; read-only
 * agents may run in the background and report back into the session when done.
 * The host supplies `run` (it owns models, sessions and permissions).
 */
import type { Tool, ToolContext } from '../types.js';
import { error, text, truncate } from './registry.js';
import { shortId } from '../session.js';

export interface SubAgentDef {
  name: string;
  /** what it is for: shown to the model in the tool description */
  description?: string;
  /** extra system-prompt instruction for this sub-agent */
  instruction?: string;
  /** model id; falls back to roles.subagent, then the parent's model */
  model?: string;
  /** read-only tools only (like explore); required for background runs */
  readonly?: boolean;
}

export const BUILTIN_SUBAGENTS: SubAgentDef[] = [
  { name: 'explore', readonly: true, description: 'fast read-only investigation: find code, explain how something works, review a change, research the web' },
  { name: 'general', readonly: false, description: 'multi-step work that may edit files and run commands in the current directory' }
];

export interface TaskInput {
  agent?: string;
  description: string;
  prompt: string;
  background?: boolean;
}

export interface TaskRunResult {
  text: string;
  sessionId: string;
  ok: boolean;
}

export interface TaskDeps {
  /** custom sub-agents from config (config `subAgents`, as the session opened) */
  subAgents: () => SubAgentDef[];
  /** run one sub-agent to completion */
  run: (def: SubAgentDef, input: TaskInput, ctx: ToolContext) => Promise<TaskRunResult>;
  /** start a background run; returns its id, or an error string */
  start: (def: SubAgentDef, input: TaskInput, ctx: ToolContext) => { id: string } | { error: string };
}

/** Built-ins first; a config entry with a built-in's name replaces it. */
export function allSubAgents(custom: SubAgentDef[]): SubAgentDef[] {
  const byName = new Map(BUILTIN_SUBAGENTS.map((d) => [d.name, d]));
  for (const d of custom) if (d && typeof d.name === 'string' && /^[\w.-]{1,40}$/.test(d.name)) byName.set(d.name, { ...d });
  return [...byName.values()];
}

const MAX_RESULT = 20_000;

export function makeTaskTool(deps: TaskDeps): Tool<TaskInput> {
  const find = (name: string) => allSubAgents(deps.subAgents()).find((d) => d.name === name);
  return {
    name: 'task',
    // a getter: the list is built when the model request is assembled
    get description() {
      const list = allSubAgents(deps.subAgents())
        .map((d) => `- ${d.name}${d.readonly ? ' (read-only)' : ''}: ${d.description ?? d.instruction?.slice(0, 120) ?? ''}`)
        .join('\n');
      return (
        'Delegate a focused task to a sub-agent with a fresh context. It sees none of this conversation, ' +
        'so the prompt must carry every detail it needs (paths, goal, what to report). Only its final ' +
        'report comes back, which keeps this context small. Use it proactively for investigation, ' +
        'review and research; do small, tightly scoped lookups yourself. ' +
        'background: true (read-only agents only) returns at once; the report arrives later as a message. ' +
        'Read-only agents have no bash: ask them for read/grep/find/ls/web work only, and name the ' +
        'directories to search (a scan of the whole home directory is slow).\n' +
        'Agents:\n' + list
      );
    },
    input: {
      type: 'object',
      properties: {
        agent: { type: 'string', description: 'Sub-agent name (default explore)' },
        description: { type: 'string', description: 'Short label, e.g. "find the auth middleware"' },
        prompt: { type: 'string', description: 'Complete, self-contained instructions and what to report back' },
        background: { type: 'boolean', description: 'Run without waiting (read-only agents only)' }
      },
      required: ['description', 'prompt']
    },
    // explore reads; anything that can write counts as a write (plan mode, checkpoints)
    writeEffect: (input) => !find(String(input?.agent ?? 'explore'))?.readonly,
    async execute(input, ctx) {
      const name = String(input.agent ?? 'explore');
      const def = find(name);
      if (!def) {
        const names = allSubAgents(deps.subAgents()).map((d) => d.name).join(', ');
        return error(`unknown sub-agent "${name}"; available: ${names}`);
      }
      if (!String(input.prompt ?? '').trim()) return error('task: empty prompt');
      if (input.background) {
        if (!def.readonly) return error(`background tasks must be read-only; "${name}" can write. Run it in the foreground.`);
        const r = deps.start(def, input, ctx);
        if ('error' in r) return error(r.error);
        return text(`started background task ${r.id} (${name}: ${input.description}). Its report arrives as a message when done; keep working meanwhile.`);
      }
      const r = await deps.run(def, input, ctx);
      const body = r.text.trim() || '(the sub-agent ended without a report)';
      const out = `[${name} · session ${shortId(r.sessionId)}]\n${truncate(body, MAX_RESULT)}`;
      return r.ok ? text(out) : error(out);
    }
  };
}
