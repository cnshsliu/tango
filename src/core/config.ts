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
  /** TUI palette: fixed dark/light or terminal COLORFGBG detection */
  theme?: 'dark' | 'light' | 'auto';
  /** Mouse capture; false keeps native terminal selection. Missing means true. */
  mouse?: boolean;
  /** Input editing mode; missing/unknown values use the default Emacs-like editing */
  editor?: 'default' | 'vim';
  /** TUI action → key binding, e.g. { "expandTools": "ctrl+u" } */
  keys?: Record<string, string>;
  /** Neovim viewer preference; standard Vim is never used */
  nvimViewer?: 'ask' | 'readonly';
  /** Per-turn workspace snapshots for time travel; missing means on */
  checkpoint?: 'on' | 'off';
  /** Auto-compact threshold as a window share; 'off'/0 disables. Missing means 0.75. */
  autocompact?: number | 'off';
  /** Custom status bar template with {model} {mode} {cost} {ctx} {pct} {session} {tokens}; missing = default bar */
  statusline?: string;
  /** Persistent allow rules: tool:<name> | path:<prefix> | bash:<prefix> */
  permissions?: { allow: string[] };
  /** Extra write roots outside cwd (/add_dir) */
  extraDirs?: string[];
  /** Tool-call hooks: sh -c <cmd>, the call JSON arrives on stdin; failures never block */
  hooks?: { beforeTool?: string; afterTool?: string };
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

export async function saveConfig(cfg: Config): Promise<void> {
  await mkdir(tangoDir(), { recursive: true });
  await writeFile(join(tangoDir(), 'config.json'), JSON.stringify(cfg, null, 2) + '\n', 'utf8');
}

/** Built-in presets + ~/.tango/models.json; user entries win on id collision. */
export async function loadModels(): Promise<ModelSpec[]> {
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
 * Resolve a model: exact id, then single id match on the search string,
 * else config default, else first model whose API key exists.
 */
export function pickModel(models: ModelSpec[], cfg: Config, wanted?: string): ModelSpec {
  const q = wanted ?? cfg.model;
  if (q) {
    const exact = models.find((m) => m.id === q);
    if (exact) return exact;
    const hits = models.filter((m) => m.id.includes(q));
    if (hits.length === 1) return hits[0];
    if (hits.length > 1) throw new Error(`ambiguous model "${q}": ${hits.map((m) => m.id).join(', ')}`);
    throw new Error(`unknown model "${q}". Run \`tango --models\` to list what is available.`);
  }
  const usable = models.find(hasKey);
  if (usable) return usable;
  throw new Error(
    'no model configured and no API key found in the environment.\n' +
      'Set one (e.g. export DEEPSEEK_API_KEY=sk-...) or create ~/.tango/models.json.\n' +
      'Run `tango --models` to see all options.'
  );
}
