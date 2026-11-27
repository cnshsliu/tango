import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import type { CompletionRequest, ModelSpec, RetryInfo, StreamEvent } from '../src/core/types.js';
import { anthropicAdapter } from '../src/core/models/anthropic.js';
import { openaiAdapter } from '../src/core/models/openai.js';
import {
  UNCLASSIFIED_RETRIES,
  apiErrorRetry,
  sseLines,
  streamApiError,
  streamWithRetry
} from '../src/core/models/util.js';

/** An SSE body out of events; a plain string is sent as is (`[DONE]`). */
const sse = (events: unknown[]) => events.map((e) => `data: ${typeof e === 'string' ? e : JSON.stringify(e)}\n\n`).join('');

const GOOD_ANTHROPIC = sse([
  { type: 'message_start', message: { usage: { input_tokens: 5, output_tokens: 1 } } },
  { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
  { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'hi' } },
  { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: '!' } },
  { type: 'content_block_stop', index: 0 },
  { type: 'message_delta', delta: { stop_reason: 'end_turn' }, usage: { output_tokens: 2 } },
  { type: 'message_stop' }
]);
const GOOD_OPENAI = sse([
  { choices: [{ delta: { role: 'assistant', content: 'hi' } }] },
  { choices: [{ delta: { content: '!' }, finish_reason: 'stop' }] },
  '[DONE]'
]);
const textOf = (events: StreamEvent[]) => events.filter((e) => e.type === 'text').map((e) => (e as { delta: string }).delta).join('');

