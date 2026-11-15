/**
 * tango serve — the boundary between core and any UI (the Rust TUI, later web/IDE).
 * stdio is the transport: no flags, no port, no token.
 *
 * stdin: one JSON command per line.
 * stdout: one JSON frame per line, protocol only — human logs never touch stdout.
 *
 * commands:  prompt | interrupt | permission | models | set_model | set_mode | compact | sessions | history | save_skill | status
 * frames:    hello | stream | message | tool_result | permission_request | turn_end | error | result
 */
import { createInterface } from 'node:readline';
import { hasKey, keyProblem, loadConfig, loadModels, pickModel, saveConfig, sessionsDir } from './core/config.js';
import { listDrafts, promoteDraft, saveDraft } from './core/skills.js';
import { join } from 'node:path';
import { SessionStore } from './core/session.js';
import type { AskFn, PermissionMode } from './core/permissions.js';
import type { ToolResult } from './core/types.js';
import { buildTools, openAgent, VERSION, type HostArgs, type PermState } from './host.js';

export const PROTOCOL = 1;

interface Command {
  id?: number;
  cmd: string;
  [k: string]: unknown;
}

const resultText = (r: ToolResult) =>
  r.content
    .map((c) => (c.type === 'text' ? c.text : `[image ${c.mimeType}]`))
    .join('\n');

