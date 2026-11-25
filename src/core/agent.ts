import { existsSync } from 'node:fs';
import { buildContext, findAgentsFiles } from './context.js';
import { EventBus } from './events.js';
import { adapterFor, costUsd } from './models/registry.js';
import { SUMMARY_OPEN, type SessionStore } from './session.js';
import type { ToolRegistry } from './tools/registry.js';
import type {
  CompletionRequest,
  ContentBlock,
  ImageBlock,
  Message,
  ModelSpec,
  RetryInfo,
  StreamEvent,
  ToolCallBlock,
  ToolResult,
  ToolResultBlock
} from './types.js';

/** Shown when a turn would start in a directory that is no longer there. */
export function cwdMissingMessage(cwd: string): string {
  return `工作目录不存在：${cwd}。请选择用过的目录，或输入 /路径（也可 /cd <目录>）。`;
}

export interface AgentOptions {
  store: SessionStore;
  tools: ToolRegistry;
  model: ModelSpec;
  cwd: string;
  events?: EventBus;
  maxTurns?: number;
  /** retries for a model request that fails before any output (config `retries`; default 10) */
  retries?: number;
  /** compact when the estimated context passes this share of the window (0 disables) */
  compactAt?: number;
}

/** verbatim tail kept after compaction, in estimated tokens */
export const COMPACT_KEEP_TOKENS = 20_000;
/** tool results are cut to this many chars in the summarizer transcript */
const COMPACT_TOOL_CHARS = 2000;

const COMPACT_FORMAT =
  'Use exactly these markdown sections: ## Goal, ## Constraints, ## Progress, ## Key Decisions (with reasons), ' +
  '## Next Steps, ## Critical Context (exact paths, commands, ids, error messages, numbers). Omit a section only ' +
  'when it is empty. Be dense and factual. This is data for a successor model that will NOT see the original ' +
  'messages, not a reply to a human. Write in the language the user used. The most recent messages stay in ' +
  'context verbatim after this summary, so focus on what came before them.';
const COMPACT_INSTRUCTION = 'Summarize the conversation above. ' + COMPACT_FORMAT;
const COMPACT_UPDATE_INSTRUCTION =
  'Above is the summary from the previous compaction, followed by the conversation since then. Write an ' +
  'updated summary: keep everything from the previous one that still matters, fold in the new messages, ' +
  'drop what is obsolete. ' + COMPACT_FORMAT;

const HANDOFF_INSTRUCTION =
  'Write a handoff brief for ANOTHER session that will continue this work without seeing the messages above. ' +
  'Use these markdown sections: ## Goal, ## Done so far, ## Key decisions (with reasons), ' +
  '## Files touched (paths and what changed), ## Current state (what works, what is broken, last command results), ' +
  '## Next steps (ordered, concrete), ## Open questions. Omit empty sections. Be dense and factual; ' +
  'quote exact paths, commands, ids and error messages. Write in the language the user used.';

/**
 * The agent loop: append user message, then model → tools → model until the
 * model stops without tool calls. Every step is persisted and evented.
 */
/** A Stop hook can send the model back at most this many times per prompt (cost fuse). */
const MAX_STOP_CONTINUES = 3;

export class Agent {
  readonly events: EventBus;
  model: ModelSpec;
  /** USD accumulated by prompt() calls in this process. */
  totalCostUsd = 0;

  private readonly store: SessionStore;
  readonly tools: ToolRegistry;
  /** live working directory (/cd); tools and context read it per call */
  cwd: string;
  /** reasoning effort for adapters that support it (/effort) */
  effort?: 'low' | 'medium' | 'high';
  /** transient system-prompt suffix (plan mode) — never persisted into history */
  systemSuffix?: string;
  /** mid-turn supplements (Alt+Enter): appended as user messages before the next model call */
  private supplements: { text: string; images?: ImageBlock[] }[] = [];
  /** hook additionalContext: appended as user notes before the next model call */
  private hookContext: string[] = [];
  /** a hook's continue: false — the turn ends at the next safe point with this reason */
  private hookStop?: string;
  /** the AGENTS.md set last announced as instructions_loaded (joined paths) */
  private instructionsKey?: string;
  /** prompt() ran at least once; session_end only fires for sessions that did something */
  private prompted = false;
  private ended = false;
  private readonly maxTurns: number;
  /** config `retries`; a model's own `retries` wins, undefined means the default (3) */
  retries?: number;
  /** auto-compact threshold as a share of the context window; 0 disables (/autocompact) */
  compactAt: number;
  private controller?: AbortController;