/** A server that answers each request with the next reply (the last one repeats), and counts. */
function scripted(replies: { status?: number; body: string }[]) {
  let hits = 0;
  const bodies: string[] = [];
  const server = createServer((req, res) => {
    let buf = '';
    req.on('data', (c) => (buf += c));
    req.on('end', () => {
      const r = replies[Math.min(hits, replies.length - 1)];
      bodies.push(buf);
      hits++;
      res.writeHead(r.status ?? 200, { 'content-type': 'text/event-stream' });
      res.end(r.body);
    });
  });
  return {
    server,
    hits: () => hits,
    bodies,
    listen: async () => {
      await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
      return `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
    }
  };
}

async function collect(iter: AsyncIterable<StreamEvent>): Promise<StreamEvent[]> {
  const out: StreamEvent[] = [];
  for await (const ev of iter) out.push(ev);
  return out;
}

const req = (extra: Partial<CompletionRequest> = {}): CompletionRequest => ({ system: '', messages: [], tools: [], ...extra });

// apikey.fan on 2026-11-25: HTTP 200, then `{"type":"error","error":{"type":"upstream_error",…}}`
// as the first and only event. One scheduled task reported it as the answer and stopped.
test('anthropic: an error event inside HTTP 200 is retried while nothing was output yet', async () => {
  const srv = scripted([
    { body: sse([{ type: 'error', error: { type: 'upstream_error', message: 'Upstream request failed' } }]) },
    { body: GOOD_ANTHROPIC }
  ]);
  const baseUrl = await srv.listen();
  const spec: ModelSpec = { id: 'gw/claude', provider: 'gw', model: 'claude', api: 'anthropic-messages', baseUrl, apiKey: 'k' };
  const seen: RetryInfo[] = [];
  try {
    const events = await collect(anthropicAdapter.stream(spec, req({ retries: 3, onRetry: (i) => void seen.push(i) })));
    assert.equal(textOf(events), 'hi!', 'the second request is the answer');
    assert.equal(events.at(-1)?.type, 'done');
    assert.equal(srv.hits(), 2);
    assert.equal(srv.bodies[1], srv.bodies[0], 'the retry sends the same request');
    assert.deepEqual(seen, [{ attempt: 1, max: 3, reason: 'upstream_error: Upstream request failed', delayMs: 1000 }]);
  } finally {
    srv.server.close();
  }
});

test('openai: a rate-limit error event (OpenRouter style) inside HTTP 200 is retried', async () => {
  const srv = scripted([
    { body: sse([{ error: { message: 'Rate limit reached', type: 'requests', code: 429 } }]) },
    { body: GOOD_OPENAI }
  ]);
  const baseUrl = await srv.listen();
  const spec: ModelSpec = { id: 'gw/gpt', provider: 'gw', model: 'gpt', api: 'openai-chat', baseUrl, apiKey: 'k' };
  const seen: RetryInfo[] = [];
  try {
    const events = await collect(openaiAdapter.stream(spec, req({ onRetry: (i) => void seen.push(i) })));
    assert.equal(textOf(events), 'hi!');
    assert.equal(srv.hits(), 2);
    assert.deepEqual(seen.map((i) => [i.attempt, i.max, i.reason]), [[1, 10, 'requests: Rate limit reached']]);
  } finally {
    srv.server.close();
  }
});

test('a stream that goes silent before any output is retried', async () => {
  const spec = { id: 'm/m' } as ModelSpec;
  let calls = 0;
  const start = async function* (): AsyncIterable<StreamEvent> {
    if (calls++ === 0) {
      // comments only, and the body never closes: no data: line ever arrives
      const body = new ReadableStream<Uint8Array>({ start(c) { c.enqueue(new TextEncoder().encode(': keep-alive\n')); } });
      for await (const line of sseLines(body, 20)) yield { type: 'text', delta: line };
      return;
    }
    yield { type: 'text', delta: 'ok' };
    yield { type: 'done', message: { role: 'assistant', content: [{ type: 'text', text: 'ok' }], usage: { input: 0, output: 0 }, model: 'm', stopReason: 'stop' } };
  };
  const seen: RetryInfo[] = [];
  const events = await collect(streamWithRetry(spec, { onRetry: (i) => void seen.push(i) }, start, { baseMs: 1 }));
  assert.equal(textOf(events), 'ok');
  assert.equal(calls, 2);
  assert.match(seen[0].reason, /^stream stalled: no data for \d+s$/);
});

test('a failure after output began is passed on, never replayed', async () => {
  const srv = scripted([
    {
      body: sse([
        { type: 'message_start', message: { usage: { input_tokens: 5, output_tokens: 1 } } },
        { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } },
        { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'hel' } },
        { type: 'error', error: { type: 'overloaded_error', message: 'Overloaded' } }
      ])
    }
  ]);
  const baseUrl = await srv.listen();
  const spec: ModelSpec = { id: 'gw/claude', provider: 'gw', model: 'claude', api: 'anthropic-messages', baseUrl, apiKey: 'k' };
  let retries = 0;
  try {
    await assert.rejects(
      collect(anthropicAdapter.stream(spec, req({ onRetry: () => void retries++ }))),
      /gw\/claude: \{"type":"overloaded_error","message":"Overloaded"\}/
    );
    assert.equal(srv.hits(), 1, 'one request only');
    assert.equal(retries, 0);
  } finally {
    srv.server.close();
  }
});

test('an error a retry cannot fix is shown at once', async () => {
  const srv = scripted([
    { body: sse([{ type: 'error', error: { type: 'invalid_request_error', message: 'max_tokens: must be greater than 1' } }]) }
  ]);
  const baseUrl = await srv.listen();
  const spec: ModelSpec = { id: 'gw/claude', provider: 'gw', model: 'claude', api: 'anthropic-messages', baseUrl, apiKey: 'k' };
  let retries = 0;
  try {
    await assert.rejects(
      collect(anthropicAdapter.stream(spec, req({ onRetry: () => void retries++ }))),
      /invalid_request_error[\s\S]*must be greater than 1/
    );
    assert.equal(srv.hits(), 1);
    assert.equal(retries, 0);
  } finally {
    srv.server.close();
  }
});

/** `start` that fails every time with the given error object, counting its calls. */
function alwaysFailing(err: unknown) {
  const spec = { id: 'm/m' } as ModelSpec;
  let calls = 0;
  const start = async function* (): AsyncIterable<StreamEvent> {
    calls++;
    throw streamApiError(spec, err);
  };
  return { start, calls: () => calls, spec };
}

test('when the retries run out the error says so, and each retry was announced', async () => {
  const f = alwaysFailing({ type: 'upstream_error', message: 'Upstream request failed' });
  const seen: RetryInfo[] = [];
  await assert.rejects(
    collect(streamWithRetry(f.spec, { retries: 3, onRetry: (i) => void seen.push(i) }, f.start, { baseMs: 1 })),
    /Upstream request failed"\} \(gave up after 3 retries\)$/
  );
  assert.equal(f.calls(), 4, 'one try plus 3 retries');
  assert.deepEqual(seen.map((i) => [i.attempt, i.max, i.delayMs]), [[1, 3, 1], [2, 3, 2], [3, 3, 4]]);
});

test('retries: 0 turns retrying off; the model spec supplies the count', async () => {
  const a = alwaysFailing({ type: 'overloaded_error', message: 'Overloaded' });
  let retries = 0;
  await assert.rejects(collect(streamWithRetry(a.spec, { retries: 0, onRetry: () => void retries++ }, a.start, { baseMs: 1 })), /Overloaded"\}$/);
  assert.equal(a.calls(), 1);
  assert.equal(retries, 0);

  const spec = { id: 'm/m', retries: 2 } as ModelSpec;
  const b = alwaysFailing({ type: 'overloaded_error', message: 'Overloaded' });
  await assert.rejects(collect(streamWithRetry(spec, {}, b.start, { baseMs: 1 })), /gave up after 2 retries/);
  assert.equal(b.calls(), 3);
});

test('an error tango cannot classify gets a few retries, not all of them', async () => {
  const f = alwaysFailing({ type: 'banana_error', message: '???' });
  const seen: RetryInfo[] = [];
  await assert.rejects(collect(streamWithRetry(f.spec, { onRetry: (i) => void seen.push(i) }, f.start, { baseMs: 1 })), /gave up after 3 retries/);
  assert.equal(f.calls(), 1 + UNCLASSIFIED_RETRIES);
  assert.deepEqual(seen.map((i) => [i.attempt, i.max]), [[1, 3], [2, 3], [3, 3]]);
});

test('an abort during the backoff wait stops the retrying', async () => {
  const spec = { id: 'm/m' } as ModelSpec;
  let calls = 0;
  const start = async function* (): AsyncIterable<StreamEvent> {
    calls++;
    throw streamApiError(spec, { type: 'overloaded_error', message: 'Overloaded' });
  };
  const ctl = new AbortController();
  await assert.rejects(
    collect(streamWithRetry(spec, { signal: ctl.signal, onRetry: () => ctl.abort() }, start, { baseMs: 60_000 })),
    { name: 'AbortError' }
  );
  assert.equal(calls, 1, 'no second request after the abort');
});

test('a failure that is not a stream failure is not retried', async () => {
  const spec = { id: 'm/m' } as ModelSpec;
  let calls = 0;
  let retries = 0;
  const start = async function* (): AsyncIterable<StreamEvent> {
    calls++;
    throw new Error('m/m: malformed stream event: <html>');
  };
  await assert.rejects(collect(streamWithRetry(spec, { onRetry: () => void retries++ }, start, { baseMs: 1 })), /malformed stream event/);
  assert.equal(calls, 1);
  assert.equal(retries, 0);
});

test('apiErrorRetry: transient errors get the full count, broken requests none, the rest a few', () => {
  const yes = [
    { type: 'overloaded_error', message: 'Overloaded' },
    { type: 'upstream_error', message: 'Upstream request failed' },
    { type: 'rate_limit_error', message: 'Rate limit exceeded' },
    { type: 'api_error', message: 'Internal server error' },
    { type: 'timeout_error', message: 'Request timed out.' },
    { type: 'server_error', code: 'server_error', message: 'The server had an error while processing your request.' },
    { type: 'requests', code: 429, message: 'Rate limit reached for gpt-4 on requests per min (RPM)' },
    { code: 'rate_limit_exceeded', message: 'Rate limit reached' },
    { status: 502, message: 'Bad gateway' },
    'Rate limit reached for requests per minute',
    'The engine is currently overloaded'
  ];
  const no = [
    { type: 'invalid_request_error', message: 'max_tokens: must be greater than 1' },
    { type: 'authentication_error', message: 'invalid x-api-key' },
    { type: 'permission_error', message: 'Your API key does not have permission' },
    { type: 'not_found_error', message: 'model: claude-x' },
    { type: 'invalid_request_error', message: 'Output blocked by content filtering policy' },
    { message: 'insufficient balance' },
    { type: 'insufficient_quota', code: 'insufficient_quota', message: 'You exceeded your current quota' },
    { type: 'invalid_request_error', message: 'messages: text content blocks must be non-empty' },
    { status: 403, message: 'Request not allowed' },
    { status: 400, message: 'temporarily weird' }
  ];
  const maybe = [{ type: 'banana_error', message: '???' }, { message: '' }, undefined];
  for (const e of yes) assert.equal(apiErrorRetry(e), 'yes', JSON.stringify(e));
  for (const e of no) assert.equal(apiErrorRetry(e), 'no', JSON.stringify(e));
  for (const e of maybe) assert.equal(apiErrorRetry(e), 'maybe', JSON.stringify(e));
});

test('streamApiError keeps the message users saw and adds the short reason for the retry line', () => {
  const spec = { id: 'gw/claude' } as ModelSpec;
  const e = streamApiError(spec, { type: 'overloaded_error', message: 'Overloaded' });
  assert.equal(e.message, 'gw/claude: {"type":"overloaded_error","message":"Overloaded"}');
  assert.equal(e.reason, 'overloaded_error: Overloaded');
  assert.equal(e.retry, 'yes');
  assert.equal(streamApiError(spec, 'bad gateway').reason, 'error: bad gateway');
  assert.equal(streamApiError(spec, { message: 'x'.repeat(300) }).reason.length, 120, 'the reason is capped');
});
