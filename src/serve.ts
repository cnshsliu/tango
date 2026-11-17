/**
 * tango serve — the boundary between core and any UI (the Rust TUI, later web/IDE).
 * stdio is the transport: no flags, no port, no token.
 *
 * stdin: one JSON command per line.
 * stdout: one JSON frame per line, protocol only — human logs never touch stdout.
 *
 * commands:  prompt | btw | interrupt | permission | models | set_model | set_mode | set_nvim_viewer
 *            set_theme | set_mouse | diff | compact | sessions | history | save_skill | status
 * frames:    hello | stream | btw_stream | message | tool_result | permission_request | turn_end | error | result
 */
import { createInterface } from 'node:readline';
import { restoreSnapshot } from './core/git-snapshot.js';
import { hasKey, keyProblem, loadConfig, loadModels, pickModel, saveConfig, sessionsDir } from './core/config.js';
import { listDrafts, promoteDraft, saveDraft } from './core/skills.js';
import { join } from 'node:path';
import { SessionStore } from './core/session.js';
import { costUsd } from './core/models/registry.js';
import type { AskFn, PermissionMode } from './core/permissions.js';
import type { ToolResult } from './core/types.js';
import { buildTools, openAgent, VERSION, type HostArgs, type PermState } from './host.js';
import { diffTool } from './core/tools/diff.js';

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
  const config = await loadConfig();
  const pricedModels = await loadModels();
  let sessionCostUsd = store.allMessages().reduce((sum, m) => {
    if (m.role !== 'assistant' || !m.usage) return sum;
    const model = pricedModels.find((p) => p.id === m.model) ?? agent.model;
    return sum + costUsd(model, m.usage);
  }, 0);

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
  agent.events.on('turn_end', (e) => {
    sessionCostUsd += e.costUsd;
    out({
      type: 'turn_end',
      ...e,
      totalCostUsd: sessionCostUsd,
      contextTokens: e.usage.input,
      contextWindow: agent.model.contextWindow ?? 128_000
    });
  });
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
    totalCostUsd: sessionCostUsd,
    contextTokens: store.allMessages().filter((m) => m.role === 'assistant').at(-1)?.usage?.input ?? 0,
    contextWindow: agent.model.contextWindow ?? 128_000,
    theme: config.theme,
    mouse: config.mouse ?? true,
    editor: config.editor,
    keys: config.keys,
    nvimViewer: config.nvimViewer,
    tools: buildTools().schemas().map(({ name, description }) => ({ name, description }))
  });

  const reply = (cmd: Command, ok: boolean, data?: unknown, error?: string) => {
    if (cmd.id === undefined) return;
    out(ok ? { type: 'result', id: cmd.id, ok: true, data } : { type: 'result', id: cmd.id, ok: false, error });
  };

  // ---------- time travel: turns, rewind, undo, redo ----------

  /** one prompt turn: its user-message entry, its last entry, and its pre-turn snapshot */
  function turns(): { entryIndex: number; end: number; ts: string; firstLine: string; hash?: string }[] {
    const indexed = store.indexedEntries();
    const list: { entryIndex: number; end: number; ts: string; firstLine: string; hash?: string }[] = [];
    for (const { entry, index } of indexed) {
      if (entry.kind !== 'message' || entry.message.role !== 'user') continue;
      const first = entry.message.content[0];
      if (!first || first.type !== 'text') continue; // tool-result messages are not turns
      list.push({
        entryIndex: index,
        end: index,
        ts: entry.ts,
        firstLine: first.text.split('\n')[0].slice(0, 80)
      });
    }
    // a checkpoint entry belongs to the turn whose user message precedes it: the
    // snapshot taken BEFORE that turn's first write (state after the previous turn)
    for (const { entry, index } of indexed) {
      if (entry.kind !== 'checkpoint') continue;
      for (let i = list.length - 1; i >= 0; i--) {
        if (list[i].entryIndex < index) {
          if (!list[i].hash) list[i].hash = entry.hash;
          break;
        }
      }
    }
    for (let i = 0; i < list.length; i++)
      list[i].end = (i + 1 < list.length ? list[i + 1].entryIndex : indexed.length) - 1;
    return list;
  }

  let promptInFlight = false;

  /**
   * Drop turns[k..]: restore code to the workspace state at the boundary just
   * before turns[k] ran, then truncate the projection to match.
   *
   * boundary(k) = the last checkpoint logged before turns[k]'s user message
   * (= post-snapshot of turn k-1), falling back to the first checkpoint after
   * it (= turn k's own pre-snapshot) when k is the first writing turn.
   */
  async function rewindToTurn(k: number, cmd: Command): Promise<void> {
    const list = turns();
    if (k < 0 || k >= list.length) return reply(cmd, false, undefined, 'nothing to rewind/redo');
    const target = list[k];
    let code: { restored?: number; deleted?: string[]; kept?: string[] } = {};
    const cps = store
      .indexedEntries()
      .filter(({ entry }) => entry.kind === 'checkpoint')
      .map(({ entry, index }) => ({ hash: (entry as { hash: string }).hash, index }));
    let boundaryIdx = -1;
    for (const cp of cps) if (cp.index < target.entryIndex) boundaryIdx = Math.max(boundaryIdx, cps.indexOf(cp));
    if (boundaryIdx < 0) boundaryIdx = cps.findIndex((cp) => cp.index > target.entryIndex);
    if (boundaryIdx >= 0) {
      const later = cps.slice(boundaryIdx + 1).map((c) => c.hash);
      code = await restoreSnapshot(cwd, cps[boundaryIdx].hash, later);
    }
    await store.rewindTo(target.entryIndex - 1);
    reply(cmd, true, { codeRestored: boundaryIdx >= 0, turnDropped: k, ...code });
  }

  /** index of the last turn visible in the current projection */
  function lastVisibleTurn(list: ReturnType<typeof turns>): number {
    const visibleEnd = store.visibleEntries().length;
    let last = -1;
    for (let i = 0; i < list.length; i++) {
      if (list[i].entryIndex < visibleEnd) last = i;
      else break;
    }
    return last;
  }

  function timeTravel(cmd: Command): Promise<void> | void {
    if (promptInFlight) return reply(cmd, false, undefined, 'a turn is running — interrupt it first');
    const list = turns();
    if (cmd.cmd === 'timeline') {
      reply(cmd, true, {
        nodes: list.map((t) => ({
          entryIndex: t.entryIndex, ts: t.ts, firstLine: t.firstLine, hasSnapshot: !!t.hash
        }))
      });
      return;
    }
    const last = lastVisibleTurn(list);
    if (cmd.cmd === 'rewind') {
      const entryIndex = Number(cmd.entryIndex);
      const k = list.findIndex((t) => t.entryIndex === entryIndex);
      if (k < 0) return reply(cmd, false, undefined, `rewind: no turn at entry ${entryIndex}`);
      return rewindToTurn(k, cmd);
    }
    if (cmd.cmd === 'undo') {
      if (last < 0) return reply(cmd, false, undefined, 'nothing to undo');
      return rewindToTurn(last, cmd);
    }
    if (cmd.cmd === 'redo') {
      if (last >= list.length - 1) return reply(cmd, false, undefined, 'nothing to redo');
      return rewindToTurn(last + 2, cmd);
    }
  }

  async function handle(cmd: Command): Promise<void> {
    switch (cmd.cmd) {
      case 'prompt': {
        const text = String(cmd.text ?? '');
        if (!text.trim()) return reply(cmd, false, undefined, 'prompt: empty text');
        promptInFlight = true;
        try {
          const msg = await agent.prompt(text, (ev) => {
            if (ev.type !== 'done') out({ type: 'stream', event: ev });
          });
          reply(cmd, true, { stopReason: msg.stopReason ?? 'stop' });
        } catch (e: any) {
          reply(cmd, false, undefined, String(e?.message ?? e));
        } finally {
          promptInFlight = false;
        }
        return;
      }
      case 'btw': {
        const text = String(cmd.text ?? '');
        if (!text.trim()) return reply(cmd, false, undefined, 'btw: empty text');
        try {
          const msg = await agent.ask(text, (ev) => {
            if (ev.type !== 'done') out({ type: 'btw_stream', id: cmd.id, event: ev });
          });
          reply(cmd, true, {
            text: msg.content
              .filter((b) => b.type === 'text')
              .map((b) => b.text)
              .join(''),
            stopReason: msg.stopReason ?? 'stop'
          });
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
        // the visible conversation (compaction ignored, rewind respected) flattened for
        // display: a resumed client redraws its transcript from this; tool calls are
        // paired with their results
        const results = new Map<string, { isError: boolean; output: string }>();
        const all = store.allVisibleMessages();
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
          reply(cmd, true, {
            model: model.id,
            warning: keyProblem(model),
            contextWindow: model.contextWindow ?? 128_000
          });
        } catch (e: any) {
          reply(cmd, false, undefined, e.message);
        }
        return;
      }
      case 'set_theme': {
        const theme = String(cmd.theme ?? '');
        if (theme !== 'dark' && theme !== 'light' && theme !== 'auto')
          return reply(cmd, false, undefined, `bad theme "${cmd.theme}" (dark|light|auto)`);
        await saveConfig({ ...(await loadConfig()), theme });
        reply(cmd, true, { theme });
        return;
      }
      case 'set_mouse': {
        if (typeof cmd.mouse !== 'boolean')
          return reply(cmd, false, undefined, `bad mouse value "${cmd.mouse}" (true|false)`);
        await saveConfig({ ...(await loadConfig()), mouse: cmd.mouse });
        reply(cmd, true, { mouse: cmd.mouse });
        return;
      }
      case 'set_nvim_viewer': {
        const mode = String(cmd.mode ?? '');
        if (mode !== 'ask' && mode !== 'readonly')
          return reply(cmd, false, undefined, `bad nvim viewer mode "${cmd.mode}" (ask|readonly)`);
        await saveConfig({ ...(await loadConfig()), nvimViewer: mode });
        reply(cmd, true, { nvimViewer: mode });
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
        reply(cmd, true, {
          compacted,
          contextMessages: store.messageCount(),
          contextTokens: agent.contextTokenEstimate(),
          contextWindow: agent.model.contextWindow ?? 128_000
        });
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
      case 'diff': {
        const paths = Array.isArray(cmd.paths) ? cmd.paths.map(String) : ['.'];
        const staged = cmd.staged === true;
        const result = await diffTool.execute(
          { paths, staged },
          { cwd, signal: new AbortController().signal }
        );
        const output = resultText(result);
        reply(cmd, true, {
          diff: output === '(no diff for those paths)' ? 'clean — nothing changed' : output,
          isError: !!result.isError,
          staged
        });
        return;
      }
      case 'timeline':
      case 'rewind':
      case 'undo':
      case 'redo':
        return timeTravel(cmd);
      case 'status':
        reply(cmd, true, {
          sessionId: store.id,
          model: agent.model.id,
          mode: perm.mode,
          cwd,
          contextMessages: store.messageCount(),
          totalCostUsd: sessionCostUsd,
          contextTokens: agent.contextTokenEstimate(),
          contextWindow: agent.model.contextWindow ?? 128_000
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
    if (cmd.cmd === 'set_mode' || cmd.cmd === 'status' || cmd.cmd === 'timeline') {
      void handle(cmd);
      return;
    }
    if (cmd.cmd === 'rewind' || cmd.cmd === 'undo' || cmd.cmd === 'redo') {
      void timeTravel(cmd);
      return;
    }
    queue = queue.then(() => handle(cmd));
  });
  rl.on('close', () => {
    // stdin EOF (a pipe, not a UI): finish queued work, then leave; never cut a running prompt short
    void queue.then(() => process.exit(0));
  });
}