  constructor(opts: AgentOptions) {
    this.store = opts.store;
    this.tools = opts.tools;
    this.cwd = opts.cwd;
    this.model = opts.model;
    this.events = opts.events ?? new EventBus();
    this.maxTurns = opts.maxTurns ?? 64;
    this.retries = opts.retries;
    this.compactAt = opts.compactAt ?? 0.75;
  }

  /** Stream one model request; failures before any output are retried and announced as `retry` events. */
  private streamModel(req: CompletionRequest): AsyncIterable<StreamEvent> {
    const retries = this.model.retries ?? this.retries;
    const onRetry = (info: RetryInfo) => void this.events.emit('retry', info);
    return adapterFor(this.model).stream(this.model, { ...req, retries, onRetry });
  }

    abort(): void {
    this.controller?.abort();
  }

  /** Add context to the RUNNING turn (Alt+Enter): the next model request sees it. */
  supplement(text: string, images?: ImageBlock[]): void {
    if (text.trim() || images?.length) {
      this.supplements.push({ text: text.trim(), images: images?.length ? images : undefined });
    }
  }

  /** Hook additionalContext: the model sees it before its next request (this turn or the next). */
  addContext(text: string): void {
    if (text.trim()) this.hookContext.push(text.trim());
  }

  /** A hook said continue: false: end the running turn after the current step. */
  requestStop(reason: string): void {
    if (this.controller) this.hookStop ??= reason;
  }

  /** flush pending supplements into the session as user messages */
  private async flushSupplements(): Promise<void> {
    while (this.hookContext.length) {
      const text = this.hookContext.shift()!;
      await this.store.appendMessage({ role: 'user', content: [{ type: 'text', text: `[hook context] ${text}` }] });
    }
    while (this.supplements.length) {
      const s = this.supplements.shift()!;
      await this.store.appendMessage({
        role: 'user',
        content: [{ type: 'text', text: `（补充）${s.text}` }, ...(s.images ?? [])]
      });
    }
  }

  setModel(model: ModelSpec): void {
    this.model = model;
    void this.store.setModel(model.id);
  }

  /** move the working directory (/cd); everything downstream reads this live */
  async setCwd(dir: string): Promise<void> {
    const oldCwd = this.cwd;
    this.cwd = dir;
    if (oldCwd !== dir) await this.events.emit('cwd_changed', { oldCwd, newCwd: dir });
  }

  /**
   * The session is closing (UI exit, EOF, end of a one-shot run): emit session_end once, for
   * sessions that ran at least one prompt. Awaited, so SessionEnd hooks finish before exit.
   */
  async end(reason: string): Promise<void> {
    if (this.ended || !this.prompted) return;
    this.ended = true;
    await this.events.emit('session_end', { reason });
  }

  contextTokenEstimate(): number {
    return this.estimateTokens();
  }

