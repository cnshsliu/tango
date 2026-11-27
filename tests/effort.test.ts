import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { anthropicEffort, claudeInfo, effortsFor, openaiEffort } from '../src/core/models/reasoning.js';
import { adapterFor, presets } from '../src/core/models/registry.js';

const spec = (model: string, extra: object = {}) => ({ id: `x/${model}`, provider: 'x', model, api: 'openai-completions' as const, baseUrl: 'https://x.example/v1', apiKey: 'k', ...extra });

test('effortsFor: the levels differ per model, and come from the preset table', () => {
  const of = (id: string) => effortsFor(presets.find((p) => p.id === id)!);
  assert.deepEqual(of('anthropic/claude-opus-5-5'), ['low', 'medium', 'high', 'xhigh', 'max']);
  assert.deepEqual(of('zhipu-plan/glm-5.4'), ['low', 'high', 'max'], 'measured on the live endpoint');
  assert.deepEqual(of('openai/gpt-6.1-terra'), ['none', 'low', 'medium', 'high', 'xhigh', 'max']);
  assert.deepEqual(of('opencode/gpt-5.5-pro'), ['medium', 'high', 'xhigh']);
  assert.deepEqual(of('google/gemini-4-carbon'), ['none', 'low', 'medium', 'high']);
  assert.deepEqual(of('qwen/qwen3.8-max'), ['low', 'medium', 'xhigh']);
  assert.deepEqual(of('openrouter/google/gemma-4-31b-it:free'), [], 'a switch only: nothing to choose');
  // the same model behind another provider
  assert.deepEqual(of('deepseek/deepseek-v4-pro'), ['low', 'high', 'max']);
  assert.deepEqual(of('tknet/deepseek-v4.1-flash'), ['low', 'high', 'max'], 'not in the table: from the name');
});

test('effortsFor: a model nobody described yet is read from its name', () => {
  assert.deepEqual(effortsFor(spec('some-vendor/claude-sonnet-4-5')), ['low', 'medium', 'high'], 'before Claude 4.6: a token budget');
  assert.deepEqual(effortsFor(spec('claude-3-7-opus')), ['low', 'medium', 'high']);
  assert.deepEqual(effortsFor(spec('claude-haiku-5-5')), ['low', 'medium', 'high', 'xhigh', 'max']);
  assert.deepEqual(effortsFor(spec('claude-opus-4-6')), ['low', 'medium', 'high', 'xhigh', 'max']);
  assert.deepEqual(effortsFor(spec('gpt-5.5-pro')), ['medium', 'high', 'xhigh']);
  assert.deepEqual(effortsFor(spec('gpt-5.3-codex')), ['none', 'low', 'medium', 'high', 'xhigh']);
  assert.deepEqual(effortsFor(spec('gpt-6.2-sol')), ['low', 'medium', 'high', 'xhigh', 'max']);
  assert.deepEqual(effortsFor(spec('o3-mini')), ['low', 'medium', 'high']);
  assert.deepEqual(effortsFor(spec('glm-5.3x')), ['low', 'high', 'max']);
  assert.deepEqual(effortsFor(spec('glm-5.2')), ['low', 'medium', 'high', 'xhigh', 'max', 'none']);
  assert.deepEqual(effortsFor(spec('glm-4.7')), [], 'GLM before 5.2: a switch only');
  assert.deepEqual(effortsFor(spec('kimi-k3.1-pro')), ['low', 'high', 'max']);
  assert.deepEqual(effortsFor(spec('kimi-k2.7-code')), []);
  assert.deepEqual(effortsFor(spec('grok-4.7')), ['low', 'medium', 'high', 'xhigh']);
  assert.deepEqual(effortsFor(spec('grok-build-0.1')), []);
  assert.deepEqual(effortsFor(spec('gemini-9-pro')), ['low', 'medium', 'high']);
  assert.deepEqual(effortsFor(spec('minimax-m3')), []);
  assert.deepEqual(effortsFor(spec('some-unknown-model')), ['low', 'medium', 'high'], 'unknown: the commonest set, what tango always sent');
});

