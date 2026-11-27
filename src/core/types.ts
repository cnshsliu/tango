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
  /**
   * Reasoning effort levels this model's endpoint accepts, e.g. ["low", "high", "max"] (models.dev
   * names, see reasoning.ts); [] when there are none. Unset: taken from the preset table, or from
   * the model's name.
   */
  efforts?: string[];
  /** accepts image input (vision). Unset = unknown: images are still sent, with a heads-up notice */
  vision?: boolean;
  /** per-model override of config `retries` (failed requests retried before giving up) */
  retries?: number;
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
   * Defaults to `provider` when unset. Display grouping uses this; zero-cost models with no
   * platform are additionally collected into the virtual "free" platform (see groupModels).
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
  /** a reasoning effort level the model accepts (`effortsFor`), e.g. "high", "max" (/effort) */
  effort?: string;
  /** retries for a request that fails before any output (network error, 429, 5xx); default 10 */
  retries?: number;
  /** told before each retry, so the user sees "retrying n/max" instead of silence */
  onRetry?: (info: RetryInfo) => void;
}

export interface RetryInfo {
  /** 1-based retry number */
  attempt: number;
  max: number;
  /** why the previous try failed, e.g. "connection reset [ECONNRESET]" or "HTTP 503" */
  reason: string;
  delayMs: number;
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
  /** id of the session making the call (session_search skips it) */
  sessionId?: string;
  /** id of this tool call (task: its sub-agent's progress names the call it belongs to) */
  callId?: string;
}

export interface ToolResult {
  content: (TextBlock | ImageBlock)[];
  isError?: boolean;
}

export interface Tool<TInput = unknown> extends ToolSchema {
  execute(input: TInput, ctx: ToolContext): Promise<ToolResult>;
  /** true when a call can change the workspace; checkpoints and plan mode key off this.
   * A function decides per call (task: explore reads, general writes). */
  writeEffect?: boolean | ((input: TInput) => boolean);
}

// ---------- Session ----------

export type SessionEntry =
  | { kind: 'meta'; id: string; createdAt: string; cwd: string; name?: string; branch?: string; forkOf?: { sessionId: string; entryIndex: number }; parent?: string }
  | { kind: 'message'; ts: string; message: Message }
  | { kind: 'model'; ts: string; modelId: string }
  | { kind: 'task_run'; ts: string; ok: boolean }
  /** keep: how many of the latest context messages before this entry stay verbatim after it
   * (absent on pre-v2 entries = 0: everything was summarized) */
  | { kind: 'compaction'; ts: string; summary: string; droppedCount: number; keep?: number }
  /** a brief of session `from` merged into this one (/handoff from there, or /takeover from here) */
  | { kind: 'handoff'; ts: string; from: string; summary: string; via: 'handoff' | 'takeover' }
  /** a short message from another session's user (/send): shown in the chat, in the model's context from the next request on */
  | { kind: 'mail'; ts: string; from: string; text: string }
  | { kind: 'checkpoint'; ts: string; hash: string; note?: string }
  | { kind: 'rewind'; ts: string; to: number };

// ---------- Events (the extension point) ----------

/** Does this call change the workspace? (writeEffect may depend on the input) */
export function writesWorkspace(tool: Tool<any>, input: unknown): boolean {
  return typeof tool.writeEffect === 'function' ? !!tool.writeEffect(input) : !!tool.writeEffect;
}

export interface TangoEvents {
  session_start: { sessionId: string; cwd: string };
  /** a prompt turn began; re-arms per-turn machinery like checkpointing */
  prompt_start: { text: string };
  /** before the user's text is recorded; a veto rejects the prompt (UserPromptSubmit hooks) */
  before_prompt: { text: string };
  /** the model is about to finish; a veto keeps it going with reason as the next user note (Stop hooks) */
  before_stop: { message: Message; continued: boolean };
  /** before a compaction (manual = /compact, auto = context threshold); a veto skips it (PreCompact hooks) */
  before_compact: { trigger: 'manual' | 'auto' };
  /** a compaction finished; summary is what replaced the older messages (PostCompact hooks) */
  after_compact: { trigger: 'manual' | 'auto'; summary: string };
  /** a session that ran at least one prompt is closing (SessionEnd hooks) */
  session_end: { reason: string };
  /** the AGENTS.md files feeding the system prompt, on first load and whenever the set changes */
  instructions_loaded: { files: string[] };
  /** the working directory moved (/cd) */
  cwd_changed: { oldCwd: string; newCwd: string };
  /** tango wants the user's attention (a permission prompt) */
  notification: { message: string; type: string };
  /** a /subagent is about to start (a veto refuses it) or has finished */
  subagent_start: { agentType: string; description: string; taskId?: string };
  subagent_stop: { agentType: string; description: string; taskId: string; success: boolean; status: string };
  /** a /bg task is about to start (a veto refuses it) or has finished */
  task_created: { agentType: string; description: string };
  task_completed: { agentType: string; description: string; taskId: string; success: boolean; status: string };
  /** the prompt turn ended (stop, abort or error); per-turn machinery closes out */
  prompt_end: Record<string, never>;
  /** may mutate req before it is sent */
  before_request: { req: CompletionRequest; model: ModelSpec };
  message: { message: Message };
  /** return { block: true, reason } to veto a tool call */
  before_tool: { call: ToolCallBlock; tool: Tool };
  after_tool: { call: ToolCallBlock; result: ToolResult; ms: number };
  turn_end: { stopReason: StopReason; usage: Usage; costUsd: number };
  /** a `task` sub-agent finished (foreground or background); a background report is already delivered */
  /** a `task` sub-agent is about to run one of its tools (callId: the parent's task call) */
  task_progress: { callId?: string; agent: string; step: number; tool: string; detail: string };
  task_done: { id: string; agent: string; description: string; ok: boolean; sessionId: string; costUsd: number; background: boolean };
  /** a model request failed before any output and is about to be retried */
  retry: RetryInfo;
  error: { error: Error };
}

export type EventName = keyof TangoEvents;
export type Veto = { block: true; reason: string } | void;
export type Handler<K extends EventName> = (
  payload: TangoEvents[K]
) => Promise<Veto> | Veto;