  /** Append a user message and loop (model → tools → model) until the model stops. */
  async prompt(text: string, onStream?: (ev: StreamEvent) => void, images?: ImageBlock[]): Promise<Message> {
    if (!existsSync(this.cwd)) throw new Error(cwdMissingMessage(this.cwd));
    this.prompted = true;
    const rejected = await this.events.emit('before_prompt', { text });
    if (rejected && rejected.block) throw new Error(`prompt blocked by hook: ${rejected.reason}`);
    const user: Message = {
      role: 'user',
      content: [{ type: 'text', text }, ...(images ?? [])]
    };
    await this.store.appendMessage(user);
    await this.events.emit('prompt_start', { text });

    this.controller = new AbortController();
    this.hookStop = undefined;
    let continued = 0; // times a Stop hook sent the model back this prompt (capped)
    try {
      for (let turn = 0; turn < this.maxTurns; turn++) {
        await this.announceInstructions();
        await this.flushSupplements();
        await this.compactIfNeeded();
        // a hook during auto-compaction said continue: false
        if (this.hookStop !== undefined) return await this.hookStopped(this.hookStop);

        const req = await this.request();
        await this.events.emit('before_request', { req, model: this.model });

        let message: Message | undefined;
        for await (const ev of this.streamModel(req)) {
          onStream?.(ev);
          if (ev.type === 'done') message = ev.message;
        }
        if (!message) throw new Error(`${this.model.id}: stream ended without a final message`);

        await this.store.appendMessage(message);
        await this.events.emit('message', { message });

        const usage = message.usage ?? { input: 0, output: 0 };
        const cost = costUsd(this.model, usage);
        this.totalCostUsd += cost;
        await this.events.emit('turn_end', { stopReason: message.stopReason ?? 'stop', usage, costUsd: cost });

        const calls = message.content.filter((b): b is ToolCallBlock => b.type === 'tool_call');
        if (!calls.length) {
          const keepGoing = await this.events.emit('before_stop', { message, continued: continued > 0 });
          if (!(keepGoing && keepGoing.block) || continued >= MAX_STOP_CONTINUES || this.controller.signal.aborted) return message;
          continued++;
          await this.store.appendMessage({
            role: 'user',
            content: [{ type: 'text', text: `[Stop hook] ${keepGoing.reason}` }]
          });
          continue;
        }

        // Sequential on purpose: order is observable and simple to reason about.
        const results: ContentBlock[] = [];
        for (const call of calls) results.push(await this.runTool(call));
        await this.store.appendMessage({ role: 'user', content: results });

        if (this.controller.signal.aborted) {
          return { role: 'assistant', content: [], stopReason: 'aborted' };
        }
        if (this.hookStop !== undefined) return await this.hookStopped(this.hookStop);
      }
      throw new Error(
        `单轮工具循环已达上限 ${this.maxTurns} 次（防失控保险丝）。进度已保存：发一句"继续"可接着做；` +
          `长任务可在 ~/.tango/config.json 把 maxTurns 调大（1-500，默认 64），一次性运行也可用 --max-tool-rounds N`
      );
    } catch (e: any) {
      if (this.controller.signal.aborted || e?.name === 'AbortError') {
        const msg: Message = { role: 'assistant', content: [], stopReason: 'aborted' };
        await this.store.appendMessage(msg);
        return msg;
      }
      await this.events.emit('error', { error: e instanceof Error ? e : new Error(String(e)) });
      throw e;
    } finally {
      await this.events.emit('prompt_end', {});
      this.controller = undefined;
    }
  }

  /** instructions_loaded when the AGENTS.md set is first read and whenever it changes (/cd). */
  private async announceInstructions(): Promise<void> {
    const files = findAgentsFiles(this.cwd);
    const key = files.join('\n');
    if (key === this.instructionsKey) return;
    this.instructionsKey = key;
    await this.events.emit('instructions_loaded', { files });
  }

  /** End the turn for a hook's continue: false; the reason is the visible last word. */
  private async hookStopped(reason: string): Promise<Message> {
    const msg: Message = { role: 'assistant', content: [{ type: 'text', text: `[hook] ${reason}` }], stopReason: 'stop' };
    await this.store.appendMessage(msg);
    await this.events.emit('message', { message: msg });
    return msg;
  }

  /** Ask a side question with the current context, without tools or session writes. */
  async ask(text: string, onStream?: (ev: StreamEvent) => void): Promise<Message> {
    const user: Message = { role: 'user', content: [{ type: 'text', text }] };
    this.controller = new AbortController();
    try {
      const { systemPrompt } = await buildContext(this.cwd);
      const req: CompletionRequest = {
        system: systemPrompt,
        messages: [...this.store.messages(), user],
        tools: [],
        maxTokens: this.model.maxTokens,
        signal: this.controller.signal
      };
      let message: Message | undefined;
      for await (const ev of this.streamModel(req)) {
        onStream?.(ev);
        if (ev.type === 'done') message = ev.message;
      }
      if (!message) throw new Error(`${this.model.id}: stream ended without a final message`);
      const usage = message.usage ?? { input: 0, output: 0 };
      const cost = costUsd(this.model, usage);
      this.totalCostUsd += cost;
      await this.events.emit('turn_end', {
        stopReason: message.stopReason ?? 'stop',
        usage,
        costUsd: cost
      });
      return message;
    } catch (e: any) {
      if (this.controller.signal.aborted || e?.name === 'AbortError') {
        return { role: 'assistant', content: [], stopReason: 'aborted' };
      }
      await this.events.emit('error', { error: e instanceof Error ? e : new Error(String(e)) });
      throw e;
    } finally {
      this.controller = undefined;
    }
  }

