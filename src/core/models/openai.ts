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

/** Flatten our message model into OpenAI chat-completions shape. */
export function toOpenAIMessages(system: string, messages: Message[]) {
  const out: any[] = [{ role: 'system', content: system }];
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
      messages: toOpenAIMessages(req.system, req.messages),
      stream: true,
      stream_options: { include_usage: true }
    };
    // OpenAI's reasoning models reject max_tokens; other compatible APIs don't all accept the new name.
    const maxKey = /api\.openai\.com/.test(spec.baseUrl) ? 'max_completion_tokens' : 'max_tokens';
    body[maxKey] = req.maxTokens ?? spec.maxTokens ?? 8192;
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
      req.signal
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
      if (ev.usage) {
        usage.input = ev.usage.prompt_tokens ?? 0;
        usage.output = ev.usage.completion_tokens ?? 0;
        usage.cacheRead = ev.usage.prompt_tokens_details?.cached_tokens ?? ev.usage.prompt_cache_hit_tokens;
      }
      const choice = ev.choices?.[0];
      if (!choice) continue;
      const d = choice.delta ?? {};
      if (d.reasoning_content) {
        thinking += d.reasoning_content;
        yield { type: 'thinking', delta: d.reasoning_content };
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
