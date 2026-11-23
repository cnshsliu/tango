import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import type { ModelSpec } from '../src/core/types.js';
import { costUsd, presets } from '../src/core/models/registry.js';
import { parseUsage } from '../src/core/models/openai.js';
import { anthropicAdapter } from '../src/core/models/anthropic.js';

const preset = (id: string) => presets.find((p) => p.id === id)!;
const near = (a: number, b: number) => assert.ok(Math.abs(a - b) < 1e-9, `${a} != ${b}`);

test('costUsd bills fresh, cached and cache-write tokens separately', () => {
  const spec = preset('openai/gpt-6.1-terra');
  // 100k prompt: 60k cached, 10k written, 30k fresh; 1k output
  near(costUsd(spec, { input: 100_000, output: 1_000, cacheRead: 60_000, cacheWrite: 10_000 }),
    (30_000 * 1 + 60_000 * 0.05 + 10_000 * 1.25 + 1_000 * 5) / 1e6);
});

test('costUsd moves the whole request to the long-prompt tier', () => {
  const haiku = preset('anthropic/claude-haiku-5-5');
  near(costUsd(haiku, { input: 100_000, output: 0 }), (100_000 * 0.1) / 1e6);
  // over 100K counts cache reads too, and the cached part pays the higher read price
  near(costUsd(haiku, { input: 100_001, output: 10, cacheRead: 90_000 }),
    (10_001 * 0.5 + 90_000 * 0.05 + 10 * 2.5) / 1e6);

  const terra = preset('openai/gpt-6.1-terra');
  near(costUsd(terra, { input: 272_000, output: 0 }), (272_000 * 1) / 1e6);
  near(costUsd(terra, { input: 300_000, output: 1_000 }), (300_000 * 2 + 1_000 * 7.5) / 1e6);

  const grok = preset('xai/grok-4.7');
  near(costUsd(grok, { input: 199_999, output: 0 }), (199_999 * 2) / 1e6);
  near(costUsd(grok, { input: 200_000, output: 0 }), (200_000 * 4) / 1e6);
});

test('parseUsage reads cache writes and counts Gemini thinking as output', () => {
  // OpenAI / Kimi
  assert.deepEqual(
    parseUsage({ prompt_tokens: 100, completion_tokens: 5, total_tokens: 105, prompt_tokens_details: { cached_tokens: 40, cache_write_tokens: 30 } }),
    { input: 100, output: 5, cacheRead: 40, cacheWrite: 30 }
  );
  // Qwen explicit cache
  assert.equal(parseUsage({ prompt_tokens: 10, completion_tokens: 1, prompt_tokens_details: { cache_creation_input_tokens: 8 } }).cacheWrite, 8);
  // DeepSeek
  assert.equal(parseUsage({ prompt_tokens: 10, completion_tokens: 1, prompt_cache_hit_tokens: 6 }).cacheRead, 6);
  // Gemini: completion_tokens excludes thinking (seen live: 9 + 56 vs total 302)
  assert.equal(parseUsage({ prompt_tokens: 9, completion_tokens: 56, total_tokens: 302 }).output, 293);
});

test('anthropic usage.input includes cache reads and writes', async () => {
  const events = [
    { type: 'message_start', message: { usage: { input_tokens: 5, cache_read_input_tokens: 1000, cache_creation_input_tokens: 200, output_tokens: 1 } } },
    { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
    { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'hi' } },
    { type: 'content_block_stop', index: 0 },
    { type: 'message_delta', delta: { stop_reason: 'end_turn' }, usage: { output_tokens: 7 } },
    { type: 'message_stop' }
  ];
  const server = createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      for (const e of events) res.write(`event: ${e.type}\ndata: ${JSON.stringify(e)}\n\n`);
      res.end();
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  try {
    const spec: ModelSpec = {
      id: 'test/claude',
      provider: 'test',
      model: 'claude',
      api: 'anthropic-messages',
      baseUrl: `http://127.0.0.1:${(server.address() as AddressInfo).port}`,
      apiKey: 'sk-test'
    };
    let usage;
    for await (const ev of anthropicAdapter.stream(spec, { system: '', messages: [], tools: [] }))
      if (ev.type === 'done') usage = ev.message.usage;
    assert.deepEqual(usage, { input: 1205, output: 7, cacheRead: 1000, cacheWrite: 200 });
  } finally {
    server.close();
  }
});

