import { existsSync } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { homedir } from 'node:os';
import { join } from 'node:path';
import type { ModelSpec } from './types.js';
import { presets } from './models/registry.js';
import { resolveApiKey } from './models/util.js';

/** Everything user-specific lives here (override with TANGO_HOME). */
export function tangoDir(): string {
  return process.env.TANGO_HOME ?? join(homedir(), '.tango');
}

export const sessionsDir = () => join(tangoDir(), 'sessions');

export interface Config {
  /** default model id, e.g. "deepseek/deepseek-flash" */
  model?: string;
  /** Feishu/Lark custom-bot webhook; used by the notify tool and scheduled tasks */
  feishuWebhook?: string;
  /** TUI palette: fixed dark/light or auto (OSC 11 background query, then COLORFGBG) */
  theme?: 'dark' | 'light' | 'auto';
  /** Mouse capture; false keeps native terminal selection. Missing means true. */
  mouse?: boolean;
  /** TUI motion (pulse, rule sweep, thinking wave). Missing means true. */
  animations?: boolean;
  /** TUI ASCII mode: emoji and the thinking icon drawn as plain ASCII. Missing means false. */
  ascii?: boolean;
  /** TUI copy path: auto (OSC 52 over SSH, else native, then OSC 52), osc52 or native. Missing means auto. */
  clipboard?: 'auto' | 'osc52' | 'native';
  /** User-message style: true = right-positioned bubble (default), false = full-width left. */
  bubble?: boolean;
  /** Input editing mode; missing/unknown values use the default Emacs-like editing */
  editor?: 'default' | 'vim';
  /** TUI action → key binding, e.g. { "expandTools": "ctrl+u" } */
  keys?: Record<string, string>;
  /** Neovim viewer preference; standard Vim is never used */
  nvimViewer?: 'ask' | 'readonly' | 'internal';
  /** Per-turn workspace snapshots for time travel; missing means on */
  checkpoint?: 'on' | 'off';
  /** Auto-compact threshold as a window share; 'off'/0 disables. Missing means 0.75. */
  autocompact?: number | 'off';
  /** Max model round-trips per prompt (runaway tool-loop fuse). 1-500, default 64. */
  maxTurns?: number;
  /** Retries for a model request that fails before any output (network error, 429, 5xx). 0-10, default 10. */
  retries?: number;
  /** Keep provider prompt caches warm between prompts. Off unless set. */
  warming?: boolean | {
    /** seconds between warming requests; "auto" = 240s (no per-model TTL data yet) */
    interval?: number | 'auto';
    /** seconds of inactivity after the last real request during which warming runs */
    duration?: number;
  };
  /** Custom status bar template with {model} {mode} {cost} {ctx} {pct} {session} {tokens}; missing = default bar */
  statusline?: string;
  /** Persistent allow rules: tool:<name> | path:<prefix> | bash:<prefix> */
  permissions?: { allow: string[] };
  /** Extra write roots outside cwd (/add_dir) */
  extraDirs?: string[];
  /**
   * Hooks. Legacy beforeTool/afterTool: sh -c, call JSON on stdin, never block.
   * Lifecycle (see core/hooks.ts): PreToolUse / PostToolUse / UserPromptSubmit / Stop,
   * arrays of { matcher?, command, timeout? }; exit 2 blocks.
   */
  hooks?: { beforeTool?: string; afterTool?: string } & Record<string, unknown>;
  /** Role-based model routing (/agents): driver / subagent / longContext */
  /** plan: the model plan mode switches to (and back from) */
  roles?: { driver?: string; subagent?: string; longContext?: string; plan?: string };
  /** Named sub-agents for the `task` tool: { name, description?, instruction?, model?, readonly? } */
  subAgents?: { name: string; description?: string; instruction?: string; model?: string; readonly?: boolean }[];
  /** MCP servers (stdio JSON-RPC); tools bridge in as mcp_<server>_<tool> */
  mcpServers?: Record<string, import('./mcp.js').McpServerSpec>;
}

async function readJson<T>(path: string): Promise<T | undefined> {
  if (!existsSync(path)) return;
  try {
    return JSON.parse(await readFile(path, 'utf8')) as T;
  } catch (e: any) {
    throw new Error(`${path}: invalid JSON — ${e.message}`);
  }
}

export async function loadConfig(): Promise<Config> {
  return (await readJson<Config>(join(tangoDir(), 'config.json'))) ?? {};
}

/**
 * `~/.tango/config.json` is created readable by its owner only (mode 600): it can hold a webhook
 * URL. The mode applies when the file is first written; one that already exists keeps whatever
 * mode its owner gave it.
 */
