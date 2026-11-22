import { spawnSync } from 'node:child_process';
import { appendFileSync, existsSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { dirname, isAbsolute, join, resolve } from 'node:path';
import { loadConfig, tangoDir, type Config } from './config.js';

/**
 * Layered config, lowest first: global ~/.tango/config.json, project shared
 * <repo>/.tango/config.json (committed), project personal <repo>/.tango/config.local.json
 * (git-excluded). Scalars: the nearer layer wins. Lists (permissions.allow, extraDirs, hook
 * events) add up. Some keys come only from the global file; keys that run code or widen
 * access wait until the user trusts the project.
 */
export type Scope = 'global' | 'project' | 'local';

/** keys a project file cannot set at all */
export const GLOBAL_ONLY = ['feishuWebhook'] as const;
/** keys that run commands or widen access: a project layer needs trust for these */
export const NEEDS_TRUST = ['hooks', 'permissions', 'mcpServers', 'extraDirs'] as const;

export interface ConfigSource {
  scope: Scope;
  path: string;
  exists: boolean;
  /** keys this layer set that were dropped, with why */
  ignored: { key: string; why: 'global-only' | 'untrusted' }[];
  /** the file exists but is not valid JSON: the layer is skipped, not fatal */
  error?: string;
}

export interface ProjectInfo {
  /** where the shared file lives: the git toplevel, or the cwd outside git */
  root: string;
  /** main checkout (worktrees share it): the personal file and the trust key */
  localRoot: string;
  /** recorded decision; undefined = never asked */
  trusted: boolean | undefined;
  /** trust-gated keys present in a layer that needs trust */
  gated: string[];
  /** undecided and something is gated: ask the user */
  needsTrust: boolean;
}

export interface Resolved {
  config: Config;
  sources: ConfigSource[];
  project?: ProjectInfo;
}

const git = (cwd: string, ...args: string[]) => {
  const r = spawnSync('git', args, { cwd, encoding: 'utf8' });
  return r.status === 0 ? r.stdout.trim() : '';
};

const real = (p: string) => {
  try {
    return realpathSync(p);
  } catch {
    return resolve(p);
  }
};

/**
 * The project around `cwd`: shared file at the git toplevel (or cwd outside git), personal
 * file and trust at the main checkout, so every worktree of a repo shares them.
 * None when the project's .tango would be the global dir itself (cwd = home).
 */
export function projectRoots(cwd: string): { root: string; localRoot: string } | undefined {
  const top = git(cwd, 'rev-parse', '--show-toplevel');
  const root = real(top || cwd);
  const common = top ? git(cwd, 'rev-parse', '--path-format=absolute', '--git-common-dir') : '';
  const localRoot = common ? real(dirname(common)) : root;
  const global = real(tangoDir());
  if (real(join(root, '.tango')) === global || real(join(localRoot, '.tango')) === global) return;
  return { root, localRoot };
}

export const projectFile = (root: string) => join(root, '.tango', 'config.json');
export const localFile = (localRoot: string) => join(localRoot, '.tango', 'config.local.json');
const trustFile = () => join(tangoDir(), 'trusted.json');

function readTrust(): Record<string, { trusted: boolean; at: string }> {
  try {
    return JSON.parse(readFileSync(trustFile(), 'utf8'));
  } catch {
    return {};
  }
}

export function trustOf(localRoot: string): boolean | undefined {
  return readTrust()[localRoot]?.trusted;
}

/** Record the user's decision for a project (keyed by its main checkout). */
export function setTrust(localRoot: string, trusted: boolean): void {
  const all = readTrust();
  all[localRoot] = { trusted, at: new Date().toISOString() };
  mkdirSync(tangoDir(), { recursive: true });
  writeFileSync(trustFile(), JSON.stringify(all, null, 2) + '\n', 'utf8');
}

const LEGACY_HOOKS = ['beforeTool', 'afterTool'];
const union = (a: string[] = [], b: string[] = []) => [...new Set([...a, ...b])];

/** Fold `layer` over `base`: nearer scalars win, lists add up, maps merge by key. */
export function mergeLayer(base: Config, layer: Config, layerRoot?: string): Config {
  const out: Config = { ...base };
  for (const [k, v] of Object.entries(layer) as [keyof Config, any][]) {
    if (v === undefined) continue;
    switch (k) {
      case 'permissions':
        out.permissions = { allow: union(base.permissions?.allow, Array.isArray(v?.allow) ? v.allow.map(String) : []) };
        break;
      case 'extraDirs': {
        // relative dirs in a project file mean "relative to that project"
        const dirs = Array.isArray(v) ? v.map((d: unknown) => (layerRoot && !isAbsolute(String(d)) ? resolve(layerRoot, String(d)) : String(d))) : [];
        out.extraDirs = union(base.extraDirs, dirs);
        break;
      }
      case 'hooks': {
        const merged: Record<string, unknown> = { ...(base.hooks ?? {}) };
        for (const [ev, list] of Object.entries(v ?? {})) {
          const prev = merged[ev];
          merged[ev] = Array.isArray(prev) && Array.isArray(list) ? [...prev, ...list] : list;
        }
        out.hooks = merged as Config['hooks'];
        break;
      }
      case 'mcpServers':
      case 'keys':
      case 'roles':
        (out as any)[k] = { ...((base as any)[k] ?? {}), ...(v ?? {}) };
        break;
      default:
        (out as any)[k] = v;
    }
  }
  return out;
}

function readLayer(path: string): { cfg?: Config; error?: string } {
  if (!existsSync(path)) return {};
  try {
    const cfg = JSON.parse(readFileSync(path, 'utf8'));
    if (!cfg || typeof cfg !== 'object' || Array.isArray(cfg)) return { error: 'not a JSON object' };
    return { cfg };
  } catch (e: any) {
    return { error: `invalid JSON — ${e.message}` };
  }
}

/** Drop keys a project layer may not set (always) or may not set yet (until trusted). */
function strip(cfg: Config, trusted: boolean, src: ConfigSource): { kept: Config; gated: string[] } {
  const kept: any = { ...cfg };
  const gated: string[] = [];
  for (const k of GLOBAL_ONLY) {
    if (k in kept) {
      delete kept[k];
      src.ignored.push({ key: k, why: 'global-only' });
    }
  }
  if (kept.hooks && typeof kept.hooks === 'object') {
    kept.hooks = { ...kept.hooks };
    for (const k of LEGACY_HOOKS) {
      if (k in kept.hooks) {
        delete kept.hooks[k];
        src.ignored.push({ key: `hooks.${k}`, why: 'global-only' });
      }
    }
  }
  for (const k of NEEDS_TRUST) {
    if (!(k in kept)) continue;
    gated.push(k);
    if (!trusted) {
      delete kept[k];
      src.ignored.push({ key: k, why: 'untrusted' });
    }
  }
  return { kept, gated };
}

/** A personal file git tracks came with the repo, not from this user: it needs trust too. */
const tracked = (localRoot: string) =>
  spawnSync('git', ['ls-files', '--error-unmatch', '.tango/config.local.json'], { cwd: localRoot }).status === 0;

/** The effective config for a session in `cwd`, and where each part came from. */
export async function resolveConfig(cwd: string): Promise<Resolved> {
  const global = await loadConfig(); // a broken global file stays fatal, as before
  const sources: ConfigSource[] = [{ scope: 'global', path: join(tangoDir(), 'config.json'), exists: existsSync(join(tangoDir(), 'config.json')), ignored: [] }];
  const roots = projectRoots(cwd);
  if (!roots) return { config: global, sources };
  const trusted = trustOf(roots.localRoot);
  let config = global;
  const gated = new Set<string>();
  const layers: [Scope, string, string, boolean][] = [
    ['project', projectFile(roots.root), roots.root, true],
    ['local', localFile(roots.localRoot), roots.localRoot, tracked(roots.localRoot)]
  ];
  for (const [scope, path, layerRoot, needsTrust] of layers) {
    const src: ConfigSource = { scope, path, exists: existsSync(path), ignored: [] };
    sources.push(src);
    const { cfg, error } = readLayer(path);
    if (error) src.error = error;
    if (!cfg) continue;
    const { kept, gated: g } = strip(cfg, !needsTrust || trusted === true, src);
    if (needsTrust) g.forEach((k) => gated.add(k));
    config = mergeLayer(config, kept, layerRoot);
  }
  const project: ProjectInfo = { ...roots, trusted, gated: [...gated], needsTrust: trusted === undefined && gated.size > 0 };
  return { config, sources, project };
}

/** Keep the personal file out of commits: append it to the repo's .git/info/exclude once. */
function excludeLocalFile(localRoot: string): void {
  const common = git(localRoot, 'rev-parse', '--path-format=absolute', '--git-common-dir');
  if (!common) return;
  const ignored = spawnSync('git', ['check-ignore', '-q', '.tango/config.local.json'], { cwd: localRoot }).status === 0;
  if (ignored) return;
  const exclude = join(common, 'info', 'exclude');
  mkdirSync(dirname(exclude), { recursive: true });
  const prev = existsSync(exclude) ? readFileSync(exclude, 'utf8') : '';
  appendFileSync(exclude, `${prev && !prev.endsWith('\n') ? '\n' : ''}# tango personal project config\n/.tango/config.local.json\n`);
}

/** Read-modify-write the project's personal file; returns its path. Callers serialize. */
export function updateLocalConfig(localRoot: string, fn: (cfg: Config) => Config): string {
  const path = localFile(localRoot);
  const { cfg, error } = readLayer(path);
  if (error) throw new Error(`${path}: ${error}`);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, JSON.stringify(fn(cfg ?? {}), null, 2) + '\n', 'utf8');
  excludeLocalFile(localRoot);
  return path;
}

const SCOPE_LABEL: Record<Scope, string> = { global: '全局', project: '项目', local: '个人' };

/** One line for /status: which files loaded, what was dropped, the trust state. */
export function describeSources(r: Resolved): string {
  const parts = r.sources.map((s) => {
    const mark = s.error ? `✗ ${s.error}` : s.exists ? '✓' : '—';
    const dropped = s.ignored.length ? `（忽略 ${s.ignored.map((i) => i.key).join(', ')}）` : '';
    return `${SCOPE_LABEL[s.scope]} ${mark}${dropped}`;
  });
  if (r.project) {
    const t = r.project.trusted;
    parts.push(`信任 ${t === true ? '是' : t === false ? '否' : r.project.gated.length ? '待确认' : '—'}`);
  }
  return `配置：${parts.join(' · ')}`;
}