test('postJson retries 502 with backoff up to `retries`, announcing each retry', async () => {
  const { postJson } = await import('../src/core/models/util.js');
  let hits = 0;
  let failTimes = 1;
  let status = 502;
  const server = createServer((req, res) => {
    req.resume();
    req.on('end', () => {
      hits++;
      if (hits <= failTimes) {
        res.writeHead(status, { 'content-type': 'application/json' });
        res.end('{"error":{"message":"Stream failed"}}');
      } else {
        res.writeHead(200, { 'content-type': 'text/plain' });
        res.end('ok');
      }
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/v1`;
  const spec = { id: 'mock/mock', apiKey: 'X' } as unknown as ModelSpec;
  const seen: { attempt: number; max: number; reason: string }[] = [];
  const opts = { baseMs: 1, onRetry: (i: { attempt: number; max: number; reason: string }) => void seen.push(i) };
  try {
    const res = await postJson(spec, url, {}, {}, undefined, opts);
    assert.equal(res.status, 200);
    assert.equal(hits, 2);
    assert.deepEqual(seen.map((i) => [i.attempt, i.max, i.reason]), [[1, 3, 'HTTP 502']]);

    hits = 0;
    failTimes = 99;
    seen.length = 0;
    await assert.rejects(postJson(spec, url, {}, {}, undefined, opts), /HTTP 502[\s\S]*gave up after 3 retries/);
    assert.equal(hits, 4, 'one try plus 3 retries');
    assert.deepEqual(seen.map((i) => i.attempt), [1, 2, 3]);

    hits = 0;
    await assert.rejects(postJson(spec, url, {}, {}, undefined, { ...opts, retries: 0 }), /HTTP 502/);
    assert.equal(hits, 1, 'retries: 0 turns retrying off');

    hits = 0;
    await assert.rejects(postJson({ ...spec, retries: 1 }, url, {}, {}, undefined, opts), /gave up after 1 retry\b/);
    assert.equal(hits, 2, 'a model spec `retries` overrides the default');

    hits = 0;
    status = 401;
    await assert.rejects(postJson(spec, url, {}, {}, undefined, opts), /HTTP 401/);
    assert.equal(hits, 1, 'auth errors are not retried');
  } finally {
    server.close();
  }
});

test('postJson retries a reset connection, and an abort stops the backoff wait', async () => {
  const { postJson } = await import('../src/core/models/util.js');
  let hits = 0;
  const server = createServer((req) => {
    hits++;
    req.socket.destroy(); // ECONNRESET / "other side closed" for the client
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/v1`;
  const spec = { id: 'mock/mock', apiKey: 'X' } as unknown as ModelSpec;
  try {
    const seen: number[] = [];
    await assert.rejects(
      postJson(spec, url, {}, {}, undefined, { retries: 2, baseMs: 1, onRetry: (i) => void seen.push(i.attempt) }),
      /cannot reach 127\.0\.0\.1:\d+: .*gave up after 2 retries/
    );
    assert.equal(hits, 3);
    assert.deepEqual(seen, [1, 2]);

    hits = 0;
    const ctl = new AbortController();
    const t0 = Date.now();
    const p = postJson(spec, url, {}, {}, ctl.signal, { baseMs: 60_000, onRetry: () => ctl.abort() });
    await assert.rejects(p, (e: any) => e?.name === 'AbortError');
    assert.ok(Date.now() - t0 < 5_000, 'abort cuts the backoff short');
    assert.equal(hits, 1);
  } finally {
    server.close();
  }
});
