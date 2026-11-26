/**
 * Secrets the user keeps never enter the conversation through a tool's output. tango can name
 * them: [tango]/env, the Feishu webhook in the config, a model's literal API key, an MCP server's
 * credentials, and environment variables whose name says they hold one. Wherever one of those
 * values shows up in what a tool returned (a printed config, a curl of an API, a grep through old
 * notes), it is replaced by `[REDACTED <name>]` before the result is shown, stored or sent to the
 * model. The model loses nothing it may have: to use a credential it has the shell read it.
 *
 * Limits, by design: only whole values (16+ characters, so ordinary text is never hit), only
 * tool output (not what the user types or the model writes), and nothing already in a session file.
 */
import { readFile } from 'node:fs/promises';
import { join } from 'node:path';
import { type Config, loadConfig, loadModels, tangoDir } from './config.js';
import type { ImageBlock, TextBlock } from './types.js';

export interface Secret {
  /** what the value is called in the placeholder, e.g. `APIKEY_FAN_API_KEY` */
  name: string;
  value: string;
}

/** Shorter strings occur in ordinary text by coincidence, so they are left alone. */
export const MIN_SECRET_LENGTH = 16;

/** An environment variable whose name alone says it holds a credential (not `SSH_KEY_PATH`, not `SSH_AUTH_SOCK`). */
const SECRET_NAME = /(^|_)(API_?KEY|TOKEN|SECRET|PASS(WORD|WD)?|CREDENTIALS?|WEBHOOK|PAT|ACCESS_KEY|PRIVATE_KEY)(_|$)/i;
/** A model's `apiKey` that looks like this names an environment variable instead of holding the key. */
const ENV_NAME = /^[A-Z][A-Z0-9_]*$/;

/** Where the values can come from; `knownSecrets` fills this in from the machine, tests pass their own. */
export interface SecretSources {
  /** text of [tango]/env */
  envFile?: string;
  config?: { feishuWebhook?: string; mcpServers?: Record<string, { env?: Record<string, string>; headers?: Record<string, string> }> };
  models?: { id: string; apiKey?: string }[];
  /** the process environment */
  env?: Record<string, string | undefined>;
}

/** `KEY=VALUE` lines of an env file, quoted or not, `# comments` skipped: the same reading as `loadEnvFile`. */
export function parseEnvFile(text: string): [string, string][] {
  const out: [string, string][] = [];
  for (const line of text.split('\n')) {
    if (line.trim().startsWith('#')) continue;
    const m = /^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$/.exec(line);
    if (!m) continue;
    let v = m[2];
    if ((v.startsWith('"') && v.endsWith('"')) || (v.startsWith("'") && v.endsWith("'"))) v = v.slice(1, -1);
    out.push([m[1], v]);
  }
  return out;
}

/**
 * Every secret named by these sources, longest value first (when one secret contains another, the
 * longer one is replaced first). A value must be 16+ characters; `Authorization: Bearer <key>` is
 * split on whitespace so the key alone counts; file paths and `${VAR}` placeholders are not secrets.
 */
export function collectSecrets(src: SecretSources): Secret[] {
  const byValue = new Map<string, string>(); // value → name; the first name a value was given wins
  const add = (value: unknown, name: string) => {
    if (typeof value !== 'string') return;
    for (const part of value.trim().split(/\s+/)) {
      if (part.length < MIN_SECRET_LENGTH || part.startsWith('${') || part[0] === '/' || part[0] === '~') continue;
      if (!byValue.has(part)) byValue.set(part, name);
    }
  };
  for (const [name, value] of parseEnvFile(src.envFile ?? '')) add(value, name);
  if (src.config?.feishuWebhook) add(src.config.feishuWebhook, 'FEISHU_WEBHOOK');
  for (const [server, spec] of Object.entries(src.config?.mcpServers ?? {})) {
    for (const [k, v] of Object.entries(spec.env ?? {})) add(v, `mcp ${server} ${k}`);
    for (const [k, v] of Object.entries(spec.headers ?? {})) add(v, `mcp ${server} ${k}`);
  }
  for (const m of src.models ?? []) if (m.apiKey && !ENV_NAME.test(m.apiKey)) add(m.apiKey, `${m.id} apiKey`);
  for (const [name, value] of Object.entries(src.env ?? {})) if (SECRET_NAME.test(name)) add(value, name);
  return [...byValue].map(([value, name]) => ({ name, value })).sort((a, b) => b.value.length - a.value.length);
}

/** `text` with every secret replaced by `[REDACTED <name>]`; the same string when none occurs. */
export function redactText(text: string, secrets: Secret[]): string {
  let out = text;
  for (const { name, value } of secrets) {
    // the value itself, and the value as it appears inside a JSON string (e.g. `cat config.json`)
    const forms = [value];
    const escaped = JSON.stringify(value).slice(1, -1);
    if (escaped !== value) forms.push(escaped);
    let next = out;
    for (const form of forms) if (next.includes(form)) next = next.split(form).join(`[REDACTED ${name}]`);
    out = next;
  }
  return out;
}

let cache: { at: number; secrets: Secret[] } | undefined;

/** Every secret this tango can name right now; read at most every five minutes. Empty when `redactSecrets` is off. */
export async function knownSecrets(): Promise<Secret[]> {
  if (cache && Date.now() - cache.at < 5 * 60_000) return cache.secrets;
  const [cfg, models, envFile] = await Promise.all([
    loadConfig().catch(() => ({} as Config)),
    loadModels().catch(() => []),
    readFile(join(tangoDir(), 'env'), 'utf8').catch(() => '')
  ]);
  const secrets = cfg.redactSecrets === false ? [] : collectSecrets({ envFile, config: cfg, models, env: process.env });
  cache = { at: Date.now(), secrets };
  return secrets;
}

/** Forget the cached list, so the next `knownSecrets` reads the files again (config changed; tests). */
export function resetSecretCache(): void {
  cache = undefined;
}

/** Text blocks of a tool result with the secrets replaced; other blocks (images) as they are. */
export async function redactContent(content: (TextBlock | ImageBlock)[]): Promise<(TextBlock | ImageBlock)[]> {
  const secrets = await knownSecrets();
  if (!secrets.length) return content;
  let changed = false;
  const out = content.map((b) => {
    if (b.type !== 'text' || !secrets.some((s) => b.text.includes(s.value))) return b;
    changed = true;
    return { ...b, text: redactText(b.text, secrets) };
  });
  return changed ? out : content;
}