  /**
   * Summarize older context and keep the latest ~COMPACT_KEEP_TOKENS verbatim.
   * A previous summary is updated incrementally instead of re-summarized from scratch.
   * The kept tail starts at a user message, so no tool_result is cut from its tool_call.
   */
  async compact(trigger: 'manual' | 'auto' = 'manual'): Promise<boolean> {
    const messages = this.store.messages();
    if (messages.length < 4) return false;
    const veto = await this.events.emit('before_compact', { trigger });
    if (veto && veto.block) return false;
    const hasPrev = messages[0]?.role === 'user' && textOf(messages[0]).startsWith(SUMMARY_OPEN);
    const body = hasPrev ? messages.slice(1) : messages;
    // small windows keep less, or the kept tail alone would re-trigger auto-compaction
    const budget = Math.min(COMPACT_KEEP_TOKENS, Math.round((this.model.contextWindow ?? 128_000) * 0.25));
    let keep = keptTail(body, budget);
    let older = body.slice(0, body.length - keep);
    if (older.length === 0) {
      // everything fits the tail budget (a manual /compact): keep just the latest turn
      const last = lastTurnStart(body);
      if (last > 0) {
        older = body.slice(0, last);
        keep = body.length - last;
      }
    }
    if (keep === 0 || older.length === 0) {
      // one huge turn: summarize the first half of the last turn instead
      const half = splitPoint(body);
      if (half > 0) {
        older = body.slice(0, half);
        keep = body.length - half;
      } else {
        // no safe split point: summarize everything, as before
        older = body;
        keep = 0;
      }
    }
    const prev = hasPrev ? this.store.lastSummary() : undefined;
    const transcript = this.transcript(older, COMPACT_TOOL_CHARS);
    const text = prev
      ? `<previous summary>\n${prev}\n</previous summary>\n\n${transcript}\n\n${COMPACT_UPDATE_INSTRUCTION}`
      : `${transcript}\n\n${COMPACT_INSTRUCTION}`;
    let summary = '';
    const req: CompletionRequest = {
      system: 'You summarize conversations for other models.',
      messages: [{ role: 'user', content: [{ type: 'text', text }] }],
      tools: [],
      maxTokens: 4000,
      signal: this.controller?.signal
    };
    for await (const ev of this.streamModel(req)) {
      if (ev.type === 'done') {
        summary = ev.message.content
          .filter((b) => b.type === 'text')
          .map((b) => (b as { text: string }).text)
          .join('');
      }
    }
    if (!summary.trim()) return false;
    await this.store.compact(summary.trim(), keep);
    await this.events.emit('after_compact', { trigger, summary: summary.trim() });
    return true;
  }

  private async request(): Promise<CompletionRequest> {
    const { systemPrompt } = await buildContext(this.cwd);
    return {
      system: this.systemSuffix ? systemPrompt + this.systemSuffix : systemPrompt,
      messages: this.store.messages(),
      tools: this.tools.schemas(),
      maxTokens: this.model.maxTokens,
      signal: this.controller?.signal,
      effort: this.effort
    };
  }

  private async runTool(call: ToolCallBlock): Promise<ToolResultBlock> {
    const fail = (text: string): ToolResultBlock => ({
      type: 'tool_result',
      toolCallId: call.id,
      content: [{ type: 'text', text }],
      isError: true
    });

    const tool = this.tools.get(call.name);
    if (!tool) return fail(`unknown tool: ${call.name}`);
    if (!call.input || typeof call.input !== 'object' || Array.isArray(call.input)) {
      const raw = typeof call.input === 'string' ? call.input.slice(0, 300) : JSON.stringify(call.input);
      return fail(`invalid arguments for ${call.name}: expected a JSON object, got ${raw}`);
    }

    const veto = await this.events.emit('before_tool', { call, tool });
    if (veto && veto.block) {
      // announce blocked calls too, so UIs can show why nothing ran
      const result = { content: [{ type: 'text' as const, text: `blocked: ${veto.reason}` }], isError: true };
      await this.events.emit('after_tool', { call, result, ms: 0 });
      return { type: 'tool_result', toolCallId: call.id, content: result.content, isError: true };
    }

    const t0 = Date.now();
    let result: ToolResult;
    try {
      result = await tool.execute(call.input, {
        cwd: this.cwd,
        sessionId: this.store.id,
        signal: this.controller?.signal ?? new AbortController().signal
      });
    } catch (e: any) {
      if (this.controller?.signal.aborted) return fail('aborted');
      result = { content: [{ type: 'text', text: `tool failed: ${e?.message ?? e}` }], isError: true };
    }
    await this.events.emit('after_tool', { call, result, ms: Date.now() - t0 });
    return { type: 'tool_result', toolCallId: call.id, content: result.content, isError: result.isError };
  }

  /** Cheap char-based token estimate of what the next request would carry. */
  private estimateTokens(): number {
    return this.store.messages().reduce((n, m) => n + messageTokens(m), 0);
  }

