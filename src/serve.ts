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
import { spawnSync } from 'node:child_process';
import { createInterface } from 'node:readline';
import { randomUUID } from 'node:crypto';
import { buildContext } from './core/context.js';
import { parseRule } from './core/permissions.js';
import { restoreSnapshot } from './core/git-snapshot.js';
import { hasKey, keyProblem, loadConfig, loadModels, pickModel, saveConfig, sessionsDir } from './core/config.js';
import { listDrafts, promoteDraft, saveDraft } from './core/skills.js';
import { join, resolve } from 'node:path';
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

const ruleNote = (rule: string) =>
  rule.startsWith('tool:')
    ? 'always allow this tool'
    : rule.startsWith('path:')
      ? 'allow writes under this path'
      : 'allow bash commands with this prefix';

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
  const { agent, store } = opened;
  let cwd = opened.cwd; // /cd moves it (agent.cwd is the same truth)
  const config = await loadConfig();
  // the persisted threshold (or 'off') must survive restarts
  if (config.autocompact === 'off') agent.compactAt = 0;
  else if (typeof config.autocompact === 'number') agent.compactAt = config.autocompact;
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
  // token-plan users watch tokens more than dollars (qwen4 plan T5)
  const sessionTokens = { input: 0, output: 0, cacheRead: 0 };
  agent.events.on('turn_end', (e) => {
    sessionCostUsd += e.costUsd;
    sessionTokens.input += e.usage.input;
    sessionTokens.output += e.usage.output;
    sessionTokens.cacheRead += e.usage.cacheRead ?? 0;
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
    autocompact: agent.compactAt,
    statusline: config.statusline,
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
  // queue introspection: what's running and what is waiting behind it
  let runningText: string | null = null;
  const queuedTexts: { text: string; enqueuedAt: number }[] = [];

  /**
   * Drop turns[k..]: restore code to the workspace state at the boundary just
   * before turns[k] ran, then truncate the projection to match.
   *
   * boundary(k) = the last checkpoint logged before turns[k]'s user message
   * (= post-snapshot of turn k-1), falling back to the first checkpoint after
   * it (= turn k's own pre-snapshot) when k is the first writing turn.
   */
  async function rewindToTurn(k: number): Promise<{ ok: true; data: Record<string, unknown> } | { ok: false; error: string }> {
    const list = turns();
    // k == list.length is the redo-to-latest case: drop nothing, keep every turn
    if (k < 0 || k > list.length || list.length === 0) return { ok: false, error: 'nothing to rewind/redo' };
    const dropAll = k === list.length;
    const target = list[dropAll ? list.length - 1 : k];
    const cutEntry = dropAll ? target.end : target.entryIndex - 1;
    let code: { restored?: number; deleted?: string[]; kept?: string[] } = {};
    const cps = store
      .indexedEntries()
      .filter(({ entry }) => entry.kind === 'checkpoint')
      .map(({ entry, index }) => ({ hash: (entry as { hash: string }).hash, index }));
    // boundary = the FIRST checkpoint logged at/after the cut: the snapshot taken as the
    // dropped range began, which includes files the user made between turns (C1). When the
    // dropped range had no writes at all, no checkpoint exists there — code is untouched.
    let boundaryIdx = cps.findIndex((cp) => cp.index > target.entryIndex);
    if (!dropAll && boundaryIdx < 0) {
      // the dropped range begins with a no-write turn: fall back to the previous turn's
      // end state; user between-turn files are outside the snapshot system and stay put
      for (let i = 0; i < cps.length; i++) if (cps[i].index < target.entryIndex) boundaryIdx = i;
    }
    if (dropAll) boundaryIdx = cps.length - 1; // redo-to-latest: restore the last known state
    if (boundaryIdx >= 0) {
      const later = cps.slice(boundaryIdx + 1).map((c) => c.hash);
      code = await restoreSnapshot(cwd, cps[boundaryIdx].hash, later);
    }
    await store.rewindTo(cutEntry);
    return { ok: true, data: { codeRestored: boundaryIdx >= 0, turnDropped: k, ...code } };
  }

  /** index of the last turn visible in the current projection (C3: set membership, not length) */
  function lastVisibleTurn(list: ReturnType<typeof turns>): number {
    const entries = store.indexedEntries();
    let R = -1;
    let to = -1;
    for (const { entry, index } of entries) {
      if (entry.kind === 'rewind') {
        R = index;
        to = (entry as { to: number }).to;
      }
    }
    const visible = (i: number) => (R < 0 ? true : i <= to || i > R);
    let last = -1;
    for (let i = 0; i < list.length; i++) if (visible(list[i].entryIndex)) last = i;
    return last;
  }

  /** last raw entry index visible in the current projection (fork's default position) */
  function lastVisibleEntry(): number {
    const entries = store.indexedEntries();
    for (let i = entries.length - 1; i >= 0; i--) {
      const { entry } = entries[i];
      if (entry.kind === 'rewind') {
        const to = (entry as { to: number }).to;
        return Math.max(to, entries.length - 1);
      }
    }
    return entries.length - 1;
  }

  function timeTravel(cmd: Command): Promise<void> | void {
    if (promptInFlight) return reply(cmd, false, undefined, 'a turn is running — interrupt it first');
    const list = turns();
    // visibility predicate shared by timeline flags and lastVisibleTurn (rewind-aware)
    const visibleFn = (() => {
      let R = -1;
      let to = -1;
      for (const { entry, index } of store.indexedEntries()) {
        if (entry.kind === 'rewind') {
          R = index;
          to = (entry as { to: number }).to;
        }
      }
      return (i: number) => (R < 0 ? true : i <= to || i > R);
    })();
    if (cmd.cmd === 'timeline') {
      reply(cmd, true, {
        nodes: list.map((t) => ({
          entryIndex: t.entryIndex, ts: t.ts, firstLine: t.firstLine, hasSnapshot: !!t.hash,
          orphaned: !visibleFn(t.entryIndex)
        }))
      });
      return;
    }
    const last = lastVisibleTurn(list);
    const finish = (r: Awaited<ReturnType<typeof rewindToTurn>>) =>
      r.ok ? reply(cmd, true, r.data) : reply(cmd, false, undefined, r.error);
    if (cmd.cmd === 'rewind') {
      const entryIndex = Number(cmd.entryIndex);
      const k = list.findIndex((t) => t.entryIndex === entryIndex);
      if (k < 0) return reply(cmd, false, undefined, `rewind: no turn at entry ${entryIndex}`);
      // include: true = recover that turn (e.g. an orphaned /find hit): keep it, drop after
      return rewindToTurn(cmd.include === true ? k + 1 : k).then(finish);
    }
    if (cmd.cmd === 'undo') {
      if (last < 0) return reply(cmd, false, undefined, 'nothing to undo');
      return rewindToTurn(last).then(finish);
    }
    if (cmd.cmd === 'redo') {
      // last + 2 == list.length means "restore everything" — a valid redo
      if (last + 2 > list.length) return reply(cmd, false, undefined, 'nothing to redo');
      return rewindToTurn(last + 2).then(finish);
    }
  }

  async function handle(cmd: Command): Promise<void> {
    switch (cmd.cmd) {
      case 'prompt': {
        let text = String(cmd.text ?? '');
        if (!text.trim()) return reply(cmd, false, undefined, 'prompt: empty text');
        if (perm.plan?.on) text = `[plan mode] 先只做研究和计划，不要尝试写入或执行修改。产出一份清晰的执行计划，等用户批准后 /plan off 再执行。\n\n${text}`;
        try {
          const msg = await agent.prompt(text, (ev) => {
            if (ev.type !== 'done') out({ type: 'stream', event: ev });
          });
          reply(cmd, true, { stopReason: msg.stopReason ?? 'stop' });
        } catch (e: any) {
          reply(cmd, false, undefined, String(e?.message ?? e));
        } finally {
          // promote the next queued prompt (the interrupt handler drains the queue itself)
          const next = queuedTexts.shift();
          if (next) runningText = next.text;
          else {
            runningText = null;
            promptInFlight = false;
          }
        }
        return;
      }
      case 'usage':
      case 'stats':
      case 'insights': {
        // aggregate over every session log in TANGO_HOME (the log is the truth)
        const byModel = new Map<string, { model: string; requests: number; input: number; output: number; costUsd: number }>();
        const bySession: { id: string; name: string; costUsd: number; messages: number }[] = [];
        let totalCostUsd = 0;
        for (const summ of await SessionStore.list(sessionsDir())) {
          const st = await SessionStore.open(sessionsDir(), summ.id);
          let sessionCost = 0;
          for (const m of st.allMessages()) {
            if (m.role !== 'assistant' || !m.usage) continue;
            const model = pricedModels.find((p) => p.id === m.model) ?? agent.model;
            const cost = costUsd(model, m.usage);
            sessionCost += cost;
            totalCostUsd += cost;
            const row = byModel.get(m.model ?? model.id) ?? { model: m.model ?? model.id, requests: 0, input: 0, output: 0, costUsd: 0 };
            row.requests++;
            row.input += m.usage.input;
            row.output += m.usage.output;
            row.costUsd += cost;
            byModel.set(row.model, row);
          }
          bySession.push({ id: summ.id, name: summ.name ?? '', costUsd: sessionCost, messages: summ.messages });
        }
        const totalTokens = { input: 0, output: 0, cacheRead: 0 };
        for (const m of byModel.values()) {
          totalTokens.input += m.input;
          totalTokens.output += m.output;
        }
        const data = {
          totalCostUsd,
          totalTokens,
          byModel: [...byModel.values()].sort((a, b) => b.costUsd - a.costUsd),
          bySession: bySession.sort((a, b) => b.costUsd - a.costUsd),
          since: undefined as string | undefined
        };
        const esc = (t: string) => t.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
        if (cmd.cmd === 'insights') {
          const maxCost = Math.max(...data.bySession.map((x) => x.costUsd), 1);
          const rows = (title: string, rowsHtml: string) => `<h2>${title}</h2><table>${rowsHtml}</table>`;
          const html = `<!doctype html><meta charset="utf-8"><title>tango insights</title><style>body{font-family:ui-sans-serif,system-ui;margin:2em auto;max-width:52em;padding:0 1em;line-height:1.5}h1,h2{color:#333}table{border-collapse:collapse;width:100%;margin-bottom:1.5em}td,th{border-bottom:1px solid #ddd;padding:6px 10px;text-align:left}td.num,th.num{text-align:right;font-variant-numeric:tabular-nums}.bar{background:#6b8dd6;height:10px}</style>
<h1>tango 使用报告</h1><p>总花费 <b>$${totalCostUsd.toFixed(4)}</b> · 会话数 ${data.bySession.length}</p>
${rows('按模型', '<tr><th>模型</th><th class=num>请求</th><th class=num>输入 tok</th><th class=num>输出 tok</th><th class=num>花费</th></tr>' + data.byModel.map((m) => `<tr><td>${esc(m.model)}</td><td class=num>${m.requests}</td><td class=num>${m.input}</td><td class=num>${m.output}</td><td class=num>$${m.costUsd.toFixed(4)}</td></tr>`).join(''))}
${rows('按会话', '<tr><th>会话</th><th>名称</th><th class=num>消息</th><th class=num>花费</th><th></th></tr>' + data.bySession.map((x) => `<tr><td>${x.id.slice(0, 8)}</td><td>${x.name || ''}</td><td class=num>${x.messages}</td><td class=num>$${x.costUsd.toFixed(4)}</td><td><div class=bar style="width:${Math.round((x.costUsd / maxCost) * 100)}px"></div></td></tr>`).join(''))}`;
          const { writeFile: wf } = await import('node:fs/promises');
          const path = join(cwd, `tango-insights-${new Date().toISOString().slice(0, 10)}.html`);
          await wf(path, html, 'utf8');
          reply(cmd, true, { path });
          return;
        }
        reply(cmd, true, data);
        return;
      }
      case 'set_autocompact': {
        // accepts a fraction (0.5), a percent (50), or 'off'; anything else is refused
        const raw = cmd.value;
        let value: number;
        if (raw === 'off' || raw === 0) value = 0;
        else if (typeof raw === 'number' && raw > 0 && raw < 1) value = raw;
        else if (typeof raw === 'number' && Number.isInteger(raw) && raw >= 1 && raw <= 100) value = raw / 100;
        else if (typeof raw === 'string' && /^\d+(\.\d+)?%?$/.test(raw)) {
          const n = parseFloat(raw.replace('%', ''));
          value = raw.endsWith('%') || n >= 1 ? n / 100 : n;
        } else return reply(cmd, false, undefined, 'autocompact: use a fraction (0.5), percent (50 / 50%) or "off"');
        agent.compactAt = value;
        await saveConfig({ ...(await loadConfig()), autocompact: value === 0 ? 'off' : value });
        reply(cmd, true, { autocompact: value });
        return;
      }
      case 'init': {
        const { readFile: rf3, writeFile: wf3, readdir: rd3 } = await import('node:fs/promises');
        const agentsPath = join(cwd, 'AGENTS.md');
        try {
          await rf3(agentsPath);
          if (cmd.force !== true) return reply(cmd, false, undefined, 'AGENTS.md already exists — /init --force to append a tango section');
        } catch {}
        // toolchain detection from marker files
        const markers: [string, string][] = [
          ['package.json', 'Node/TypeScript (npm)'],
          ['Cargo.toml', 'Rust (cargo)'],
          ['pyproject.toml', 'Python (uv/pip)'],
          ['go.mod', 'Go'],
          ['pom.xml', 'Java (Maven)'],
          ['requirements.txt', 'Python (pip)']
        ];
        const found: string[] = [];
        let pkgName = '';
        for (const [file, label] of markers) {
          try {
            await rf3(join(cwd, file));
            found.push(label);
            if (file === 'package.json') {
              try {
                pkgName = JSON.parse(await rf3(join(cwd, file), 'utf8')).name ?? '';
              } catch {}
            }
          } catch {}
        }
        // shallow directory tree (depth 2, max 40 entries)
        const tree: string[] = [];
        const walk = async (dir: string, depth: number) => {
          if (depth > 2 || tree.length > 40) return;
          let entries: string[] = [];
          try {
            entries = (await rd3(dir)).filter((e) => !e.startsWith('.') && e !== 'node_modules' && e !== 'target').sort();
          } catch { return; }
          for (const e of entries.slice(0, 20)) {
            tree.push(`${'  '.repeat(depth)}- ${e}`);
            await walk(join(dir, e), depth + 1);
          }
        };
        await walk(cwd, 0);
        const body = `# AGENTS.md${pkgName ? ` — ${pkgName}` : ''}\n\n> 由 /init 生成 — 请按项目实际情况补全。tango 每次请求自动读取本文件。\n\n## 项目结构\n\n${tree.join('\n') || '（空目录）'}\n\n## 工具链\n\n${found.map((f) => `- ${f}`).join('\n') || '- 未识别（手工补充）'}\n\n## 约定\n\n- （构建/测试命令，例如 npm test）\n- （代码风格、目录约定）\n- （禁区：不要改的文件/目录）\n`;
        if (cmd.force === true) {
          const prev = await rf3(agentsPath, 'utf8').catch(() => '');
          await wf3(agentsPath, prev + '\n' + body, 'utf8');
        } else {
          await wf3(agentsPath, body, 'utf8');
        }
        reply(cmd, true, { path: agentsPath });
        return;
      }
      case 'doctor': {
        const { accessSync, constants } = await import('node:fs');
        const checks: { name: string; ok: boolean; detail: string }[] = [];
        const add = (name: string, ok: boolean, detail = '') => checks.push({ name, ok, detail });
        const gitOkDoctor = (() => {
          try {
            accessSync(join(cwd, '.git'), constants.F_OK);
            return true;
          } catch { return false; }
        })();
        add('git', gitOkDoctor, gitOkDoctor ? 'repo detected — time travel checkpoints active' : 'not a git repo — tar fallback snapshots');
        let homeWritable = false;
        try {
          const { mkdtemp: mk } = await import('node:fs/promises');
          const d = await mk(join(sessionsDir(), 'doctor-'));
          const { rm: rmx } = await import('node:fs/promises');
          await rmx(d, { recursive: true, force: true });
          homeWritable = true;
        } catch {}
        add('TANGO_HOME writable', homeWritable);
        add('sessions dir', homeWritable, sessionsDir());
        const hasKey = !keyProblem(agent.model);
        add('model key', hasKey, hasKey ? `${agent.model.id} ready` : `${agent.model.id}: no api key configured`);
        const nodeV = process.version;
        add('node', true, nodeV);
        reply(cmd, true, { checks });
        return;
      }
      case 'debug_zip': {
        // offline diagnostics bundle: doctor + sanitized config + session inventory (names only)
        const { mkdtemp: mk2, writeFile: wf4, readdir: rd4, rm: rm2 } = await import('node:fs/promises');
        const { tmpdir } = await import('node:os');
          const tmp = await mk2(join(tmpdir(), 'tango-debug-'));
        try {
          const cfg = await loadConfig();
          const doctor = {
            version: VERSION,
            protocol: PROTOCOL,
            node: process.version,
            platform: process.platform,
            cwd,
            sessionId: store.id,
            config: (() => {
              const c = { ...(cfg as Record<string, unknown>) };
              if (c.feishuWebhook) c.feishuWebhook = '[REDACTED]';
              return c;
            })(),
            sessions: (await SessionStore.list(sessionsDir())).map((x) => ({ id: x.id, createdAt: x.createdAt, messages: x.messages, cwd: x.cwd, name: x.name }))
          };
          await wf4(join(tmp, 'doctor.json'), JSON.stringify(doctor, null, 2), 'utf8');
          const stamp = new Date().toISOString().slice(0, 10);
          const zipPath = join(cwd, `tango-debug-${stamp}.zip`);
          const z = spawnSync('zip', ['-j', '-q', zipPath, join(tmp, 'doctor.json')], { encoding: 'utf8' });
          if (z.status !== 0) {
            // no zip: fall back to copying the report next to the project
            const fallback = join(cwd, `tango-debug-${stamp}.json`);
            await wf4(fallback, JSON.stringify(doctor, null, 2), 'utf8');
            reply(cmd, true, { path: fallback, format: 'json' });
            return;
          }
          reply(cmd, true, { path: zipPath, format: 'zip' });
          return;
        } finally {
          await rm2(tmp, { recursive: true, force: true });
        }
      }
      case 'set_effort': {
        const raw = String(cmd.level ?? 'off');
        const level = raw === 'low' || raw === 'medium' || raw === 'high' ? raw : undefined;
        if (raw !== 'off' && !level) return reply(cmd, false, undefined, 'effort: low | medium | high | off');
        agent.effort = level;
        reply(cmd, true, { effort: level ?? 'off' });
        return;
      }
      case 'set_plan': {
        perm.plan = perm.plan ?? { on: false };
        perm.plan.on = cmd.on === true;
        reply(cmd, true, { plan: perm.plan.on });
        return;
      }
      case 'cd': {
        const dir = String(cmd.dir ?? '').trim();
        const target = resolve(cwd, dir);
        const { statSync } = await import('node:fs');
        let isDir = false;
        try {
          isDir = statSync(target).isDirectory();
        } catch {}
        if (!isDir) return reply(cmd, false, undefined, `cd: not a directory: ${target}`);
        cwd = target;
        agent.setCwd(target);
        await store.append({ kind: 'meta', id: store.id, createdAt: store.meta()?.createdAt ?? new Date().toISOString(), cwd: target, name: store.meta()?.name });
        reply(cmd, true, { cwd: target });
        return;
      }
      case 'pwd': {
        reply(cmd, true, { cwd });
        return;
      }
      case 'add_dir': {
        const dir = String(cmd.dir ?? '').trim();
        const target = resolve(cwd, dir);
        const { statSync } = await import('node:fs');
        let isDir = false;
        try {
          isDir = statSync(target).isDirectory();
        } catch {}
        if (!isDir) return reply(cmd, false, undefined, `add_dir: not a directory: ${target}`);
        const cfg0 = await loadConfig();
        const dirs = [...new Set([...(cfg0.extraDirs ?? []), target])];
        await saveConfig({ ...cfg0, extraDirs: dirs });
        if (perm.extraDirs) perm.extraDirs.dirs = dirs;
        reply(cmd, true, { extraDirs: dirs });
        return;
      }
      case 'config_show': {
        const cfg0 = await loadConfig();
        reply(cmd, true, { config: cfg0, path: join(sessionsDir(), '..', 'config.json') });
        return;
      }
      case 'import': {
        // map other agents' allowed-tools into our rule grammar; unmappable → reported
        const source = String(cmd.source ?? '');
        const rawEntries: { raw: string }[] = [];
        const { readFile: rf2 } = await import('node:fs/promises');
        const { homedir } = await import('node:os');
        let rawJson = '';
        try {
          if (source === 'claude-code') rawJson = await rf2(join(cwd, '.claude', 'settings.json'), 'utf8');
          else if (source === 'grok') rawJson = await rf2(join(homedir(), '.grok', 'settings.json'), 'utf8');
          else return reply(cmd, false, undefined, `import: unknown source "${source}" (claude-code | grok)`);
          const parsed = JSON.parse(rawJson);
          for (const e of parsed?.permissions?.allow ?? []) rawEntries.push({ raw: String(e) });
        } catch (e: any) {
          return reply(cmd, false, undefined, `import ${source}: ${e?.message ?? e}`);
        }
        const imported: string[] = [];
        const skipped: { raw: string; reason: string }[] = [];
        for (const { raw } of rawEntries) {
          // Tool(Args) and bare Tool forms
          const m = /^(\w+)\((.*)\)$/.exec(raw);
          const tool = m ? m[1] : raw;
          const arg = m ? m[2] : '';
          const snake = tool.replace(/(?<=[a-z0-9])([A-Z])/g, '_$1').toLowerCase();
          if (m && (tool === 'Bash' || tool === 'bash')) {
            const cmdPrefix = arg.replace(/:?\*$/, '').split(':')[0].trim();
            if (cmdPrefix) imported.push(`bash:${cmdPrefix}`);
            else skipped.push({ raw, reason: 'no command prefix' });
          } else if (m && ['Write', 'Edit', 'write', 'edit'].includes(tool)) {
            const raw0 = arg.replace(/[*]+$/, '').split('**')[0].replace(/[/]+$/, '');
            const p = raw0.startsWith('~/') ? join(homedir(), raw0.slice(2)) : raw0;
            if (p && !p.includes('*')) imported.push(`path:${p}`);
            else skipped.push({ raw, reason: 'wildcard path not representable' });
          } else if (!m || !arg) {
            // bare tool name → tool rule if we have that tool
            if (buildTools().get(snake)) imported.push(`tool:${snake}`);
            else skipped.push({ raw, reason: `no matching tool "${snake}"` });
          } else {
            skipped.push({ raw, reason: 'unsupported shape' });
          }
        }
        const cfg0 = await loadConfig();
        const allow = [...new Set([...(cfg0.permissions?.allow ?? []), ...imported])];
        await saveConfig({ ...cfg0, permissions: { allow } });
        if (perm.rules) perm.rules.allow = allow;
        reply(cmd, true, { imported, skipped });
        return;
      }
      case 'permissions_list':
      case 'permissions_add':
      case 'permissions_remove': {
        const cfg0 = await loadConfig();
        const allow = cfg0.permissions?.allow ? [...cfg0.permissions.allow] : [];
        if (cmd.cmd === 'permissions_list') {
          reply(cmd, true, { allow: allow.map((rule) => ({ rule, note: ruleNote(rule) })) });
          return;
        }
        if (cmd.cmd === 'permissions_add') {
          const raw = String(cmd.rule ?? '').trim();
          const p = parseRule(raw);
          if (!p.ok) return reply(cmd, false, undefined, p.error);
          if (!allow.includes(raw)) allow.push(raw);
          await saveConfig({ ...(await loadConfig()), permissions: { allow } });
          if (perm.rules) perm.rules.allow = allow;
          reply(cmd, true, { allow });
          return;
        }
        const idx = allow.indexOf(String(cmd.rule ?? ''));
        if (idx < 0) return reply(cmd, false, undefined, `rule not found: ${cmd.rule}`);
        allow.splice(idx, 1);
        await saveConfig({ ...(await loadConfig()), permissions: { allow } });
        if (perm.rules) perm.rules.allow = allow;
        reply(cmd, true, { allow });
        return;
      }
      case 'preset_save':
      case 'preset_list':
      case 'preset_get': {
        // prompt templates live in TANGO_HOME/presets.json as {name: text}
        const { readFile: rf, writeFile: wf } = await import('node:fs/promises');
        const file = join(sessionsDir(), '..', 'presets.json');
        let presets: Record<string, string> = {};
        try {
          presets = JSON.parse(await rf(file, 'utf8'));
        } catch {}
        if (cmd.cmd === 'preset_save') {
          const name = String(cmd.name ?? '').trim();
          const text = String(cmd.text ?? '');
          if (!name || !text.trim()) return reply(cmd, false, undefined, 'preset_save: need name and text');
          const overwritten = presets[name] !== undefined;
          presets[name] = text;
          await wf(file, JSON.stringify(presets, null, 2), 'utf8');
          reply(cmd, true, { name, overwritten });
          return;
        }
        if (cmd.cmd === 'preset_list') {
          reply(cmd, true, {
            presets: Object.entries(presets)
              .sort((a, b) => a[0].localeCompare(b[0]))
              .map(([name, text]) => ({ name, preview: text.split('\n')[0].slice(0, 60) }))
          });
          return;
        }
        const text = presets[String(cmd.name ?? '')];
        if (text === undefined) return reply(cmd, false, undefined, `preset not found: ${cmd.name}`);
        reply(cmd, true, { text });
        return;
      }
      case 'set_statusline': {
        const template = cmd.template === null ? null : String(cmd.template ?? '');
        if (template !== null && !template.includes('{')) {
          return reply(cmd, false, undefined, 'statusline: template has no {placeholder}');
        }
        await saveConfig({ ...(await loadConfig()), statusline: template ?? undefined });
        reply(cmd, true, { template });
        return;
      }
      case 'queue': {
        reply(cmd, true, {
          running: runningText,
          queued: queuedTexts.map((q) => ({ text: q.text, waitedMs: Date.now() - q.enqueuedAt }))
        });
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
      case 'context': {
        // token breakdown of the next request's assembly (chars/3.5, same as Agent.estimateTokens)
        const { systemPrompt, skills } = await buildContext(cwd);
        const skillLines = skills.length
          ? '# Skills\n\nAvailable skills. When one matches the task, read its SKILL.md and follow it:\n\n' +
            skills.map((sk: { name: string; description: string; path: string }) => `- ${sk.name}: ${sk.description} (at ${sk.path}; read it before following it)`).join('\n')
          : '';
        const est = (t: string) => Math.round(t.length / 3.5);
        const skillTokens = est(skillLines);
        // systemPrompt already contains the skills block — subtract it so rows sum to total
        const system = Math.max(0, est(systemPrompt) - skillTokens);
        const tools = est(JSON.stringify(buildTools().schemas()));
        const history = agent.contextTokenEstimate();
        const total = system + tools + history;
        const window = agent.model.contextWindow ?? 128_000;
        reply(cmd, true, {
          system, skills: skillTokens, tools, history, total,
          window, pct: Math.round((total / window) * 100)
        });
        return;
      }
      case 'timeline':
      case 'rewind':
      case 'undo':
      case 'redo':
        return timeTravel(cmd);
      case 'find': {
        // full-text search across ALL sessions, including entries abandoned by
        // rewinds — the whole point is finding a discarded future again
        const needle = String(cmd.text ?? '').toLowerCase();
        if (!needle) return reply(cmd, false, undefined, 'find: empty query');
        const hits: { sessionId: string; entryIndex: number; ts: string; firstLine: string; snippet: string; orphaned: boolean }[] = [];
        for (const summ of await SessionStore.list(sessionsDir())) {
          if (hits.length >= 200) break;
          const st = await SessionStore.open(sessionsDir(), summ.id);
          const visible = new Set(st.visibleEntries());
          let perSession = 0;
          for (const { entry, index } of st.indexedEntries()) {
            if (perSession >= 20) break;
            if (entry.kind !== 'message') continue;
            for (const b of entry.message.content) {
              const text = b.type === 'text' ? b.text : b.type === 'tool_call' ? JSON.stringify(b.input ?? {}) : '';
              const at = text.toLowerCase().indexOf(needle);
              if (at < 0) continue;
              const from = Math.max(0, at - 40);
              hits.push({
                sessionId: summ.id,
                entryIndex: index,
                ts: entry.ts,
                firstLine: text.split('\n')[0].slice(0, 80),
                snippet: text.slice(from, from + 120),
                orphaned: !visible.has(entry)
              });
              perSession++;
              break;
            }
          }
        }
        reply(cmd, true, { hits });
        return;
      }
      case 'export': {
        const format = cmd.format === 'html' ? 'html' : 'md';
        const parts: string[] = [];
        let inOrphan = false;
        const indexed = store.indexedEntries();
        const visible = new Set(store.visibleEntries());
        for (const { entry, index } of indexed) {
          if (entry.kind !== 'message') continue;
          const orph = !visible.has(entry);
          if (orph !== inOrphan) {
            parts.push(orph ? '\n---\n**⟲ 以下为已回滚的未来（/redo 可找回）**\n' : '\n---\n**⟲ 回到当前时间线**\n');
            inOrphan = orph;
          }
          for (const b of entry.message.content) {
            if (b.type === 'text') parts.push(`**${entry.message.role}**: ${b.text}\n`);
            else if (b.type === 'tool_call') parts.push(`*tool ${b.name}* \`${JSON.stringify(b.input ?? {}).slice(0, 200)}\`\n`);
            else if (b.type === 'tool_result')
              parts.push(`*result* ${b.content.map((c) => (c.type === 'text' ? c.text : '[image]')).join('\n').slice(0, 500)}\n`);
          }
        }
        const body = parts.join('\n');
        const out = format === 'html'
          ? `<!doctype html><meta charset="utf-8"><title>tango export</title><style>body{font-family:ui-monospace,monospace;max-width:60em;margin:2em auto;line-height:1.5;padding:0 1em}hr{border:0;border-top:1px dashed #999}</style><pre>${body.replace(/&/g, '&amp;').replace(/</g, '&lt;')}</pre>`
          : `# tango 会话导出\n\n${body}`;
        const path = join(cwd, `tango-export-${store.id.slice(0, 8)}.${format}`);
        const { writeFile: wf } = await import('node:fs/promises');
        await wf(path, out, 'utf8');
        reply(cmd, true, { path, format });
        return;
      }
      case 'transcript': {
        const lines: string[] = [];
        const visible = new Set(store.visibleEntries());
        for (const { entry, index } of store.indexedEntries()) {
          if (entry.kind !== 'message') continue;
          const orph = !visible.has(entry);
          const tag = orph ? '⟲' : ' ';
          for (const b of entry.message.content) {
            if (b.type === 'text') lines.push(`${tag} ${entry.message.role}: ${b.text}`);
            else if (b.type === 'tool_call') lines.push(`${tag} tool ${b.name}: ${JSON.stringify(b.input ?? {}).slice(0, 200)}`);
            else if (b.type === 'tool_result')
              lines.push(`${tag} result: ${b.content.map((c) => (c.type === 'text' ? c.text : '[image]')).join(' ').slice(0, 400)}`);
          }
        }
        reply(cmd, true, { text: lines.join('\n') });
        return;
      }
      case 'rename': {
        const name = String(cmd.name ?? '').trim();
        if (!name) return reply(cmd, false, undefined, 'rename: empty name');
        await store.append({ kind: 'meta', id: store.id, createdAt: store.meta()?.createdAt ?? new Date().toISOString(), cwd, name });
        reply(cmd, true, { name });
        return;
      }
      case 'recap': {
        try {
          const msg = await agent.ask(
            '用一两句话总结当前对话到目前为止的要点，直接给出总结，不要任何前缀。',
            (ev) => { if (ev.type !== 'done') out({ type: 'btw_stream', id: cmd.id, event: ev }); }
          );
          const text = msg.content.filter((b) => b.type === 'text').map((b) => (b as { text: string }).text).join('');
          reply(cmd, true, { text });
        } catch (e: any) {
          reply(cmd, false, undefined, String(e?.message ?? e));
        }
        return;
      }
      case 'fork': {
        if (promptInFlight) return reply(cmd, false, undefined, 'a turn is running — interrupt it first');
        const list = turns();
        const entryIndex = cmd.entryIndex === undefined
          ? lastVisibleEntry() // fork from the current (post-rewind) position
          : Number(cmd.entryIndex);
        const at = list.find((t) => t.entryIndex === entryIndex);
        const toIndex = at ? at.end : Math.max(0, Math.min(entryIndex, store.indexedEntries().length - 1));
        const forkId = randomUUID();
        await store.forkTo(sessionsDir(), forkId, toIndex, cmd.name ? String(cmd.name) : undefined);
        reply(cmd, true, { sessionId: forkId, name: cmd.name ?? null });
        return;
      }
      case 'branch': {
        if (promptInFlight) return reply(cmd, false, undefined, 'a turn is running — interrupt it first');
        const list = turns();
        const entryIndex = cmd.entryIndex === undefined ? undefined : Number(cmd.entryIndex);
        let k: number;
        if (entryIndex === undefined) {
          k = lastVisibleTurn(list);
        } else {
          k = list.findIndex((t) => t.entryIndex === entryIndex);
        }
        if (k < 0 || k >= list.length) return reply(cmd, false, undefined, 'branch: no such turn');
        // keep the named turn, drop everything after it (same slice fork would copy)
        const r = await rewindToTurn(k + 1);
        if (!r.ok) return reply(cmd, false, undefined, r.error);
        reply(cmd, true, { branch: true, branchedAt: list[k].entryIndex, ...r.data });
        return;
      }
      case 'tree': {
        const sessions = await SessionStore.list(sessionsDir());
        const metas = await Promise.all(
          sessions.map(async (summ) => {
            const st = await SessionStore.open(sessionsDir(), summ.id);
            return st.meta();
          })
        );
        reply(cmd, true, {
          nodes: metas.map((m) => ({
            sessionId: m?.id,
            name: m?.name,
            createdAt: m?.createdAt,
            forkOf: m?.forkOf,
            cwd: m?.cwd
          }))
        });
        return;
      }
      case 'status':
        reply(cmd, true, {
          sessionId: store.id,
          model: agent.model.id,
          mode: perm.mode,
          cwd,
          contextMessages: store.messageCount(),
          totalCostUsd: sessionCostUsd,
          contextTokens: agent.contextTokenEstimate(),
          contextWindow: agent.model.contextWindow ?? 128_000,
          autocompact: agent.compactAt,
          effort: agent.effort ?? 'off',
          sessionTokens
        });
        return;
      default:
        reply(cmd, false, undefined, `unknown command "${cmd.cmd}"`);
    }
  }

  // prompts run one at a time; interrupt and permission answers jump the queue
  let queue: Promise<void> = Promise.resolve();
  // bumped on interrupt: queued prompts captured under an older generation never run
  let interruptGen = 0;
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
      // drop the queued prompts (they never run) and abort the running turn; the
      // running turn's own settle path clears runningText/promptInFlight, so time
      // travel stays guarded until the abort has fully landed
      denyPending();
      queuedTexts.length = 0;
      interruptGen++;
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
    const guarded = (p: Promise<unknown>) =>
      void p.catch((e) => out({ type: 'error', message: String(e?.message ?? e) }));
    if (['set_mode', 'status', 'timeline', 'context', 'queue', 'usage', 'stats', 'insights', 'preset_save', 'preset_list', 'preset_get', 'permissions_list', 'permissions_add', 'permissions_remove', 'import', 'cd', 'pwd', 'add_dir', 'config_show', 'init', 'doctor', 'debug_zip', 'set_plan', 'set_effort'].includes(cmd.cmd)) {
      guarded(handle(cmd));
      return;
    }
    if (cmd.cmd === 'rewind' || cmd.cmd === 'undo' || cmd.cmd === 'redo') {
      guarded(Promise.resolve(timeTravel(cmd)));
      return;
    }
    if (cmd.cmd === 'fork' || cmd.cmd === 'branch' || cmd.cmd === 'tree' || cmd.cmd === 'find' || cmd.cmd === 'export' || cmd.cmd === 'transcript' || cmd.cmd === 'rename') {
      guarded(handle(cmd));
      return;
    }
    // /queue bookkeeping lives at dispatch time: the serial queue means handle()
    // for a queued prompt only starts after its predecessor finished
    let wasQueued = false;
    if (cmd.cmd === 'prompt') {
      const text = String(cmd.text ?? '');
      if (promptInFlight && text.trim()) {
        queuedTexts.push({ text, enqueuedAt: Date.now() });
        wasQueued = true;
      } else if (text.trim()) {
        promptInFlight = true;
        runningText = text;
      }
    }
    const gen = interruptGen;
    queue = queue
      .then(() => {
        // an interrupt since this prompt was queued dropped it on purpose
        if (wasQueued && gen !== interruptGen && cmd.id !== undefined) {
          out({ type: 'result', id: cmd.id, ok: false, error: 'interrupted (dropped from queue)' });
          return;
        }
        return handle(cmd);
      })
      .catch((e) => out({ type: 'error', message: String(e?.message ?? e) }));
  });
  rl.on('close', () => {
    // stdin EOF (a pipe, not a UI): finish queued work, then leave; never cut a running prompt short
    void queue.then(() => process.exit(0));
  });
}
