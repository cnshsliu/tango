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

function toAnthropicMessages(messages: Message[]) {
  return messages.map((m) => ({
    role: m.role,
    content: m.content
      .filter((b) => b.type !== 'thinking' || b.signature) // drop unsigned thinking on replay
      .map((b) => {
        switch (b.type) {
          case 'text':
            return { type: 'text', text: b.text };
          case 'image':
            return {
              type: 'image',
              source: { type: 'base64', media_type: b.mimeType, data: b.data }
            };
          case 'tool_call':
            return { type: 'tool_use', id: b.id, name: b.name, input: argsObject(b.input) };
          case 'tool_result':
            return {
              type: 'tool_result',
              tool_use_id: b.toolCallId,
              is_error: b.isError ?? false,
              content: b.content.map((c) =>
                c.type === 'text'
                  ? { type: 'text', text: c.text }
                  : { type: 'image', source: { type: 'base64', media_type: c.mimeType, data: c.data } }
              )
            };
          case 'thinking':
            return { type: 'thinking', thinking: b.text, signature: b.signature };
        }
      })
  }));
}

export const anthropicAdapter: ModelAdapter = {
  async *stream(spec: ModelSpec, req: CompletionRequest): AsyncIterable<StreamEvent> {
    const body: Record<string, unknown> = {
      model: spec.model,
      max_tokens: req.maxTokens ?? spec.maxTokens ?? 8192,
      // cache the stable system prompt (anthropic prompt caching) — token plans
    // bill cache reads far cheaper than fresh input
    system: [{ type: 'text', text: req.system, cache_control: { type: 'ephemeral' } }],
      messages: toAnthropicMessages(req.messages),
      stream: true
    };
    if (req.effort) {
      let budget = req.effort === 'high' ? 16_000 : req.effort === 'medium' ? 8_000 : 4_000;
      const maxT = (body.max_tokens as number) ?? 8192;
      // thinking budget must leave room for the answer and never exceed max_tokens
      budget = Math.min(budget, Math.max(1024, maxT - 1024));
      body.max_tokens = Math.max(maxT, budget + 1024);
      body.thinking = { type: 'enabled', budget_tokens: budget };
    }
    if (req.tools.length) {
      body.tools = req.tools.map((t) => ({
        name: t.name,
        description: t.description,
        input_schema: t.input
      }));
    }

    const res = await postJson(
      spec,
      `${spec.baseUrl.replace(/\/$/, '')}/messages`,
      { 'x-api-key': resolveApiKey(spec), 'anthropic-version': '2023-06-01' },
      body,
      req.signal,
      { retries: req.retries, onRetry: req.onRetry }
    );

    const blocks: ContentBlock[] = [];
    const partialJson = new Map<number, string>();
    const usage: Usage = { input: 0, output: 0 };
    let stopReason: Message['stopReason'] = 'stop';
    let sawEvent = false;

    for await (const data of sseLines(res.body!)) {
      sawEvent = true;
      const ev = parseEvent(spec, data);
      switch (ev.type) {
        case 'message_start': {
          // input_tokens excludes cached tokens; Usage.input is the whole prompt
          const u = ev.message?.usage ?? {};
          usage.cacheRead = u.cache_read_input_tokens;
          usage.cacheWrite = u.cache_creation_input_tokens;
          usage.input = (u.input_tokens ?? 0) + (usage.cacheRead ?? 0) + (usage.cacheWrite ?? 0);
          break;
        }
        case 'content_block_start': {
          const cb = ev.content_block;
          if (cb.type === 'text') blocks[ev.index] = { type: 'text', text: '' };
          else if (cb.type === 'thinking') blocks[ev.index] = { type: 'thinking', text: '' };
          else if (cb.type === 'tool_use') {
            blocks[ev.index] = { type: 'tool_call', id: cb.id, name: cb.name, input: {} };
            partialJson.set(ev.index, '');
            yield { type: 'tool_call_start', id: cb.id, name: cb.name };
          }
          break;
        }
        case 'content_block_delta': {
          const d = ev.delta;
          const b = blocks[ev.index];
          if (d.type === 'text_delta' && b?.type === 'text') {
            b.text += d.text;
            yield { type: 'text', delta: d.text };
          } else if (d.type === 'thinking_delta' && b?.type === 'thinking') {
            b.text += d.thinking;
            yield { type: 'thinking', delta: d.thinking };
          } else if (d.type === 'signature_delta' && b?.type === 'thinking') {
            b.signature = d.signature;
          } else if (d.type === 'input_json_delta' && b?.type === 'tool_call') {
            partialJson.set(ev.index, (partialJson.get(ev.index) ?? '') + d.partial_json);
            yield { type: 'tool_call_delta', id: b.id, delta: d.partial_json };
          }
          break;
        }
        case 'content_block_stop': {
          const b = blocks[ev.index];
          if (b?.type === 'tool_call') {
            const raw = partialJson.get(ev.index) ?? '';
            b.input = parseArgs(raw);
          }
          break;
        }
        case 'message_delta':
          usage.output = ev.usage?.output_tokens ?? usage.output;
          stopReason =
            ev.delta?.stop_reason === 'tool_use'
              ? 'tool_use'
              : ev.delta?.stop_reason === 'max_tokens'
                ? 'length'
                : 'stop';
          break;
        case 'error':
          throw new Error(`${spec.id}: ${JSON.stringify(ev.error)}`);
      }
    }
    if (!sawEvent) {
      // HTTP 200 with a non-SSE body (a gateway 404 wrapper, an HTML error page)
      // must surface, not end the stream as an empty answer — check the baseUrl
      // (anthropic-compatible gateways usually end in /v1) and the model name
      throw new Error(
        `${spec.id}: endpoint returned no SSE events (HTTP ${res.status}) — likely a wrong baseUrl or model name for this gateway`
      );
    }

    yield {
      type: 'done',
      message: {
        role: 'assistant',
        content: blocks.filter(Boolean),
        usage,
        model: spec.id,
        stopReason
      }
    };
  }
};