  private async compactIfNeeded(): Promise<void> {
    if (this.compactAt <= 0) return;
    const window = this.model.contextWindow ?? 128_000;
    if (this.estimateTokens() >= window * this.compactAt) await this.compact('auto');
  }

  /**
   * Tool-free brief of `messages` for another session that continues the work
   * (/handoff: this session's context; /takeover: the other session's).
   * `note` is the user's steer (e.g. what to do next). '' when there is nothing to hand over.
   */
  async handoffBrief(messages: Message[], note?: string): Promise<string> {
    if (messages.length === 0) return '';
    const steer = note?.trim() ? `\n\nThe user's note for the next session: ${note.trim()}` : '';
    const req: CompletionRequest = {
      system: 'You write handoff briefs so another model session can continue unfinished work.',
      messages: [{ role: 'user', content: [{ type: 'text', text: this.transcript(messages) + '\n\n' + HANDOFF_INSTRUCTION + steer }] }],
      tools: [],
      maxTokens: 2000,
      signal: this.controller?.signal
    };
    let brief = '';
    for await (const ev of this.streamModel(req)) {
      if (ev.type === 'done')
        brief = ev.message.content
          .filter((b) => b.type === 'text')
          .map((b) => (b as { text: string }).text)
          .join('');
    }
    return brief.trim();
  }

  /** Flatten the conversation to plain text for the summarizer call. */
  private transcript(messages: Message[], toolChars = 600): string {
    const lines: string[] = [];
    for (const m of messages) {
      for (const b of m.content) {
        if (b.type === 'text') lines.push(`${m.role}: ${b.text}`);
        else if (b.type === 'tool_call')
          lines.push(`assistant called ${b.name}: ${JSON.stringify(b.input ?? {}).slice(0, 400)}`);
        else if (b.type === 'tool_result') {
          const body = b.content
            .map((c) => (c.type === 'text' ? c.text : '[image]'))
            .join('\n')
            .slice(0, toolChars);
          lines.push(`tool result${b.isError ? ' (error)' : ''}: ${body}`);
        }
      }
    }
    // the summarizer request must fit the window: keep the newest part when it would not
    const cap = Math.round((this.model.contextWindow ?? 128_000) * 0.6 * 3.5);
    return lines.join('\n').slice(-Math.max(60_000, cap));
  }
}

/** Cheap char-based token estimate of one message (≈3.5 chars per token, images 4000 chars). */
export function messageTokens(m: Message): number {
  let chars = 0;
  for (const b of m.content) {
    if (b.type === 'text' || b.type === 'thinking') chars += b.text.length;
    else if (b.type === 'tool_call') chars += JSON.stringify(b.input ?? {}).length + b.name.length;
    else if (b.type === 'tool_result')
      chars += b.content.reduce((n, c) => n + (c.type === 'text' ? c.text.length : 4000), 0);
    else if (b.type === 'image') chars += 4000;
  }
  return Math.round(chars / 3.5);
}

function textOf(m: Message): string {
  return m.content.map((b) => (b.type === 'text' ? b.text : '')).join('');
}

/** A user message that is a real prompt (not only tool results): a safe place to cut. */
function isTurnStart(m: Message): boolean {
  return m.role === 'user' && !m.content.some((b) => b.type === 'tool_result');
}

/**
 * How many trailing messages to keep verbatim: the newest whole turns that fit in
 * `budget` tokens. The cut always lands on a turn start, so tool calls keep their results.
 * 0 when even the last turn alone is over budget.
 */
export function keptTail(messages: Message[], budget: number): number {
  let used = 0;
  let keep = 0;
  for (let i = messages.length - 1; i >= 0; i--) {
    used += messageTokens(messages[i]);
    if (used > budget) break;
    if (isTurnStart(messages[i])) keep = messages.length - i;
  }
  return keep;
}

/**
 * For a last turn too big to keep: index splitting it in half (by tokens) at an
 * assistant message boundary, so [0, i) is summarized and [i, end) stays. The kept
 * part then opens with an assistant message whose tool calls are all answered after it.
 */
function lastTurnStart(messages: Message[]): number {
  for (let i = messages.length - 1; i >= 0; i--) if (isTurnStart(messages[i])) return i;
  return 0;
}

export function splitPoint(messages: Message[]): number {
  const start = lastTurnStart(messages);
  const total = messages.slice(start).reduce((n, m) => n + messageTokens(m), 0);
  let used = 0;
  for (let i = start; i < messages.length; i++) {
    if (i > start && messages[i].role === 'assistant' && used >= total / 2) return i;
    used += messageTokens(messages[i]);
  }
  return -1;
}
