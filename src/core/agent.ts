import { buildContext } from './context.js';
import { EventBus } from './events.js';
import { adapterFor, costUsd } from './models/registry.js';
import type { SessionStore } from './session.js';
import type { ToolRegistry } from './tools/registry.js';
import type {
  CompletionRequest,
  ContentBlock,
  Message,
  ModelSpec,
  StreamEvent,
  ToolCallBlock,
  ToolResult,
  ToolResultBlock
} from './types.js';

export interface AgentOptions {
  store: SessionStore;
  tools: ToolRegistry;
  model: ModelSpec;
  cwd: string;
  events?: EventBus;
  maxTurns?: number;
  /** compact when the estimated context passes this share of the window (0 disables) */
  compactAt?: number;
}

const COMPACT_INSTRUCTION =
  'Summarize everything above for a successor model that will NOT see the original messages. ' +
  'Keep: the user goals and constraints, decisions made and why, file paths touched, current state, ' +
  'open questions. Be dense and factual. This is data for another model, not a reply to a human.';

/**
 * The agent loop: append user message, then model → tools → model until the
 * model stops without tool calls. Every step is persisted and evented.
 */
export class Agent {
  readonly events: EventBus;
  model: ModelSpec;
  /** USD accumulated by prompt() calls in this process. */
  totalCostUsd = 0;

  private readonly store: SessionStore;
  private readonly tools: ToolRegistry;
  private readonly cwd: string;
  private readonly maxTurns: number;
  private readonly compactAt: number;
  private controller?: AbortController;

  constructor(opts: AgentOptions) {
    this.store = opts.store;
    this.tools = opts.tools;
    this.cwd = opts.cwd;
    this.model = opts.model;
    this.events = opts.events ?? new EventBus();
    this.maxTurns = opts.maxTurns ?? 64;
    this.compactAt = opts.compactAt ?? 0.75;
  }

  abort(): void {
    this.controller?.abort();
  }

  setModel(model: ModelSpec): void {
    this.model = model;
    void this.store.setModel(model.id);
  }

  /** Append a user message and loop (model → tools → model) until the model stops. */
  async prompt(text: string, onStream?: (ev: StreamEvent) => void): Promise<Message> {
    const user: Message = { role: 'user', content: [{ type: 'text', text }] };
    await this.store.appendMessage(user);

    this.controller = new AbortController();
    try {
      for (let turn = 0; turn < this.maxTurns; turn++) {
        await this.compactIfNeeded();

        const req = await this.request();
        await this.events.emit('before_request', { req, model: this.model });

        let message: Message | undefined;
        for await (const ev of adapterFor(this.model).stream(this.model, req)) {
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
        if (!calls.length) return message;

        // Sequential on purpose: order is observable and simple to reason about.
        const results: ContentBlock[] = [];
        for (const call of calls) results.push(await this.runTool(call));
        await this.store.appendMessage({ role: 'user', content: results });

        if (this.controller.signal.aborted) {
          return { role: 'assistant', content: [], stopReason: 'aborted' };
        }
      }
      throw new Error(`maxTurns (${this.maxTurns}) exceeded`);
    } catch (e: any) {
      if (this.controller.signal.aborted || e?.name === 'AbortError') {
        const msg: Message = { role: 'assistant', content: [], stopReason: 'aborted' };
        await this.store.appendMessage(msg);
        return msg;
      }
      await this.events.emit('error', { error: e instanceof Error ? e : new Error(String(e)) });
      throw e;
    } finally {
      this.controller = undefined;
    }
  }

  /** Ask the model to summarize the conversation, then drop everything before the summary. */
  async compact(): Promise<boolean> {
    const messages = this.store.messages();
    if (messages.length < 4) return false;

    const transcript = this.transcript(messages);
    let summary = '';
    const req: CompletionRequest = {
      system: 'You summarize conversations for other models.',
      messages: [
        {
          role: 'user',
          content: [{ type: 'text', text: transcript + '\n\n' + COMPACT_INSTRUCTION }]
        }
      ],
      tools: [],
      maxTokens: 1200,
      signal: this.controller?.signal
    };
    for await (const ev of adapterFor(this.model).stream(this.model, req)) {
      if (ev.type === 'done') {
        summary = ev.message.content
          .filter((b) => b.type === 'text')
          .map((b) => (b as { text: string }).text)
          .join('');
      }
    }
    if (!summary.trim()) return false;
    await this.store.compact(summary.trim());
    return true;
  }

  private async request(): Promise<CompletionRequest> {
    const { systemPrompt } = await buildContext(this.cwd);
    return {
      system: systemPrompt,
      messages: this.store.messages(),
      tools: this.tools.schemas(),
      maxTokens: this.model.maxTokens,
      signal: this.controller?.signal
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

    const veto = await this.events.emit('before_tool', { call, tool });
    if (veto && veto.block) return fail(`blocked: ${veto.reason}`);

    const t0 = Date.now();
    let result: ToolResult;
    try {
      result = await tool.execute(call.input, {
        cwd: this.cwd,
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
    let chars = 0;
    for (const m of this.store.messages()) {
      for (const b of m.content) {
        if (b.type === 'text' || b.type === 'thinking') chars += b.text.length;
        else if (b.type === 'tool_call') chars += JSON.stringify(b.input ?? {}).length + b.name.length;
        else if (b.type === 'tool_result')
          chars += b.content.reduce((n, c) => n + (c.type === 'text' ? c.text.length : 4000), 0);
        else if (b.type === 'image') chars += 4000;
      }
    }
    return Math.round(chars / 3.5);
  }

  private async compactIfNeeded(): Promise<void> {
    if (this.compactAt <= 0) return;
    const window = this.model.contextWindow ?? 128_000;
    if (this.estimateTokens() >= window * this.compactAt) await this.compact();
  }

  /** Flatten the conversation to plain text for the summarizer call. */
  private transcript(messages: Message[]): string {
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
            .slice(0, 600);
          lines.push(`tool result${b.isError ? ' (error)' : ''}: ${body}`);
        }
      }
    }
    return lines.join('\n').slice(-60_000);
  }
}
