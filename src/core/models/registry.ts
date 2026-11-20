import type { ApiKind, ModelAdapter, ModelSpec, Prices, Usage } from '../types.js';
import { anthropicAdapter } from './anthropic.js';
import { openaiAdapter } from './openai.js';

const adapters: Record<ApiKind, ModelAdapter> = {
  'anthropic-messages': anthropicAdapter,
  'openai-completions': openaiAdapter
};

export function adapterFor(spec: ModelSpec): ModelAdapter {
  const a = adapters[spec.api];
  if (!a) throw new Error(`unknown api kind: ${spec.api}`);
  return a;
}

/**
 * Built-in presets. `region` is where the endpoint is served: `gateway` (tknet, the user's own
 * gateway), `cn` (mainland China, no proxy needed) or `intl`. Array order decides the default
 * (first preset with a key); tknet stays first. `--models` lists them grouped by region.
 * Long-input tiers follow each vendor's pricing page; see costUsd.
 * Users override or extend via ~/.tango/models.json.
 */
export const presets: ModelSpec[] = [
  {
    id: 'tknet/qwen3.8-flash-next',
    region: 'gateway',
    provider: 'tknet',
    model: 'qwen3.8-flash-next',
    api: 'openai-completions',
    baseUrl: 'https://tknet.ai/api/v1',
    apiKey: 'TKNET_API_KEY',
    contextWindow: 200000,
    maxTokens: 16384,
    cost: { input: 0.075, output: 0.235, cacheRead: 0.008 }
  },
  {
    id: 'tknet/deepseek-v4.1-flash',
    region: 'gateway',
    provider: 'tknet',
    model: 'deepseek-v4.1-flash',
    api: 'openai-completions',
    baseUrl: 'https://tknet.ai/api/v1',
    apiKey: 'TKNET_API_KEY',
    contextWindow: 1000000,
    maxTokens: 16384,
    cost: { input: 0.135, output: 0.54, cacheRead: 0.003 }
  },
  {
    id: 'tknet/deepseek-v4-pro',
    region: 'gateway',
    provider: 'tknet',
    model: 'deepseek-v4-pro',
    api: 'openai-completions',
    baseUrl: 'https://tknet.ai/api/v1',
    apiKey: 'TKNET_API_KEY',
    contextWindow: 128000,
    maxTokens: 16384,
    cost: { input: 0.6525, output: 1.305, cacheRead: 0.005438 }
  },
  // qwen4-tokenplan: the all-protocol token plan endpoint (chat-completions is primary).
  // Checked 2026-11-17: reasoning_content streaming, tool calls over /compatible-mode/v1.
  ...([
    ['qwen/qwen4-max', 'qwen4-max', 1_000_000],
    ['qwen/qwen4-max-1m', 'qwen4-max-1m', 1_000_000],
    ['qwen/deepseek-v4-pro', 'deepseek-v4-pro', 128_000],
    ['qwen/glm-5.3', 'glm-5.3', 128_000],
    ['qwen/qwen4-flash', 'qwen4-flash', 128_000]
  ] as const).map(([id, model, contextWindow]) => ({
    id,
    provider: 'qwen-tokenplan',
    model,
    api: 'openai-completions' as const,
    baseUrl: 'https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1',
    apiKey: 'QWEN4_API_KEY',
    contextWindow,
    reasoning: true,
    region: 'cn' as const
  })),
  // China presets checked against official docs on 2026-11-14. CNY prices converted at 6.6849 CNY/USD.
  // DeepSeek costs are peak-hour prices; off-peak is half.
  {
    id: 'deepseek/deepseek-flash',
    region: 'cn',
    provider: 'deepseek',
    model: 'deepseek-flash',
    api: 'openai-completions',
    baseUrl: 'https://api.deepseek.com',
    apiKey: 'DEEPSEEK_API_KEY',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.3, output: 1.2, cacheRead: 0.006 }
  },
  {
    id: 'deepseek/deepseek-v4-pro',
    region: 'cn',
    provider: 'deepseek',
    model: 'deepseek-v4-pro',
    api: 'openai-completions',
    baseUrl: 'https://api.deepseek.com',
    apiKey: 'DEEPSEEK_API_KEY',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1.32, output: 3.96, cacheRead: 0.044 }
  },
  {
    id: 'kimi/kimi-k3.1-pro',
    region: 'cn',
    provider: 'kimi',
    model: 'kimi-k3.1-pro',
    api: 'openai-completions',
    baseUrl: 'https://api.moonshot.cn/v1',
    apiKey: 'KIMI_API_KEY',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1.945, output: 10.172, cacheRead: 0.1945, cacheWrite: 1.945 }
  },
  {
    id: 'kimi/kimi-k3.1-pro-highspeed',
    region: 'cn',
    provider: 'kimi',
    model: 'kimi-k3.1-pro-highspeed',
    api: 'openai-completions',
    baseUrl: 'https://api.moonshot.cn/v1',
    apiKey: 'KIMI_API_KEY',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 3.889, output: 20.344, cacheRead: 0.1945, cacheWrite: 3.889 }
  },
  {
    id: 'qwen/qwen3.8-max',
    region: 'cn',
    provider: 'qwen',
    model: 'qwen3.8-max',
    api: 'openai-completions',
    baseUrl: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
    apiKey: 'DASHSCOPE_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1.795, output: 5.385 }
  },
  {
    id: 'qwen/qwen3.8-flash',
    region: 'cn',
    provider: 'qwen',
    model: 'qwen3.8-flash',
    api: 'openai-completions',
    baseUrl: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
    apiKey: 'DASHSCOPE_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.12, output: 0.404 }
  },
  {
    id: 'zhipu/glm-5.3',
    region: 'cn',
    provider: 'zhipu',
    model: 'glm-5.3',
    api: 'openai-completions',
    baseUrl: 'https://open.bigmodel.cn/api/paas/v4',
    apiKey: 'Z_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1.197, output: 4.189, cacheRead: 0.299 }
  },
  {
    id: 'zhipu/glm-5.3-flash',
    region: 'cn',
    provider: 'zhipu',
    model: 'glm-5.3-flash',
    api: 'openai-completions',
    baseUrl: 'https://open.bigmodel.cn/api/paas/v4',
    apiKey: 'Z_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.12, output: 0.419, cacheRead: 0.034 }
  },
  {
    // GLM Coding Plan: subscription (not per-token), Anthropic-protocol endpoint —
    // same key Claude Code uses for its GLM plans
    id: 'zhipu-plan/glm-5.3',
    region: 'cn',
    provider: 'zhipu-plan',
    model: 'glm-5.3',
    api: 'anthropic-messages',
    baseUrl: 'https://open.bigmodel.cn/api/anthropic',
    apiKey: 'ZHIPU_CODING_PLAN_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true
  },
  {
    id: 'zhipu-plan/glm-5.3-flash',
    region: 'cn',
    provider: 'zhipu-plan',
    model: 'glm-5.3-flash',
    api: 'anthropic-messages',
    baseUrl: 'https://open.bigmodel.cn/api/anthropic',
    apiKey: 'ZHIPU_CODING_PLAN_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true
  },
  {
    id: 'kimi-code/kimi-for-coding',
    region: 'cn',
    provider: 'kimi-code',
    model: 'kimi-for-coding',
    api: 'openai-completions',
    baseUrl: 'https://api.kimi.com/coding/v1',
    apiKey: 'KIMI_CODE_API_KEY',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true
  },
  {
    id: 'kimi-intl/kimi-k3.1-pro',
    region: 'intl',
    provider: 'kimi-intl',
    model: 'kimi-k3.1-pro',
    api: 'openai-completions',
    baseUrl: 'https://api.moonshot.ai/v1',
    apiKey: 'KIMI_API_KEY',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 2, output: 10, cacheRead: 0.2, cacheWrite: 2 }
  },
  {
    id: 'kimi-intl/kimi-k3.1-pro-highspeed',
    region: 'intl',
    provider: 'kimi-intl',
    model: 'kimi-k3.1-pro-highspeed',
    api: 'openai-completions',
    baseUrl: 'https://api.moonshot.ai/v1',
    apiKey: 'KIMI_API_KEY',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 4, output: 20, cacheRead: 0.2, cacheWrite: 4 }
  },
  {
    id: 'qwen-us/qwen3.8-max',
    region: 'intl',
    provider: 'qwen-us',
    model: 'qwen3.8-max',
    api: 'openai-completions',
    baseUrl: 'https://dashscope-us.aliyuncs.com/compatible-mode/v1',
    apiKey: 'DASHSCOPE_US_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 2.242, output: 6.726 }
  },
  {
    id: 'qwen-us/qwen3.8-flash',
    region: 'intl',
    provider: 'qwen-us',
    model: 'qwen3.8-flash',
    api: 'openai-completions',
    baseUrl: 'https://dashscope-us.aliyuncs.com/compatible-mode/v1',
    apiKey: 'DASHSCOPE_US_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.164, output: 0.513 }
  },
  // US presets checked against official docs on 2026-11-14. Long-prompt tiers: Haiku >100K,
  // OpenAI >272K (2x input/cache, 1.5x output), xAI >=200K; the whole request moves up a tier.
  {
    id: 'anthropic/claude-opus-5-5',
    region: 'intl',
    provider: 'anthropic',
    model: 'claude-opus-5-5',
    api: 'anthropic-messages',
    baseUrl: 'https://api.anthropic.com/v1',
    apiKey: 'ANTHROPIC_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 4, output: 20, cacheRead: 0.2, cacheWrite: 5 }
  },
  {
    id: 'anthropic/claude-haiku-5-5',
    region: 'intl',
    provider: 'anthropic',
    model: 'claude-haiku-5-5',
    api: 'anthropic-messages',
    baseUrl: 'https://api.anthropic.com/v1',
    apiKey: 'ANTHROPIC_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: {
      input: 0.1,
      output: 0.5,
      cacheRead: 0.01,
      cacheWrite: 0.125,
      tiers: [{ above: 100_000, input: 0.5, output: 2.5, cacheRead: 0.05, cacheWrite: 0.625 }]
    }
  },
  {
    id: 'openai/gpt-6-astra',
    region: 'intl',
    provider: 'openai',
    model: 'gpt-6-astra',
    api: 'openai-completions',
    baseUrl: 'https://api.openai.com/v1',
    apiKey: 'OPENAI_API_KEY',
    contextWindow: 1050000,
    maxTokens: 32768,
    reasoning: true,
    cost: {
      input: 10,
      output: 50,
      cacheRead: 1,
      cacheWrite: 12.5,
      tiers: [{ above: 272_000, input: 20, output: 75, cacheRead: 2, cacheWrite: 25 }]
    }
  },
  {
    // GPT-6 Luna is cheaper, but on Chat Completions it only calls tools with reasoning_effort "none".
    id: 'openai/gpt-6.1-terra',
    region: 'intl',
    provider: 'openai',
    model: 'gpt-6.1-terra',
    api: 'openai-completions',
    baseUrl: 'https://api.openai.com/v1',
    apiKey: 'OPENAI_API_KEY',
    contextWindow: 1050000,
    maxTokens: 32768,
    reasoning: true,
    cost: {
      input: 1,
      output: 5,
      cacheRead: 0.05,
      cacheWrite: 1.25,
      tiers: [{ above: 272_000, input: 2, output: 7.5, cacheRead: 0.1, cacheWrite: 2.5 }]
    }
  },
  {
    // Vertex AI list prices; through 2026-11-30 Google refunds 50% as credits.
    id: 'google/gemini-4-carbon',
    region: 'intl',
    provider: 'google',
    model: 'gemini-4-carbon',
    api: 'openai-completions',
    baseUrl: 'https://generativelanguage.googleapis.com/v1beta/openai',
    apiKey: 'GEMINI_API_KEY',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 4, output: 20, cacheRead: 0.2 }
  },
  {
    // introductory price through 2026-12-31, then $1.5 / $0.15 / $7.5; same price above 200K
    id: 'google/gemini-3.8-flash',
    region: 'intl',
    provider: 'google',
    model: 'gemini-3.8-flash',
    api: 'openai-completions',
    baseUrl: 'https://generativelanguage.googleapis.com/v1beta/openai',
    apiKey: 'GEMINI_API_KEY',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.75, output: 3.75, cacheRead: 0.075 }
  },
  {
    id: 'xai/grok-4.7',
    region: 'intl',
    provider: 'xai',
    model: 'grok-4.7',
    api: 'openai-completions',
    baseUrl: 'https://api.x.ai/v1',
    apiKey: 'XAI_API_KEY',
    contextWindow: 500000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 2, output: 6, cacheRead: 0.5, tiers: [{ above: 199_999, input: 4, output: 12, cacheRead: 1 }] }
  },
  {
    id: 'xai/grok-build-0.1',
    region: 'intl',
    provider: 'xai',
    model: 'grok-build-0.1',
    api: 'openai-completions',
    baseUrl: 'https://api.x.ai/v1',
    apiKey: 'XAI_API_KEY',
    contextWindow: 256000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1, output: 2, cacheRead: 0.2, tiers: [{ above: 199_999, input: 2, output: 4, cacheRead: 0.4 }] }
  },
  // opencode Console (opencode.ai) — a hosted tokenhub; needs only an OPENCODE_API_KEY
  // from https://opencode.ai/console (no opencode install required). Catalog fetched
  // 2026-11-18 from https://opencode.ai/inference/v1/models (workspace-enabled set).
  // Console translates protocols: Claude via anthropic /v1/messages, everything else
  // via openai /v1/chat/completions (incl. responses-native GPT/Grok/Muse models).
  // The 12 zero-cost "free" catalog models are NOT preset: they are harness-locked
  // (HTTP 429 FreeUsageLimitError from any non-OpenCode client). glm-5.3-flash-free
  // is also deprecated (HTTP 410). Paid models need Console credits (402 when empty).
  {
    id: 'opencode/claude-fable-5-1',
    platform: 'opencode',
    provider: 'opencode',
    model: 'claude-fable-5-1',
    api: 'anthropic-messages',
    baseUrl: 'https://opencode.ai/inference/anthropic/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 10, output: 50, cacheRead: 0.25, cacheWrite: 12.5 }
  },
  {
    id: 'opencode/claude-haiku-5-5',
    platform: 'opencode',
    provider: 'opencode',
    model: 'claude-haiku-5-5',
    api: 'anthropic-messages',
    baseUrl: 'https://opencode.ai/inference/anthropic/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.1, output: 0.5, cacheRead: 0.01, cacheWrite: 0.125, tiers: [{ above: 100000, input: 0.5, cacheRead: 0.05, cacheWrite: 0.625, output: 2.5 }] }
  },
  {
    id: 'opencode/claude-opus-5-5',
    platform: 'opencode',
    provider: 'opencode',
    model: 'claude-opus-5-5',
    api: 'anthropic-messages',
    baseUrl: 'https://opencode.ai/inference/anthropic/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 4, output: 20, cacheRead: 0.2, cacheWrite: 5 }
  },
  {
    id: 'opencode/claude-sonnet-5-5',
    platform: 'opencode',
    provider: 'opencode',
    model: 'claude-sonnet-5-5',
    api: 'anthropic-messages',
    baseUrl: 'https://opencode.ai/inference/anthropic/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 2, output: 10, cacheRead: 0.1, cacheWrite: 2.5 }
  },
  {
    id: 'opencode/deepseek-v4.1-flash',
    platform: 'opencode',
    provider: 'opencode',
    model: 'deepseek-v4.1-flash',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.3, output: 1.2, cacheRead: 0.006 }
  },
  {
    id: 'opencode/deepseek-v4-pro',
    platform: 'opencode',
    provider: 'opencode',
    model: 'deepseek-v4-pro',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1.74, output: 3.48, cacheRead: 0.145 }
  },
  {
    id: 'opencode/gemini-3.5-flash-lite',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gemini-3.5-flash-lite',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.3, output: 2.5, cacheRead: 0.03 }
  },
  {
    id: 'opencode/gemini-3.8-flash',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gemini-3.8-flash',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.75, output: 3.75, cacheRead: 0.075 }
  },
  {
    id: 'opencode/gemini-4-argon',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gemini-4-argon',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 2, output: 10, cacheRead: 0.1 }
  },
  {
    id: 'opencode/glm-5.3',
    platform: 'opencode',
    provider: 'opencode',
    model: 'glm-5.3',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1.4, output: 4.4, cacheRead: 0.26 }
  },
  {
    id: 'opencode/glm-5.3-flash',
    platform: 'opencode',
    provider: 'opencode',
    model: 'glm-5.3-flash',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.15, output: 0.5, cacheRead: 0.03 }
  },
  {
    id: 'opencode/gpt-5.3-codex',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-5.3-codex',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 400000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1.75, output: 14, cacheRead: 0.175 }
  },
  {
    id: 'opencode/gpt-5.3-codex-spark',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-5.3-codex-spark',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 128000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1.75, output: 14, cacheRead: 0.175 }
  },
  {
    id: 'opencode/gpt-5.4-mini',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-5.4-mini',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 400000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.75, output: 4.5, cacheRead: 0.075 }
  },
  {
    id: 'opencode/gpt-5.4-nano',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-5.4-nano',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 400000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.2, output: 1.25, cacheRead: 0.02 }
  },
  {
    id: 'opencode/gpt-5.5',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-5.5',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1050000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 5, output: 30, cacheRead: 0.5, tiers: [{ above: 272000, input: 10, cacheRead: 1, output: 45 }] }
  },
  {
    id: 'opencode/gpt-5.5-pro',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-5.5-pro',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1050000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 30, output: 180, cacheRead: 30 }
  },
  {
    id: 'opencode/gpt-6-astra',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-6-astra',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1050000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 10, output: 50, cacheRead: 1, cacheWrite: 12.5, tiers: [{ above: 272000, input: 20, cacheRead: 2, cacheWrite: 25, output: 75 }] }
  },
  {
    id: 'opencode/gpt-6.1-sol',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-6.1-sol',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1050000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 2, output: 10, cacheRead: 0.1, cacheWrite: 2.5, tiers: [{ above: 272000, input: 4, cacheRead: 0.2, cacheWrite: 5, output: 15 }] }
  },
  {
    id: 'opencode/gpt-6-luna',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-6-luna',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1050000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.1, output: 0.5, cacheRead: 0.01, cacheWrite: 0.125, tiers: [{ above: 272000, input: 0.2, cacheRead: 0.02, cacheWrite: 0.25, output: 0.75 }] }
  },
  {
    id: 'opencode/gpt-5.6-terra',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-5.6-terra',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1050000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 2, output: 12, cacheRead: 0.2, cacheWrite: 2.5, tiers: [{ above: 272000, input: 4, cacheRead: 0.4, cacheWrite: 5, output: 18 }] }
  },
  {
    id: 'opencode/grok-4.7',
    platform: 'opencode',
    provider: 'opencode',
    model: 'grok-4.7',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 500000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 2, output: 6, cacheRead: 0.5, tiers: [{ above: 200000, input: 4, cacheRead: 1, output: 12 }] }
  },
  {
    id: 'opencode/grok-build-0.1',
    platform: 'opencode',
    provider: 'opencode',
    model: 'grok-build-0.1',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 256000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1, output: 2, cacheRead: 0.2 }
  },
  {
    id: 'opencode/kimi-k2.7-code',
    platform: 'opencode',
    provider: 'opencode',
    model: 'kimi-k2.7-code',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 262144,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.95, output: 4, cacheRead: 0.19 }
  },
  {
    id: 'opencode/kimi-k3',
    platform: 'opencode',
    provider: 'opencode',
    model: 'kimi-k3',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3 }
  },
  {
    id: 'opencode/minimax-m3',
    platform: 'opencode',
    provider: 'opencode',
    model: 'minimax-m3',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 512000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.3, output: 1.2, cacheRead: 0.06 }
  },
  {
    id: 'opencode/muse-wonder-1.0',
    platform: 'opencode',
    provider: 'opencode',
    model: 'muse-wonder-1.0',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 4, output: 20, cacheRead: 0.2 }
  },
  {
    id: 'opencode/mistral-large-4',
    platform: 'opencode',
    provider: 'opencode',
    model: 'mistral-large-4',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 524288,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.68, output: 2.09, cacheRead: 0.07 }
  },
  {
    id: 'opencode/qwen3.8-flash',
    platform: 'opencode',
    provider: 'opencode',
    model: 'qwen3.8-flash',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.15, output: 0.47, cacheRead: 0.016, cacheWrite: 0.2 }
  },
  {
    id: 'opencode/muse-ember-1.0',
    platform: 'opencode',
    provider: 'opencode',
    model: 'muse-ember-1.0',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 0.1, output: 0.5, cacheRead: 0.004, tiers: [{ above: 200000, input: 0.3, cacheRead: 0.012, output: 1.5 }] }
  },
  {
    id: 'opencode/claude-fable-5-2',
    platform: 'opencode',
    provider: 'opencode',
    model: 'claude-fable-5-2',
    api: 'anthropic-messages',
    baseUrl: 'https://opencode.ai/inference/anthropic/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1000000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 8, output: 40, cacheRead: 0.2, cacheWrite: 10 }
  },
  {
    id: 'opencode/gemini-4-carbon',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gemini-4-carbon',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 2, output: 10, cacheRead: 0.1 }
  },
  {
    id: 'opencode/gemini-4-hydrogen',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gemini-4-hydrogen',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1048576,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 1.5, output: 7.5, cacheRead: 0.075 }
  },
  {
    id: 'opencode/gpt-6.2-sol',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-6.2-sol',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1050000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 2, output: 10, cacheRead: 0.1, cacheWrite: 2.5, tiers: [{ above: 272000, input: 4, cacheRead: 0.2, cacheWrite: 5, output: 15 }] }
  },
  {
    id: 'opencode/gpt-6.2-astra',
    platform: 'opencode',
    provider: 'opencode',
    model: 'gpt-6.2-astra',
    api: 'openai-completions',
    baseUrl: 'https://opencode.ai/inference/openai/v1',
    apiKey: 'OPENCODE_API_KEY',
    region: 'intl',
    contextWindow: 1050000,
    maxTokens: 32768,
    reasoning: true,
    cost: { input: 8, output: 40, cacheRead: 0.4, cacheWrite: 10, tiers: [{ above: 272000, input: 16, cacheRead: 0.8, cacheWrite: 20, output: 60 }] }
  },
];

