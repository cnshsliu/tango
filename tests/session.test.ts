import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { SessionStore } from '../src/core/session.js';
import { ToolRegistry, truncate } from '../src/core/tools/registry.js';
import { costUsd } from '../src/core/models/registry.js';
import type { ModelSpec } from '../src/core/types.js';

test('session round-trips messages across open/close', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-test-'));
  try {
    const store = await SessionStore.create(dir, '/work');
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'hello' }] });
    await store.appendMessage({ role: 'assistant', content: [{ type: 'text', text: 'hi' }] });

    const reopened = await SessionStore.open(dir, store.id);
    assert.equal(reopened.messages().length, 2);
    assert.equal(reopened.meta()?.cwd, '/work');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('compaction drops earlier messages from context', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-test-'));
  try {
    const store = await SessionStore.create(dir, '/work');
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'a' }] });
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'b' }] });
    await store.compact('a and b happened');

    const ctx = store.messages();
    assert.equal(ctx.length, 1);
    assert.match(ctx[0].content[0].text, /a and b happened/);

    const reopened = await SessionStore.open(dir, store.id);
    assert.equal(reopened.messages().length, 1);

    await reopened.appendMessage({ role: 'user', content: [{ type: 'text', text: 'c' }] });
    assert.equal(reopened.messages().length, 2); // summary + c
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('registry registers, overrides by name, and exports schemas', () => {
  const reg = new ToolRegistry();
  const tool = { name: 'x', description: 'd', input: { type: 'object' }, execute: async () => ({ content: [] }) };
  reg.register(tool);
  reg.register(tool);
  assert.deepEqual(reg.names(), ['x']);
  assert.deepEqual(reg.schemas(), [{ name: 'x', description: 'd', input: { type: 'object' } }]);
});

test('truncate keeps head and marks the cut', () => {
  assert.equal(truncate('short'), 'short');
  const out = truncate('x'.repeat(100), 50);
  assert.ok(out.startsWith('x'.repeat(50)) && out.includes('truncated 50 chars'));
});

test('costUsd prices cache reads separately', () => {
  const spec: ModelSpec = {
    id: 't/t',
    provider: 't',
    model: 't',
    api: 'openai-completions',
    baseUrl: 'http://x',
    cost: { input: 1, output: 2, cacheRead: 0.1 }
  };
  // 900k uncached input + 100k cached + 1M output = 0.9 + 0.01 + 2
  const usd = costUsd(spec, { input: 1_000_000, output: 1_000_000, cacheRead: 100_000 });
  assert.ok(Math.abs(usd - 2.91) < 1e-9, `got ${usd}`);
});

test('lastModel falls back to the model that last answered when no switch was recorded', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-test-'));
  try {
    const store = await SessionStore.create(dir, '/work');
    assert.equal(store.lastModel(), undefined);
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'hi' }] });
    await store.appendMessage({ role: 'assistant', content: [{ type: 'text', text: 'yo' }], model: 'tknet/deepseek-v4.1-flash' });
    const reopened = await SessionStore.open(dir, store.id);
    assert.equal(reopened.lastModel(), 'tknet/deepseek-v4.1-flash');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('apiKey: UPPER_SNAKE reads that env var, anything else is literal, "$X" is rejected', async () => {
  const { resolveApiKey, keyEnvName } = await import('../src/core/models/util.js');
  const spec = (apiKey?: string) => ({ id: 't/m', provider: 't', model: 'm', api: 'openai-completions', baseUrl: 'http://x', apiKey }) as ModelSpec;
  process.env.TANGO_TEST_KEY_1 = 'from-env';
  try {
    assert.equal(keyEnvName(spec('TANGO_TEST_KEY_1')), 'TANGO_TEST_KEY_1');
    assert.equal(resolveApiKey(spec('TANGO_TEST_KEY_1')), 'from-env');
    assert.throws(() => resolveApiKey(spec('TANGO_UNSET_KEY_9')), /TANGO_UNSET_KEY_9 is not set/);
    assert.equal(keyEnvName(spec('sk-abc123')), undefined);
    assert.equal(resolveApiKey(spec('sk-abc123')), 'sk-abc123');
    assert.throws(() => resolveApiKey(spec('$TANGO_TEST_KEY_1')), /without "\$"/);
    assert.throws(() => resolveApiKey(spec()), /no apiKey/);
  } finally {
    delete process.env.TANGO_TEST_KEY_1;
  }
});

test('write-effect tools are marked for checkpointing', async () => {
  const { buildTools } = await import('../src/host.js');
  const tools = buildTools();
  for (const name of ['write', 'edit', 'bash', 'office_write'])
    assert.equal(tools.get(name)?.writeEffect, true, name);
  for (const name of ['read', 'grep', 'find', 'ls', 'diff', 'notify'])
    assert.notEqual(tools.get(name)?.writeEffect, true, name);
});