export async function saveConfig(cfg: Config): Promise<void> {
  await mkdir(tangoDir(), { recursive: true });
  await writeFile(join(tangoDir(), 'config.json'), JSON.stringify(cfg, null, 2) + '\n', { encoding: 'utf8', mode: 0o600 });
}

/**
 * ~/.tango/env holds KEY=VALUE lines (same file the task daemon sources).
 * Loaded once before key resolution; real environment variables always win.
 */
let envFileLoaded = false;
async function loadEnvFile(): Promise<void> {
  if (envFileLoaded) return;
  envFileLoaded = true;
  let text: string;
  try {
    text = await readFile(join(tangoDir(), 'env'), 'utf8');
  } catch {
    return; // no env file
  }
  for (const line of text.split('\n')) {
    if (line.trim().startsWith('#')) continue;
    const m = /^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$/.exec(line);
    if (!m) continue;
    let v = m[2];
    if ((v.startsWith('"') && v.endsWith('"')) || (v.startsWith("'") && v.endsWith("'"))) v = v.slice(1, -1);
    if (process.env[m[1]] === undefined) process.env[m[1]] = v;
  }
}

/** Built-in presets + ~/.tango/models.json; user entries win on id collision. */
export async function loadModels(): Promise<ModelSpec[]> {
  await loadEnvFile();
  const byId = new Map(presets.map((p) => [p.id, p]));
  const user = await readJson<ModelSpec[]>(join(tangoDir(), 'models.json'));
  for (const m of user ?? []) byId.set(m.id, { ...byId.get(m.id), ...m });
  return [...byId.values()];
}

export function hasKey(spec: ModelSpec): boolean {
  try {
    resolveApiKey(spec);
    return true;
  } catch {
    return false;
  }
}

/** Why a model can't be used right now, or undefined when it can. */
export function keyProblem(spec: ModelSpec): string | undefined {
  try {
    resolveApiKey(spec);
  } catch (e: any) {
    return e.message;
  }
}

/**
 * Do the characters of `needle` appear in `haystack` in order (case-insensitive,
 * separators skipped nowhere — a plain subsequence)? "glm54" matches
 * "zhipu-plan/glm-5.4"; "opusfan" matches "apikeyfan/claude-opus-5-5".
 */
export function fuzzyMatch(haystack: string, needle: string): boolean {
  if (!needle) return true;
  let i = 0;
  for (const c of haystack.toLowerCase()) {
    if (c === needle[i]) {
      i++;
      if (i === needle.length) return true;
    }
  }
  return false;
}

/**
 * Resolve a model: exact id, then a fuzzy match on the search string — every
 * whitespace-separated word must appear in the id as a subsequence (so
 * "glm54" finds zhipu-plan/glm-5.4, "zp glm5.4" too) — else config default,
 * else first model whose API key exists.
 */
export function pickModel(models: ModelSpec[], cfg: Config, wanted?: string): ModelSpec {
  const q = wanted ?? cfg.model;
  if (q) {
    const exact = models.find((m) => m.id === q);
    if (exact) return exact;
    const words = q.toLowerCase().split(/\s+/).filter(Boolean);
    const hit = (id: string) => words.every((w) => fuzzyMatch(id, w.toLowerCase()));
    const hits = models.filter((m) => hit(m.id));
    if (hits.length === 1) return hits[0];
    // rank what matched best (an id that contains the whole query verbatim beats
    // a scattered subsequence) so a short query still lands on the obvious one
    const ranked = [...hits].sort((a, b) => Number(b.id.toLowerCase().includes(q.toLowerCase())) - Number(a.id.toLowerCase().includes(q.toLowerCase())));
    if (ranked.length > 1) throw new Error(`ambiguous model "${q}": ${ranked.slice(0, 6).map((m) => m.id).join(', ')}${ranked.length > 6 ? ' …' : ''}`);
    const near = models.filter((m) => hit(m.id.split('/').pop() ?? m.id)).map((m) => m.id).slice(0, 6);
    throw new Error(`unknown model "${q}"${near.length ? `. Did you mean: ${near.join(', ')}?` : ''}. Run \`tango --models\` to list what is available.`);
  }
  const usable = models.find(hasKey);
  if (usable) return usable;
  throw new Error(
    'no model configured and no API key found in the environment.\n' +
      'Set one (e.g. export DEEPSEEK_API_KEY=sk-...) or create ~/.tango/models.json.\n' +
      'Run `tango --models` to see all options.'
  );
}