// Vision-capable families: the flagship multimodal lines (Claude, Gemini, GPT consumer
// flagships, Grok-4.7, qwen4-max). Coding-specialized variants (codex) and text-only
// families (deepseek, etc.) stay unmarked. Unmarked ≠ blocked: images still send with a
// heads-up notice; users can override per-model via ~/.tango/models.json (`"vision": true`).
const VISION_MODEL = /^(claude-|gemini-|grok-4\.7$|gpt-(?!.*codex)|qwen4-max$)/;
for (const p of presets) if (VISION_MODEL.test(p.model)) p.vision = true;

/** Source platform (tokenhub) a model is reached through; falls back to provider. */
export function platformOf(m: ModelSpec): string {
  return m.platform ?? m.provider;
}

/** A zero-cost model (input AND output priced 0). Unknown cost is NOT free. */
export function isFreeModel(m: ModelSpec): boolean {
  return !!m.cost && m.cost.input === 0 && m.cost.output === 0;
}

/** Display platform: zero-cost models collect into the virtual "free" platform, whatever their source. */
export function displayPlatform(m: ModelSpec): string {
  return isFreeModel(m) ? 'free' : platformOf(m);
}

/** Group models for display: virtual "free" first, then source platforms in first-appearance order. */
export function groupModels(models: ModelSpec[]): [string, ModelSpec[]][] {
  const groups = new Map<string, ModelSpec[]>();
  for (const m of models) {
    const p = displayPlatform(m);
    let list = groups.get(p);
    if (!list) groups.set(p, (list = []));
    list.push(m);
  }
  const out: [string, ModelSpec[]][] = [];
  const free = groups.get('free');
  if (free?.length) out.push(['free', free]);
  for (const [p, list] of groups) if (p !== 'free') out.push([p, list]);
  return out;
}

/** Pick the tier from total prompt tokens (u.input), then bill every token in the request at it. */
export function costUsd(spec: ModelSpec, u: Usage): number {
  if (!spec.cost) return 0;
  let p: Prices = spec.cost;
  for (const t of spec.cost.tiers ?? []) if (u.input > t.above) p = t;
  const read = u.cacheRead ?? 0;
  const write = u.cacheWrite ?? 0;
  const fresh = Math.max(0, u.input - read - write);
  return (
    (fresh * p.input + read * (p.cacheRead ?? p.input) + write * (p.cacheWrite ?? p.input) + u.output * p.output) /
    1_000_000
  );
}
