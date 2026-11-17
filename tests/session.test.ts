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
