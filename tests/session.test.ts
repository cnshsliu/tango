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