test('effortsFor: an explicit list wins, also when it is empty; reasoning: false leaves nothing', () => {
  assert.deepEqual(effortsFor(spec('gpt-6-astra', { efforts: ['high'] })), ['high']);
  assert.deepEqual(effortsFor(spec('gpt-6-astra', { efforts: [] })), []);
  assert.deepEqual(effortsFor(spec('some-model', { reasoning: false })), []);
});

test('claudeInfo: family and version, and when adaptive thinking starts', () => {
  assert.deepEqual(claudeInfo('claude-opus-5-5'), { family: 'opus', major: 5, minor: 5, modern: true });
  assert.deepEqual(claudeInfo('anthropic/claude-sonnet-4-5-20250929'), { family: 'sonnet', major: 4, minor: 5, modern: false });
  assert.deepEqual(claudeInfo('claude-3-7-haiku'), { family: 'haiku', major: 3, minor: 7, modern: false });
  assert.equal(claudeInfo('claude-4-6-opus')?.modern, true, 'Claude 4.6 is the first adaptive one');
  assert.equal(claudeInfo('glm-5.4'), undefined);
});

test('anthropicEffort: adaptive + output_config.effort on Claude 4.6+ and GLM, a token budget before', () => {
  // Claude Opus 5.5 on api.anthropic.com rejects `thinking.type: enabled` and its budget (measured)
  const opus = anthropicEffort('claude-opus-5-5', 'max', 8192);
  assert.deepEqual(opus, { max_tokens: 8192, thinking: { type: 'adaptive' }, output_config: { effort: 'max' } });
  // the budget stays below max_tokens with room for the answer (16000 asked, 8192 - 1024 possible)
  const sonnet45 = anthropicEffort('claude-sonnet-4-5', 'high', 8192);
  assert.deepEqual(sonnet45, { max_tokens: 8192, thinking: { type: 'enabled', budget_tokens: 7168 } });
  assert.deepEqual(anthropicEffort('claude-sonnet-4-5', 'high', 32768), { max_tokens: 32768, thinking: { type: 'enabled', budget_tokens: 16000 } });
  assert.equal('output_config' in sonnet45, false);
  // GLM on bigmodel.cn: thinking.type enabled + output_config.effort (measured: budget_tokens does nothing)
  assert.deepEqual(anthropicEffort('glm-5.4', 'low', 8192), { max_tokens: 8192, thinking: { type: 'enabled' }, output_config: { effort: 'low' } });
  assert.deepEqual(anthropicEffort('glm-5.2', 'none', 1000), { max_tokens: 2048, thinking: { type: 'disabled' } }, 'none: thinking off, and room for adaptive thinking');
  // a small max_tokens still leaves the budget below it
  const small = anthropicEffort('claude-opus-4-1', 'high', 2048);
  assert.deepEqual(small, { max_tokens: 2048, thinking: { type: 'enabled', budget_tokens: 1024 } });
});

test('openaiEffort: reasoning_effort, nested on OpenRouter, with the switch where the family needs it', () => {
  assert.deepEqual(openaiEffort('gpt-6-astra', 'https://api.openai.com/v1', 'xhigh'), { reasoning_effort: 'xhigh' });
  assert.deepEqual(openaiEffort('openai/gpt-6-astra', 'https://openrouter.ai/api/v1', 'high'), { reasoning: { effort: 'high' } });
  assert.deepEqual(openaiEffort('deepseek-v4-pro', 'https://api.deepseek.com', 'max'), { reasoning_effort: 'max', thinking: { type: 'enabled' } });
  assert.deepEqual(openaiEffort('deepseek-v4.1-flash', 'https://x.example/v1', 'none'), { reasoning_effort: 'none', thinking: { type: 'disabled' } });
  assert.deepEqual(openaiEffort('qwen3.8-max', 'https://dashscope.aliyuncs.com/compatible-mode/v1', 'xhigh'), { reasoning_effort: 'xhigh', enable_thinking: true });
  assert.deepEqual(openaiEffort('gemini-4-carbon', 'https://generativelanguage.googleapis.com/v1beta/openai', 'none'), { reasoning_effort: 'none' });
});

