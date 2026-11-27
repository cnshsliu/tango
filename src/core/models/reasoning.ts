import type { ModelSpec } from '../types.js';

/**
 * How hard a reasoning model thinks before it answers. The levels are the API's own names and they
 * differ from model to model — `low | medium | high` is only one of them:
 *
 *   claude-opus-5-5        low medium high xhigh max      (and a token budget before Claude 4.6)
 *   gpt-5.5-pro            medium high xhigh
 *   gpt-5.3-codex          none low medium high xhigh
 *   glm-5.4                low high max                   (thinking cannot be switched off)
 *   deepseek-v4-pro        low high max
 *   qwen3.8-max            low medium xhigh
 *   minimax-m3, kimi-k2.7  — (a switch only)
 *
 * So `/effort` offers what the current model takes, and an unknown level is refused instead of
 * being sent. Where the lists come from: an explicit `efforts` on the model spec (models.json), the
 * preset table in registry.ts (models.dev, provider by provider, checked against the live endpoint
 * where a key was at hand), and for a model nobody has described yet its name.
 */

/** What an unknown reasoning model is offered: the commonest set, and what tango always sent. */
export const DEFAULT_EFFORTS = ['low', 'medium', 'high'];

/** One line per level for the picker. */
export const EFFORT_NOTES: Record<string, string> = {
  none: '不思考',
  minimal: '几乎不思考',
  low: '轻度推理',
  medium: '中等推理',
  high: '增强推理',
  xhigh: '更强推理',
  max: '最深推理',
  ultra: '极致推理'
};

/** `low`, `medium`, … as sent; the picker shows the same names. */
export const effortNote = (level: string): string => EFFORT_NOTES[level] ?? '';

/** Which Claude this is: the family and the version, and whether it thinks adaptively. */
export interface ClaudeInfo {
  family: string;
  major: number;
  minor: number;
  /** Claude 4.6 and every Claude 5: `thinking.type: adaptive` plus `output_config.effort`. */
  modern: boolean;
}

