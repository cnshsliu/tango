/**
 * Tango core types. Everything in core speaks these; adapters translate to vendor formats.
 * Rule: this file imports nothing.
 */

// ---------- Messages ----------

export type TextBlock = { type: 'text'; text: string };
export type ImageBlock = {
  type: 'image';
  mimeType: string;
  data: string; // base64
  /** original local file path, when known — UIs reopen the image from it; never sent to vendors */
  path?: string;
};
export type ToolCallBlock = {
  type: 'tool_call';
  id: string;
  name: string;
  input: unknown;
  /** vendor data that must be echoed back unchanged, e.g. Gemini's thought_signature */
  extra?: unknown;
};
export type ToolResultBlock = {
  type: 'tool_result';
  toolCallId: string;
  content: (TextBlock | ImageBlock)[];
  isError?: boolean;
};
export type ThinkingBlock = { type: 'thinking'; text: string; signature?: string };

export type ContentBlock = TextBlock | ImageBlock | ToolCallBlock | ToolResultBlock | ThinkingBlock;

export type Role = 'user' | 'assistant';

export interface Message {
  role: Role;
  content: ContentBlock[];
  /** filled by adapters on assistant messages */
  usage?: Usage;
  model?: string;
  stopReason?: StopReason;
}

export type StopReason = 'stop' | 'tool_use' | 'length' | 'error' | 'aborted';

export interface Usage {
  /** all prompt tokens, including cacheRead and cacheWrite */
  input: number;
  /** all generated tokens, including reasoning */
  output: number;
  cacheRead?: number;
  cacheWrite?: number;
}

/** USD per million tokens */
export interface Prices {
  input: number;
  output: number;
  cacheRead?: number;
  cacheWrite?: number;
}

// ---------- Models ----------

export type ApiKind = 'anthropic-messages' | 'openai-completions';

export interface ModelSpec {
  /** display + selection id, e.g. "deepseek/deepseek-flash" */
  id: string;
  provider: string;
  /** model name as sent to the API */
  model: string;
  api: ApiKind;
  baseUrl: string;
  /** env var name if UPPER_SNAKE (e.g. "DEEPSEEK_API_KEY"), otherwise the literal key */
  apiKey?: string;
  contextWindow?: number;
  maxTokens?: number;
  /**
   * Sent as `thinking_budget` on OpenAI-compatible calls.
   * `0` turns the server's automatic thinking cap off (maxTokens still bounds the turn).
   */
  thinkingBudget?: number;
  reasoning?: boolean;
  /** accepts image input (vision). Unset = unknown: images are still sent, with a heads-up notice */
  vision?: boolean;
  /** streaming field carrying reasoning deltas; defaults to "reasoning_content" (qwen/dashscope/deepseek style) */
  reasoningField?: string;
  /**
   * Base prices. `tiers` (ascending) apply when the request's prompt tokens (Usage.input)
   * exceed `above`; the whole request is then billed at that tier.
   */
  cost?: Prices & { tiers?: (Prices & { above: number })[] };
  /** where the endpoint is served: mainland China, outside it, or the user's own gateway */
  region?: 'cn' | 'intl' | 'gateway';
  /**
   * Source platform (tokenhub) the model is reached through, e.g. "opencode", "tknet".
   * Defaults to `provider` when unset. Display grouping uses this; zero-cost models are
   * additionally collected into the virtual "free" platform (see groupModels).
   */
  platform?: string;
}

export interface ToolSchema {
  name: string;
  description: string;
  /** JSON schema for input */
  input: Record<string, unknown>;
}

export interface CompletionRequest {
  system: string;
  messages: Message[];
  tools: ToolSchema[];
  maxTokens?: number;
  signal?: AbortSignal;
  /** reasoning effort for supporting adapters (/effort) */
  effort?: 'low' | 'medium' | 'high';
}

/** Streaming events from a model adapter */
export type StreamEvent =
  | { type: 'text'; delta: string }
  | { type: 'thinking'; delta: string }
  | { type: 'tool_call_start'; id: string; name: string }
  | { type: 'tool_call_delta'; id: string; delta: string }
  | { type: 'done'; message: Message };

export interface ModelAdapter {
  stream(spec: ModelSpec, req: CompletionRequest): AsyncIterable<StreamEvent>;
}

// ---------- Tools ----------

export interface ToolContext {
  cwd: string;
  signal: AbortSignal;
}

export interface ToolResult {
  content: (TextBlock | ImageBlock)[];
  isError?: boolean;
}

export interface Tool<TInput = unknown> extends ToolSchema {
  execute(input: TInput, ctx: ToolContext): Promise<ToolResult>;
  /** true when a call can change the workspace; checkpoints key off this */
  writeEffect?: boolean;
}

// ---------- Session ----------

export type SessionEntry =
  | { kind: 'meta'; id: string; createdAt: string; cwd: string; name?: string; branch?: string; forkOf?: { sessionId: string; entryIndex: number } }
  | { kind: 'message'; ts: string; message: Message }
  | { kind: 'model'; ts: string; modelId: string }
  | { kind: 'task_run'; ts: string; ok: boolean }
  | { kind: 'compaction'; ts: string; summary: string; droppedCount: number }
  /** a brief of session `from` merged into this one (/handoff from there, or /takeover from here) */
  | { kind: 'handoff'; ts: string; from: string; summary: string; via: 'handoff' | 'takeover' }
  | { kind: 'checkpoint'; ts: string; hash: string; note?: string }
  | { kind: 'rewind'; ts: string; to: number };

// ---------- Events (the extension point) ----------

export interface TangoEvents {
  session_start: { sessionId: string; cwd: string };
  /** a prompt turn began; re-arms per-turn machinery like checkpointing */
  prompt_start: { text: string };
  /** the prompt turn ended (stop, abort or error); per-turn machinery closes out */
  prompt_end: Record<string, never>;
  /** may mutate req before it is sent */
  before_request: { req: CompletionRequest; model: ModelSpec };
  message: { message: Message };
  /** return { block: true, reason } to veto a tool call */
  before_tool: { call: ToolCallBlock; tool: Tool };
  after_tool: { call: ToolCallBlock; result: ToolResult; ms: number };
  turn_end: { stopReason: StopReason; usage: Usage; costUsd: number };
  error: { error: Error };
}

export type EventName = keyof TangoEvents;
export type Veto = { block: true; reason: string } | void;
export type Handler<K extends EventName> = (
  payload: TangoEvents[K]
) => Promise<Veto> | Veto;
