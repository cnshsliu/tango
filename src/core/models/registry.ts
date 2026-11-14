import type { ApiKind, ModelAdapter, ModelSpec } from '../types.js';
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
 * Built-in presets. China-first: these work with a single API key, no proxy.
 * Users override or extend via ~/.tango/models.json.
 */
export const presets: ModelSpec[] = [
  {
    id: 'tknet/qwen3.8-flash-next',
    provider: 'tknet',
    model: 'qwen3.8-flash-next',
    api: 'openai-completions',
    baseUrl: 'https://tknet.ai/api/v1',
    apiKey: '$TKNET_API_KEY',
    contextWindow: 200000,
    maxTokens: 16384,
    cost: { input: 0.075, output: 0.235, cacheRead: 0.008 }
  },
  {
    id: 'tknet/deepseek-v4.1-flash',
    provider: 'tknet',
    model: 'deepseek-v4.1-flash',
    api: 'openai-completions',
    baseUrl: 'https://tknet.ai/api/v1',
    apiKey: '$TKNET_API_KEY',
    contextWindow: 1000000,
    maxTokens: 16384,
    cost: { input: 0.135, output: 0.54, cacheRead: 0.003 }
  },
  {
    id: 'tknet/deepseek-v4-pro',
    provider: 'tknet',
    model: 'deepseek-v4-pro',
    api: 'openai-completions',
    baseUrl: 'https://tknet.ai/api/v1',
    apiKey: '$TKNET_API_KEY',
    contextWindow: 128000,
    maxTokens: 16384,
    cost: { input: 0.6525, output: 1.305 }
  },
  {
    id: 'deepseek/deepseek-chat',
    provider: 'deepseek',
    model: 'deepseek-chat',
    api: 'openai-completions',
    baseUrl: 'https://api.deepseek.com/v1',
    apiKey: '$DEEPSEEK_API_KEY',
    contextWindow: 128000,
    maxTokens: 8192,
    cost: { input: 0.27, output: 1.1, cacheRead: 0.07 }
  },
  {
    id: 'deepseek/deepseek-reasoner',
    provider: 'deepseek',
    model: 'deepseek-reasoner',
    api: 'openai-completions',
    baseUrl: 'https://api.deepseek.com/v1',
    apiKey: '$DEEPSEEK_API_KEY',
    contextWindow: 128000,
    maxTokens: 16384,
    reasoning: true,
    cost: { input: 0.55, output: 2.19, cacheRead: 0.14 }
  },
  {
    id: 'kimi/kimi-k2',
    provider: 'kimi',
    model: 'kimi-k2-0905-preview',
    api: 'openai-completions',
    baseUrl: 'https://api.moonshot.cn/v1',
    apiKey: '$KIMI_API_KEY',
    contextWindow: 256000,
    maxTokens: 16384,
    cost: { input: 0.6, output: 2.5 }
  },
  {
    id: 'qwen/qwen3-coder-plus',
    provider: 'qwen',
    model: 'qwen3-coder-plus',
    api: 'openai-completions',
    baseUrl: 'https://dashscope.aliyuncs.com/compatible-mode/v1',
    apiKey: '$DASHSCOPE_API_KEY',
    contextWindow: 128000,
    maxTokens: 16384
  },
  {
    id: 'zhipu/glm-4.6',
    provider: 'zhipu',
    model: 'glm-4.6',
    api: 'openai-completions',
    baseUrl: 'https://open.bigmodel.cn/api/paas/v4',
    apiKey: '$Z_API_KEY',
    contextWindow: 200000,
    maxTokens: 16384
  },
  {
    id: 'anthropic/claude-sonnet-4-5',
    provider: 'anthropic',
    model: 'claude-sonnet-4-5',
    api: 'anthropic-messages',
    baseUrl: 'https://api.anthropic.com/v1',
    apiKey: '$ANTHROPIC_API_KEY',
    contextWindow: 200000,
    maxTokens: 16384,
    reasoning: true,
    cost: { input: 3, output: 15, cacheRead: 0.3, cacheWrite: 3.75 }
  },
  {
    id: 'openai/gpt-4.1',
    provider: 'openai',
    model: 'gpt-4.1',
    api: 'openai-completions',
    baseUrl: 'https://api.openai.com/v1',
    apiKey: '$OPENAI_API_KEY',
    contextWindow: 1000000,
    maxTokens: 32768,
    cost: { input: 2, output: 8, cacheRead: 0.5 }
  }
];

export function costUsd(spec: ModelSpec, u: { input: number; output: number; cacheRead?: number; cacheWrite?: number }) {
  const c = spec.cost;
  if (!c) return 0;
  const cached = u.cacheRead ?? 0;
  return (
    ((u.input - cached) * c.input +
      cached * (c.cacheRead ?? c.input) +
      u.output * c.output +
      (u.cacheWrite ?? 0) * (c.cacheWrite ?? 0)) /
    1_000_000
  );
}
