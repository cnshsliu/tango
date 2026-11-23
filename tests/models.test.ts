import { test } from 'node:test';
import assert from 'node:assert/strict';
import { presets, platformOf, isFreeModel, displayPlatform, groupModels } from '../src/core/models/registry.js';
import type { ModelSpec } from '../src/core/types.js';

const spec = (over: Partial<ModelSpec>): ModelSpec => ({
  id: 't/m', provider: 't', model: 'm', api: 'openai-completions', baseUrl: 'http://x', ...over
});

test('zhipu coding-plan presets ride the anthropic-compatible endpoint', async () => {
  const models = presets;
  const plan = models.find((m) => m.id === 'zhipu-plan/glm-5.3');
  assert.ok(plan, 'coding-plan glm-5.3 preset registered');
  assert.equal(plan.api, 'anthropic-messages');
  assert.equal(plan.baseUrl, 'https://open.bigmodel.cn/api/anthropic/v1');
  assert.equal(plan.apiKey, 'ZHIPU_CODING_PLAN_API_KEY');
  const next = models.find((m) => m.id === 'zhipu-plan/glm-5.4');
  assert.ok(next, 'coding-plan glm-5.4 preset registered');
  assert.deepEqual([next.api, next.baseUrl, next.apiKey, next.model], [plan.api, plan.baseUrl, plan.apiKey, 'glm-5.4']);
  const flash = models.find((m) => m.id === 'zhipu-plan/glm-5.3-flash');
  assert.ok(flash, 'flash variant registered');
  // the pay-as-you-go presets stay untouched
  assert.ok(models.find((m) => m.id === 'zhipu/glm-5.3' && m.api === 'openai-completions'));
});

test('pickModel resolves fuzzily: subsequence, case-insensitive, multi-word; ambiguous lists candidates', async () => {
  const { pickModel } = await import('../src/core/config.js');
  const m = (id: string) => ({ id, provider: id.split('/')[0], model: id.split('/')[1] ?? id, api: 'openai-completions' as const, baseUrl: 'x' });
  const models = ['zhipu-plan/glm-5.4', 'zhipu-plan/glm-5.3', 'zhipu/glm-5.3', 'apikeyfan/claude-opus-5-5', 'tknet/qwen3.8-flash-next'].map(m);
  const p = (q: string) => pickModel(models, {}, q).id;
  assert.equal(p('zhipu-plan/glm-5.4'), 'zhipu-plan/glm-5.4', 'exact still wins');
  assert.equal(p('glm54'), 'zhipu-plan/glm-5.4', 'separators may be skipped');
  assert.equal(p('GLM-5.4'), 'zhipu-plan/glm-5.4', 'case-insensitive');
  assert.equal(p('opus'), 'apikeyfan/claude-opus-5-5', 'name part alone');
  assert.equal(p('zp glm5.4'), 'zhipu-plan/glm-5.4', 'every word must match');
  assert.throws(() => p('glm'), /ambiguous model "glm": zhipu-plan\/glm-5\.4, zhipu-plan\/glm-5\.3, zhipu\/glm-5\.3/);
  assert.throws(() => p('glm5'), /ambiguous/);
  assert.throws(() => p('nonexistent'), /unknown model "nonexistent"/);
});

test('platformOf: explicit platform wins, provider is the fallback', () => {
  assert.equal(platformOf(spec({ platform: 'opencode', provider: 'opencode' })), 'opencode');
  assert.equal(platformOf(spec({ provider: 'tknet' })), 'tknet');
});

test('isFreeModel: only zero input AND output cost; unknown cost is not free', () => {
  assert.ok(isFreeModel(spec({ cost: { input: 0, output: 0 } })));
  assert.ok(!isFreeModel(spec({})));
  assert.ok(!isFreeModel(spec({ cost: { input: 0, output: 1 } })));
  assert.ok(!isFreeModel(spec({ cost: { input: 1, output: 0 } })));
});