export async function serveStdio(args: HostArgs & { readonly?: boolean }): Promise<void> {
  const out = (frame: Record<string, unknown>): void => {
    process.stdout.write(JSON.stringify(frame) + '\n');
  };
  const perm: PermState = { mode: args.readonly ? 'readonly' : 'ask' };
  // session resume without flags: the TUI respawns this child with TANGO_RESUME=<id or prefix>
  if (!args.resume && process.env.TANGO_RESUME) args.resume = process.env.TANGO_RESUME;
  // launch options ride the environment too (tango tui -m / -y / --readonly)
  if (!args.model && process.env.TANGO_MODEL) args.model = process.env.TANGO_MODEL;
  const envMode = process.env.TANGO_MODE;
  if (!args.readonly && (envMode === 'ask' || envMode === 'auto' || envMode === 'readonly')) perm.mode = envMode;

  let opened;
  try {
    opened = await openAgent(args, perm);
  } catch (e: any) {
    out({ type: 'error', fatal: true, message: String(e?.message ?? e) });
    process.exitCode = 1;
    return;
  }
  const { agent, store, cwd } = opened;

  // permission bridge: gate asks → client answers by request id
  let reqSeq = 0;
  const pending = new Map<number, (allow: boolean) => void>();
  perm.ask = ((call, reason) =>
    new Promise<boolean>((resolve) => {
      const request = ++reqSeq;
      pending.set(request, resolve);
      out({ type: 'permission_request', request, call: { id: call.id, name: call.name, input: call.input }, reason });
    })) satisfies AskFn;

  agent.events.on('message', ({ message }) => out({ type: 'message', message }));
  agent.events.on('turn_end', (e) => out({ type: 'turn_end', ...e, totalCostUsd: agent.totalCostUsd }));
  agent.events.on('after_tool', ({ call, result, ms }) =>
    out({
      type: 'tool_result',
      call: { id: call.id, name: call.name, input: call.input },
      isError: !!result.isError,
      ms,
      // the UI shows a peek; the model already saw the full text in its context
      output: resultText(result).slice(0, 4_000)
    })
  );
  agent.events.on('error', ({ error }) => out({ type: 'error', message: error.message }));

  const denyPending = () => {
    for (const resolve of pending.values()) resolve(false);
    pending.clear();
  };

  out({
    type: 'hello',
    protocol: PROTOCOL,
    version: VERSION,
    sessionId: store.id,
    model: agent.model.id,
    cwd,
    mode: perm.mode,
    tools: buildTools().schemas().map(({ name, description }) => ({ name, description }))
  });

  const reply = (cmd: Command, ok: boolean, data?: unknown, error?: string) => {
    if (cmd.id === undefined) return;
    out(ok ? { type: 'result', id: cmd.id, ok: true, data } : { type: 'result', id: cmd.id, ok: false, error });
  };

  async function handle(cmd: Command): Promise<void> {
    switch (cmd.cmd) {
      case 'prompt': {
        const text = String(cmd.text ?? '');
        if (!text.trim()) return reply(cmd, false, undefined, 'prompt: empty text');
        try {
          const msg = await agent.prompt(text, (ev) => {
            if (ev.type !== 'done') out({ type: 'stream', event: ev });
          });
          reply(cmd, true, { stopReason: msg.stopReason ?? 'stop' });
        } catch (e: any) {
          reply(cmd, false, undefined, String(e?.message ?? e));
        }
        return;
      }
      case 'sessions': {
        const list = await SessionStore.list(sessionsDir());
        reply(cmd, true, {
          sessions: list.slice(0, 40).map((s) => ({
            id: s.id,
            createdAt: s.createdAt.slice(0, 16),
            messages: s.messages,
            cwd: s.cwd,
            name: s.name ?? '',
            current: s.id === store.id
          }))
        });
        return;
      }
      case 'history': {
        // the full conversation (compaction ignored) flattened for display: a resumed client
        // redraws its transcript from this; tool calls are paired with their results
        const results = new Map<string, { isError: boolean; output: string }>();
        const all = store.allMessages();
        for (const m of all)
          for (const b of m.content)
            if (b.type === 'tool_result')
              results.set(b.toolCallId, {
                isError: !!b.isError,
                output: resultText({ content: b.content, isError: b.isError }).slice(0, 4_000)
              });
        const items: unknown[] = [];
        for (const m of all)
          for (const b of m.content) {
            if (b.type === 'text' && b.text.trim()) items.push({ kind: m.role, text: b.text });
            else if (b.type === 'tool_call')
              items.push({ kind: 'tool', name: b.name, input: b.input, ...(results.get(b.id) ?? { isError: false, output: '' }) });
          }
        const limit = Math.max(1, Number(cmd.limit) || 300);
        reply(cmd, true, { items: items.slice(-limit), total: items.length });
        return;
      }
      case 'models': {
        const models = await loadModels();
        reply(cmd, true, {
          current: agent.model.id,
          models: models.map((m) => ({ id: m.id, keyed: hasKey(m), contextWindow: m.contextWindow ?? 0 }))
        });
        return;
      }
      case 'set_model': {
        try {
          const model = pickModel(await loadModels(), {}, String(cmd.modelId ?? ''));
          agent.setModel(model);
          // same as the REPL: an explicit switch becomes the default for new sessions
          await saveConfig({ ...(await loadConfig()), model: model.id });
          reply(cmd, true, { model: model.id, warning: keyProblem(model) });
        } catch (e: any) {
          reply(cmd, false, undefined, e.message);
        }
        return;
      }
      case 'set_mode': {
        const mode = String(cmd.mode ?? '') as PermissionMode;
        if (mode !== 'ask' && mode !== 'auto' && mode !== 'readonly')
          return reply(cmd, false, undefined, `bad mode "${cmd.mode}" (ask|auto|readonly)`);
        perm.mode = mode;
        reply(cmd, true, { mode });
        return;
      }
      case 'compact': {
        const compacted = await agent.compact();
        reply(cmd, true, { compacted, contextMessages: store.messageCount() });
        return;
      }
      case 'save_skill': {
        // arg "" → draft from this session; "finish <file>" → promote a draft into a project skill
        const arg = String(cmd.arg ?? '').trim();
        try {
          if (/^(finish|定稿)(\s|$)/.test(arg)) {
            const file = arg.replace(/^(finish|定稿)\s*/, '');
            if (!file) return reply(cmd, true, { drafts: await listDrafts() });
            const root = join(cwd, '.tango', 'skills');
            const s = await promoteDraft(file, root);
            return reply(cmd, true, { skill: s.file });
          }
          const firstUser = store.allMessages().find((m) => m.role === 'user');
          const firstText = firstUser?.content.find((b) => b.type === 'text') as { text: string } | undefined;
          const d = await saveDraft({
            title: arg || firstText?.text.slice(0, 60) || 'workflow',
            description: arg || undefined,
            messages: store.allMessages(),
            cwd
          });
          reply(cmd, true, { draft: d.file });
        } catch (e: any) {
          reply(cmd, false, undefined, String(e?.message ?? e));
        }
        return;
      }
      case 'status':
        reply(cmd, true, {
          sessionId: store.id,
          model: agent.model.id,
          mode: perm.mode,
          cwd,
          contextMessages: store.messageCount(),
          totalCostUsd: agent.totalCostUsd
        });
        return;
      default:
        reply(cmd, false, undefined, `unknown command "${cmd.cmd}"`);
    }
  }

  // prompts run one at a time; interrupt and permission answers jump the queue
  let queue: Promise<void> = Promise.resolve();
  const rl = createInterface({ input: process.stdin });
  rl.on('line', (line) => {
    line = line.trim();
    if (!line) return;
    let cmd: Command;
    try {
      cmd = JSON.parse(line);
    } catch {
      return out({ type: 'error', message: `not JSON: ${line.slice(0, 120)}` });
    }
    if (cmd.cmd === 'interrupt') {
      // an interrupted turn must not leave a permission dialog hanging open
      denyPending();
      agent.abort();
      return;
    }
    if (cmd.cmd === 'permission') {
      const resolve = pending.get(Number(cmd.request));
      if (resolve) {
        pending.delete(Number(cmd.request));
        resolve(!!cmd.allow);
      }
      return;
    }
    // mode flips and status reads are instant state ops: they must land mid-turn
    // (e.g. "always allow" from a permission dialog), so they skip the prompt queue too
    if (cmd.cmd === 'set_mode' || cmd.cmd === 'status') {
      void handle(cmd);
      return;
    }
    queue = queue.then(() => handle(cmd));
  });
  rl.on('close', () => {
    // stdin EOF (a pipe, not a UI): finish queued work, then leave; never cut a running prompt short
    void queue.then(() => process.exit(0));
  });
}
