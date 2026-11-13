/**
 * Tango core types. Everything in core speaks these; adapters translate to vendor formats.
 * Rule: this file imports nothing.
 */

// ---------- Messages ----------

export type TextBlock = { type: 'text'; text: string };
export type ImageBlock = { type: 'image'; mimeType: string; data: string }; // base64
export type ToolCallBlock = { type: 'tool_call'; id: string; name: string; input: unknown };
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
  input: number;
  output: number;
  cacheRead?: number;
  cacheWrite?: number;
}

// ---------- Models ----------

export type ApiKind = 'anthropic-messages' | 'openai-completions';

export interface ModelSpec {
  /** display + selection id, e.g. "deepseek/deepseek-chat" */
  id: string;
  provider: string;
  /** model name as sent to the API */
  model: string;
  api: ApiKind;
  baseUrl: string;
  /** literal, "$ENV_NAME", or undefined (then provider env var is used) */
  apiKey?: string;
  contextWindow?: number;
  maxTokens?: number;
  reasoning?: boolean;
  /** USD per million tokens */
  cost?: { input: number; output: number; cacheRead?: number; cacheWrite?: number };
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
}

// ---------- Session ----------

export type SessionEntry =
  | { kind: 'meta'; id: string; createdAt: string; cwd: string; name?: string }
  | { kind: 'message'; ts: string; message: Message }
  | { kind: 'model'; ts: string; modelId: string }
  | { kind: 'compaction'; ts: string; summary: string; droppedCount: number };

// ---------- Events (the extension point) ----------

export interface TangoEvents {
  session_start: { sessionId: string; cwd: string };
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