test('groupModels: virtual free platform first, sources keep first-appearance order', () => {
  const models = [
    spec({ id: 'tknet/a', provider: 'tknet', cost: { input: 1, output: 1 } }),
    spec({ id: 'oc/free', provider: 'opencode', cost: { input: 0, output: 0 } }),
    spec({ id: 'tknet/b', provider: 'tknet', cost: { input: 1, output: 1 } }),
    spec({ id: 'other/free', provider: 'other', cost: { input: 0, output: 0 } }),
    spec({ id: 'oc/paid', provider: 'opencode', cost: { input: 1, output: 1 } })
  ];
  const groups = groupModels(models);
  assert.deepEqual(groups.map(([p]) => p), ['free', 'tknet', 'opencode']);
  assert.deepEqual(groups[0][1].map((m) => m.id), ['oc/free', 'other/free']);
  assert.equal(displayPlatform(models[1]), 'free');
  assert.equal(displayPlatform(models[0]), 'tknet');
});

test('opencode presets: 35 paid models, key via OPENCODE_API_KEY env, no opencode install needed', () => {
  const oc = presets.filter((m) => m.platform === 'opencode');
  assert.equal(oc.length, 35);
  for (const m of oc) {
    assert.equal(m.provider, 'opencode');
    assert.equal(m.apiKey, 'OPENCODE_API_KEY');
    assert.ok(m.id.startsWith('opencode/'));
    assert.ok((m.contextWindow ?? 0) > 0, m.id);
    assert.ok(m.cost, m.id);
    if (m.model.startsWith('claude')) {
      assert.equal(m.api, 'anthropic-messages', m.id);
      assert.equal(m.baseUrl, 'https://opencode.ai/inference/anthropic/v1', m.id);
    } else {
      assert.equal(m.api, 'openai-completions', m.id);
      assert.equal(m.baseUrl, 'https://opencode.ai/inference/openai/v1', m.id);
    }
    // harness-locked free models must NOT be preset (429 from non-OpenCode clients)
    assert.ok(!isFreeModel(m), `${m.id} is free — opencode free models are harness-locked`);
  }
  // long-prompt tiers survive the catalog → preset mapping, ascending
  const sol = oc.find((m) => m.id === 'opencode/gpt-6.2-sol')!;
  assert.deepEqual(sol.cost!.tiers, [{ above: 272000, input: 4, cacheRead: 0.2, cacheWrite: 5, output: 15 }]);
  const haiku = oc.find((m) => m.id === 'opencode/claude-haiku-5-5')!;
  assert.equal(haiku.cost!.tiers![0].above, 100000);
});

test('local Phai preset: NovaMLX endpoint, key from NOVA_API_KEY, text only, never the default', () => {
  const phai = presets.find((m) => m.id === 'local/phai-a1-35b')!;
  assert.ok(phai);
  assert.equal(phai.model, 'PhaiModelLab/Phai-A1-35B-1M-MLX-4bit');
  assert.equal(phai.api, 'openai-completions');
  assert.equal(phai.baseUrl, 'http://127.0.0.1:6590/v1');
  assert.equal(phai.apiKey, 'NOVA_API_KEY');
  assert.equal(phai.contextWindow, 262144);
  assert.equal(phai.maxTokens, 4096);
  assert.equal(phai.vision, false);
  assert.equal(presets.at(-1)!.id, phai.id, 'appended last so it never becomes the default');
});

test('presets: ids stay unique after the opencode batch; existing defaults unchanged', () => {
  assert.equal(new Set(presets.map((m) => m.id)).size, presets.length);
  assert.equal(presets[0].id, 'tknet/qwen3.8-flash-next', 'tknet stays the first default');
  // every preset groups under a platform (provider fallback)
  for (const [p, list] of groupModels(presets)) {
    assert.ok(p.length > 0);
    for (const m of list) assert.equal(displayPlatform(m), p);
  }
});