export function claudeInfo(model: string): ClaudeInfo | undefined {
  const id = model.toLowerCase().replace(/^.*\//, '');
  const familyFirst = /(?:^|claude-)(opus|sonnet|haiku|fable|mythos)-(\d+)(?:[.-](\d+))?/.exec(id);
  const versionFirst = /claude-(\d+)(?:[.-](\d+))?-(opus|sonnet|haiku|fable|mythos)/.exec(id);
  const family = familyFirst?.[1] ?? versionFirst?.[3];
  if (!family) return undefined;
  const major = Number(familyFirst?.[2] ?? versionFirst?.[1]);
  const minor = Number(familyFirst?.[3] ?? versionFirst?.[2] ?? 0);
  return { family, major, minor, modern: major > 4 || (major === 4 && minor >= 6) };
}

/** The levels "switch thinking off" rather than setting a level. */
export const isEffortOff = (level: string): boolean => level === 'none' || level === 'minimal';

/** "none" and "minimal" mean the same thing to an endpoint that only knows one of them. */
const bothOff = (levels: string[]) => (levels.some(isEffortOff) ? [...levels.filter((l) => !isEffortOff(l)), 'none'] : levels);

function glmInfo(model: string): { major: number; minor: number } | undefined {
  const m = /^glm-(\d+)(?:[.-](\d+))?/.exec(model.toLowerCase().replace(/^.*\//, ''));
  return m ? { major: Number(m[1]), minor: Number(m[2] ?? 0) } : undefined;
}

/** Effort levels `model`'s endpoint accepts, in the order to offer them; empty when there are none to choose. */
export function effortsFor(model: Pick<ModelSpec, 'reasoning' | 'efforts'> & Partial<Pick<ModelSpec, 'model'>>): string[] {
  if (Array.isArray(model.efforts)) return model.efforts; // said explicitly, even when empty
  if (model.reasoning === false) return [];
  const id = (model.model ?? '').toLowerCase().replace(/^.*\//, '').replace(/:free$/, '');
  const claude = claudeInfo(id);
  if (claude) return claude.modern ? ['low', 'medium', 'high', 'xhigh', 'max'] : ['low', 'medium', 'high'];
  const glm = glmInfo(id);
  if (glm) {
    // GLM-5.3+: thinking is always on and only its depth is set; 5.2 also takes a few aliases; before that a switch only
    if (glm.major > 5 || (glm.major === 5 && glm.minor >= 3)) return ['low', 'high', 'max'];
    if (glm.major === 5 && glm.minor === 2) return bothOff(['none', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max']);
    return [];
  }
  if (/^gpt-5\.5-pro/.test(id)) return ['medium', 'high', 'xhigh'];
  if (/^gpt-5(\.\d+)?-codex/.test(id)) return ['none', 'low', 'medium', 'high', 'xhigh'];
  if (/^gpt-5/.test(id)) return ['none', 'low', 'medium', 'high', 'xhigh'];
  if (/^gpt-6/.test(id)) return ['low', 'medium', 'high', 'xhigh', 'max'];
  if (/^o[134](?:-|$)/.test(id)) return ['low', 'medium', 'high'];
  if (/^deepseek-(v4|flash)/.test(id)) return ['low', 'high', 'max'];
  if (/^deepseek/.test(id)) return [];
  if (/^kimi-k3/.test(id)) return ['low', 'high', 'max'];
  if (/^kimi/.test(id)) return [];
  if (/^grok-build/.test(id)) return [];
  if (/^grok/.test(id)) return ['low', 'medium', 'high', 'xhigh'];
  if (/^gemini/.test(id)) return ['low', 'medium', 'high'];
  if (/^qwen/.test(id)) return ['low', 'medium', 'xhigh'];
  if (/^minimax/.test(id)) return [];
  if (/^mistral-large-4/.test(id)) return ['low', 'medium', 'high'];
  return DEFAULT_EFFORTS;
}

/**
 * How a level is spelled on the Messages API.
 *
 * Claude 4.6 and the Claude 5 models think adaptively: `thinking.type: adaptive` with
 * `output_config.effort` — they reject the older `enabled`/`disabled` and its token budget.
 * Claude 3.7 and 4.0–4.5 know only the budget, so the level picks one.
 * Anything else served Anthropic-style (GLM on bigmodel.cn, Kimi, MiniMax) takes
 * `thinking.type: enabled` plus the same `output_config.effort`. Measured on GLM-5.4 and
 * GLM-5.3-Flash: that field halves or doubles the thinking tokens, while `budget_tokens`
 * (what tango used to send) and a top-level `reasoning_effort` change nothing.
 */
export function anthropicEffort(model: string, level: string, maxTokens: number): Record<string, unknown> {
  const off = isEffortOff(level);
  const claude = claudeInfo(model);
  if (claude && !claude.modern) {
    let budget = level === 'high' ? 16_000 : level === 'medium' ? 8_000 : 4_000;
    // the budget must leave room for the answer and never exceed max_tokens
    budget = Math.min(budget, Math.max(1024, maxTokens - 1024));
    return { max_tokens: Math.max(maxTokens, budget + 1024), ...(off ? {} : { thinking: { type: 'enabled', budget_tokens: budget } }) };
  }
  // adaptive thinking needs room: at least 2048 output tokens
  return {
    max_tokens: Math.max(maxTokens, 2048),
    thinking: { type: off ? 'disabled' : claude ? 'adaptive' : 'enabled' },
    ...(off ? {} : { output_config: { effort: level } })
  };
}

/**
 * How a level is spelled on a Chat Completions endpoint: `reasoning_effort`, nested under
 * `reasoning` on OpenRouter. DeepSeek and GLM want the switch set next to the level, Qwen (DashScope)
 * its own flag.
 */
export function openaiEffort(model: string, baseUrl: string, level: string): Record<string, unknown> {
  const id = model.toLowerCase().replace(/^.*\//, '');
  const off = isEffortOff(level);
  const out: Record<string, unknown> = {};
  if (/openrouter\.ai/.test(baseUrl)) out.reasoning = { effort: level };
  else out.reasoning_effort = level;
  if (/deepseek|glm/.test(id)) out.thinking = { type: off ? 'disabled' : 'enabled' };
  if (/qwen/.test(id)) out.enable_thinking = !off;
  return out;
}