test('rewind watermark: projection truncates, log keeps everything, redo restores', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-test-'));
  try {
    const store = await SessionStore.create(dir, '/work');
    for (const t of ['one', 'two', 'three'])
      await store.appendMessage({ role: 'user', content: [{ type: 'text', text: t }] });
    // entry 0 = meta; message entries are 1,2,3
    await store.rewindTo(1); // keep only "one"

    assert.equal(store.messages().length, 1);
    assert.equal(store.messages()[0].content[0].type === 'text' && (store.messages()[0].content[0] as any).text, 'one');

    // new work after the rewind joins the projection
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'four' }] });
    assert.equal(store.messages().length, 2);

    // the abandoned future is never lost
    assert.equal(store.allMessages().length, 4);

    // redo: rewind forward to a bigger watermark — abandoned messages visible again
    await store.rewindTo(3); // entries 1..3 visible: one, two, three (four is entry 5 → abandoned now)
    assert.equal(store.allMessages().length, 4);
    const texts = store.messages().map((m) => (m.content[0] as any).text);
    assert.deepEqual(texts, ['one', 'two', 'three']);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('rewind across a compaction restores pre-compaction context', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-test-'));
  try {
    const store = await SessionStore.create(dir, '/work');
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'early1' }] }); // entry 1
    await store.appendMessage({ role: 'assistant', content: [{ type: 'text', text: 'early2' }] }); // entry 2
    await store.compact('summary of early'); // entry 3
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'late' }] }); // entry 4

    // normally the compaction hides early messages
    assert.equal(store.messages().length, 2); // summary + late

    // rewind to before the compaction: full early context is back, no summary injected
    await store.rewindTo(2);
    const texts = store.messages().map((m) => (m.content[0] as any).text);
    assert.deepEqual(texts, ['early1', 'early2']);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('forkTo copies a prefix into a new session with forkOf lineage', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-test-'));
  try {
    const store = await SessionStore.create(dir, '/work', 'original');
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'a' }] }); // entry 1
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'b' }] }); // entry 2

    const fork = await store.forkTo(dir, 'fork-id-1', 1);
    assert.equal(fork.id, 'fork-id-1');
    assert.equal(fork.messages().length, 1);
    assert.equal(fork.meta()?.forkOf?.sessionId, store.id);
    assert.equal(fork.meta()?.forkOf?.entryIndex, 1);
    assert.equal(fork.meta()?.cwd, '/work');

    // independence: appending on either side does not leak to the other
    await fork.appendMessage({ role: 'user', content: [{ type: 'text', text: 'fork-only' }] });
    assert.equal(store.messages().length, 2);
    assert.equal(fork.messages().length, 2);

    // reopening the fork from disk sees the same projection
    const reopened = await SessionStore.open(dir, 'fork-id-1');
    assert.equal(reopened.messages().length, 2);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('relativeTime buckets ages the way the picker shows them', async () => {
  const { relativeTime } = await import('../src/core/session.js');
  const now = new Date('2026-11-20T12:00:00Z');
  assert.equal(relativeTime('2026-11-20T11:59:30Z', now), '刚刚');
  assert.equal(relativeTime('2026-11-20T11:30:00Z', now), '30 分钟前');
  assert.equal(relativeTime('2026-11-20T08:00:00Z', now), '4 小时前');
  assert.equal(relativeTime('2026-11-19T08:00:00Z', now), '昨天'); // 28h
  assert.equal(relativeTime('2026-11-16T00:00:00Z', now), '4 天前');
  assert.equal(relativeTime('2026-10-01T00:00:00Z', now), '10/01');
  assert.equal(relativeTime('garbage', now), '');
});

test('list() extracts the first user prompt as the topic', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-test-'));
  try {
    const store = await SessionStore.create(dir, '/work');
    // tool results arrive as user-role messages with no text — they are not a topic
    await store.appendMessage({ role: 'assistant', content: [{ type: 'text', text: 'thinking aloud' }] });
    await store.appendMessage({ role: 'user', content: [{ type: 'tool_result', toolCallId: 'x', content: 'output' }] });
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: '帮我修复选区漂移的问题，现象是……' }] });
    const [listed] = await SessionStore.list(dir);
    assert.equal(listed.topic, '帮我修复选区漂移的问题，现象是……');
    // a session with no user text yet has an empty topic
    const empty = await SessionStore.create(dir, '/work2', 'empty');
    const all = await SessionStore.list(dir);
    assert.equal(all.find((s) => s.id === empty.id)?.topic, '');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('meta() reflects the latest rename (list/meta/fork all agree)', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-test-'));
  try {
    const store = await SessionStore.create(dir, '/work', 'old-name');
    await store.append({ kind: 'meta', id: store.id, createdAt: new Date().toISOString(), cwd: '/work', name: 'new-name' });
    assert.equal(store.meta()?.name, 'new-name', 'M1: meta() returns the last meta entry');
    const [listed] = await SessionStore.list(dir);
    assert.equal(listed.name, 'new-name');
    const fork = await store.forkTo(dir, 'fork-x', store.indexedEntries().length - 1);
    assert.equal(fork.meta()?.name, 'new-name', 'M1: forks inherit the current name');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