/** A server that answers the least each protocol needs and keeps what it was asked. */
async function capture(kind: 'anthropic' | 'openai') {
  const bodies: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      bodies.push(JSON.parse(raw));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      if (kind === 'anthropic') {
        const ev = (type: string, o: object) => `event: ${type}\ndata: ${JSON.stringify({ type, ...o })}\n\n`;
        res.end(
          ev('message_start', { message: { id: 'm1', usage: { input_tokens: 3, output_tokens: 1 } } }) +
            ev('content_block_start', { index: 0, content_block: { type: 'text', text: '' } }) +
            ev('content_block_delta', { index: 0, delta: { type: 'text_delta', text: 'ok' } }) +
            ev('content_block_stop', { index: 0 }) +
            ev('message_delta', { delta: { stop_reason: 'end_turn' }, usage: { output_tokens: 2 } }) +
            ev('message_stop', {})
        );
      } else {
        res.end(
          `data: ${JSON.stringify({ choices: [{ delta: { content: 'ok' }, finish_reason: 'stop' }], usage: { prompt_tokens: 5, completion_tokens: 2 } })}\n\ndata: [DONE]\n\n`
        );
      }
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  return { bodies, port: (server.address() as AddressInfo).port, close: () => server.close() };
}

test('adapters: what goes over the wire for a level', async () => {
  const a = await capture('anthropic');
  const o = await capture('openai');
  try {
    const ask = async (kind: 'anthropic' | 'openai', model: string, effort: string) => {
      const port = kind === 'anthropic' ? a.port : o.port;
      const s = { id: `t/${model}`, provider: 't', model, api: kind === 'anthropic' ? ('anthropic-messages' as const) : ('openai-completions' as const), baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'k', reasoning: true } as const;
      const bodies = kind === 'anthropic' ? a.bodies : o.bodies;
      for await (const ev of adapterFor(s).stream(s, { system: '', messages: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }], tools: [], effort, signal: new AbortController().signal } as any)) void ev;
      return bodies.at(-1);
    };
    // Claude on an Anthropic-style gateway: adaptive, never the budget the model rejects
    const opus = await ask('anthropic', 'claude-opus-5-5', 'xhigh');
    assert.deepEqual(opus.thinking, { type: 'adaptive' });
    assert.deepEqual(opus.output_config, { effort: 'xhigh' });
    assert.equal('budget_tokens' in (opus.thinking ?? {}), false);
    // GLM: the switch on and the level, no token budget
    const glm = await ask('anthropic', 'glm-5.4', 'max');
    assert.deepEqual(glm.thinking, { type: 'enabled' });
    assert.deepEqual(glm.output_config, { effort: 'max' });
    // an older Claude keeps the budget
    const old = await ask('anthropic', 'claude-sonnet-4-5', 'medium');
    assert.deepEqual(old.thinking, { type: 'enabled', budget_tokens: 7168 }, 'medium asks 8192, capped by max_tokens 8192');
    assert.equal('output_config' in old, false);
    // OpenAI-compatible
    const gpt = await ask('openai', 'gpt-6-astra', 'max');
    assert.equal(gpt.reasoning_effort, 'max');
    assert.equal('thinking' in gpt, false);
    const deepseek = await ask('openai', 'deepseek-v4-pro', 'high');
    assert.deepEqual([deepseek.reasoning_effort, deepseek.thinking], ['high', { type: 'enabled' }]);
    // OpenRouter's nested spelling is covered by the openaiEffort test: it is chosen by the host name,
    // and a request to that host cannot be sent to this server
  } finally {
    a.close();
    o.close();
  }
});
