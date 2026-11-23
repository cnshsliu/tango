import type {
  CompletionRequest,
  ContentBlock,
  Message,
  ModelAdapter,
  ModelSpec,
  StreamEvent,
  Usage
} from '../types.js';
import { argsObject, parseArgs, parseEvent, postJson, resolveApiKey, sseLines } from './util.js';

/**
 * Map a chat-completions usage object onto Usage. prompt_tokens already includes cached and
 * cache-write tokens. Gemini leaves thinking out of completion_tokens but bills it as output,
 * so output falls back to total_tokens - prompt_tokens when that is larger.
 */
export function parseUsage(u: any): Usage {
  const input = u.prompt_tokens ?? 0;
  const d = u.prompt_tokens_details ?? {};
  const output = Math.max(u.completion_tokens ?? 0, (u.total_tokens ?? 0) - input);
  return {
    input,
    output,
    cacheRead: d.cached_tokens ?? u.prompt_cache_hit_tokens ?? u.prompt_tokens_details?.cached_tokens,
    cacheWrite: d.cache_write_tokens ?? d.cache_creation_input_tokens
  };
}

/** Flatten our message model into OpenAI chat-completions shape. */
export function toOpenAIMessages(system: string, messages: Message[], systemRole: 'system' | 'developer' = 'system') {
  // the system prompt always rides the cacheable message prefix; OpenAI-native routes
  // use the `developer` role newer models (o-series, GPT-5/6) are documented to expect
  // (opencode 2.3.2 parity — Responses `instructions` is billed uncached on GPT-6.2,
  // so nobody should carry a system prompt outside the message prefix)
  const out: any[] = [{ role: systemRole, content: system }];
  for (const m of messages) {
    if (m.role === 'user') {
      // tool results become separate "tool" messages; other content becomes one user message
      const results = m.content.filter((b) => b.type === 'tool_result');
      for (const r of results) {
        if (r.type !== 'tool_result') continue;
        out.push({
          role: 'tool',
          tool_call_id: r.toolCallId,
          content: r.content.map((c) => (c.type === 'text' ? c.text : '[image]')).join('\n')
        });
      }
      const rest = m.content.filter((b) => b.type !== 'tool_result');
      if (rest.length) {
        out.push({
          role: 'user',
          content: rest.map((b) =>
            b.type === 'text'
              ? { type: 'text', text: b.text }
              : b.type === 'image'
                ? { type: 'image_url', image_url: { url: `data:${b.mimeType};base64,${b.data}` } }
                : { type: 'text', text: '' }
          )
        });
      }
    } else {
      const text = m.content
        .filter((b) => b.type === 'text')
        .map((b) => (b as any).text)
        .join('');
      const thinking = m.content
        .filter((b) => b.type === 'thinking')
        .map((b) => (b as any).text)
        .join('');
      const calls = m.content.filter((b) => b.type === 'tool_call');
      // content is omitted (not null) when empty: some gateways, e.g. tknet, reject null.
      const msg: any = { role: 'assistant' };
      if (text || !calls.length) msg.content = text;
      // DeepSeek and Kimi thinking models return 400 on tool requests unless reasoning is sent back.
      if (thinking) msg.reasoning_content = thinking;
      if (calls.length) {
        msg.tool_calls = calls.map((c) => {
          if (c.type !== 'tool_call') return null;
          return {
            id: c.id,
            type: 'function',
            function: { name: c.name, arguments: JSON.stringify(argsObject(c.input)) },
            ...(c.extra ? { extra_content: c.extra } : {})
          };
        });
      }
      out.push(msg);
    }
  }
  return out;
}

export const openaiAdapter: ModelAdapter = {
  async *stream(spec: ModelSpec, req: CompletionRequest): AsyncIterable<StreamEvent> {
    const body: Record<string, unknown> = {
      model: spec.model,
      messages: toOpenAIMessages(
        req.system,
        req.messages,
        /api\.openai\.com/.test(spec.baseUrl) ? 'developer' : 'system'
      ),
      stream: true,
      stream_options: { include_usage: true }
    };
    // OpenAI's reasoning models reject max_tokens; other compatible APIs don't all accept the new name.
    const maxKey = /api\.openai\.com/.test(spec.baseUrl) ? 'max_completion_tokens' : 'max_tokens';
    body[maxKey] = req.maxTokens ?? spec.maxTokens ?? 8192;
    // only reasoning-capable endpoints take the param; others may 400 on it
    if (req.effort && spec.reasoning !== false) body.reasoning_effort = req.effort;
    if (typeof spec.thinkingBudget === 'number') body.thinking_budget = spec.thinkingBudget;
    if (req.tools.length) {
      body.tools = req.tools.map((t) => ({
        type: 'function',
        function: { name: t.name, description: t.description, parameters: t.input }
      }));
    }

    const res = await postJson(
      spec,
      `${spec.baseUrl.replace(/\/$/, '')}/chat/completions`,
      { authorization: `Bearer ${resolveApiKey(spec)}` },
      body,
      req.signal,
      { retries: req.retries, onRetry: req.onRetry }
    );

    let text = '';
    let thinking = '';
    const calls = new Map<number, { id: string; name: string; args: string; extra?: unknown }>();
    const usage: Usage = { input: 0, output: 0 };
    let finish: string | undefined;

    for await (const data of sseLines(res.body!)) {
      if (data === '[DONE]') break;
      const ev = parseEvent(spec, data);
      if (ev.error) throw new Error(`${spec.id}: ${JSON.stringify(ev.error)}`);
      if (ev.usage) Object.assign(usage, parseUsage(ev.usage));
      const choice = ev.choices?.[0];
      if (!choice) continue;
      const d = choice.delta ?? {};
      const rfield = spec.reasoningField ?? 'reasoning_content';
      const rdelta = (d as Record<string, unknown>)[rfield];
      if (typeof rdelta === 'string' && rdelta) {
        thinking += rdelta;
        yield { type: 'thinking', delta: rdelta };
      }
      if (d.content) {
        text += d.content;
        yield { type: 'text', delta: d.content };
      }
      for (const tc of d.tool_calls ?? []) {
        const idx = tc.index ?? 0;
        let c = calls.get(idx);
        if (!c) {
          c = { id: tc.id ?? `call_${idx}`, name: tc.function?.name ?? '', args: '' };
          calls.set(idx, c);
          yield { type: 'tool_call_start', id: c.id, name: c.name };
        }
        if (tc.id) c.id = tc.id;
        if (tc.function?.name) c.name = tc.function.name;
        if (tc.extra_content) c.extra = tc.extra_content;
        if (tc.function?.arguments) {
          c.args += tc.function.arguments;
          yield { type: 'tool_call_delta', id: c.id, delta: tc.function.arguments };
        }
      }
      if (choice.finish_reason) finish = choice.finish_reason;
    }

    const content: ContentBlock[] = [];
    if (thinking) content.push({ type: 'thinking', text: thinking });
    if (text) content.push({ type: 'text', text });
    for (const c of calls.values()) {
      content.push({
        type: 'tool_call',
        id: c.id,
        name: c.name,
        input: parseArgs(c.args),
        ...(c.extra ? { extra: c.extra } : {})
      });
    }
    const stopReason: Message['stopReason'] =
      calls.size > 0 ? 'tool_use' : finish === 'length' ? 'length' : 'stop';

    yield { type: 'done', message: { role: 'assistant', content, usage, model: spec.id, stopReason } };
  }
};
