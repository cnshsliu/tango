import type { ModelSpec } from '../types.js';
import { readFileSync } from 'node:fs';

/** What tango calls itself over HTTP. Gateways capacity-split by user agent
 * (Zhipu's anthropic endpoint 529s generic `node`/`python-requests` agents on
 * newly launched models while `tango/…` goes through), and a real name is
 * kinder to logs than an unconfigured runtime default. */
const TANGO_UA = (() => {
  try {
    return `tango/${JSON.parse(readFileSync(new URL('../../../package.json', import.meta.url), 'utf8')).version}`;
  } catch {
    return 'tango';
  }
})();

/** An apiKey written in UPPER_SNAKE (letters, digits, underscores) names an environment variable. */
const ENV_NAME = /^[A-Z_][A-Z0-9_]*$/;

/** The environment variable a spec reads its key from, or undefined for a literal key. */
export function keyEnvName(spec: ModelSpec): string | undefined {
  return spec.apiKey && ENV_NAME.test(spec.apiKey) ? spec.apiKey : undefined;
}

export function resolveApiKey(spec: ModelSpec): string {
  if (spec.apiKey?.startsWith('$'))
    throw new Error(`${spec.id}: apiKey "${spec.apiKey}": write the variable name without "$" (${spec.apiKey.slice(1)})`);
  const env = keyEnvName(spec);
  if (env) {
    const v = process.env[env];
    if (!v) throw new Error(`${spec.id}: environment variable ${env} is not set`);
    return v;
  }
  if (spec.apiKey) return spec.apiKey;
  throw new Error(`${spec.id}: no apiKey configured`);
}

/** No bytes for this long while streaming means the connection is dead. */
export const STREAM_IDLE_MS = 180_000;

/** default retries for a model request that fails before any output */
export const DEFAULT_RETRIES = 10;
/** retry n waits RETRY_BASE_MS * 2^(n-1), capped at RETRY_MAX_MS (1s, 2s, 4s, 8s, …) */
export const RETRY_BASE_MS = 1000;
const RETRY_MAX_MS = 15_000;

/** HTTP statuses worth retrying: rate limits, timeouts and gateway/upstream hiccups */
const RETRY_STATUS = new Set([408, 409, 425, 429, 500, 502, 503, 504, 529]);

export interface RetryOptions {
  retries?: number;
  onRetry?: (info: import('../types.js').RetryInfo) => void;
  /** test hook: base delay in ms */
  baseMs?: number;
}

/** Resolve the retry count: request > model spec > default, clamped to 0-10. */
export function retryCount(spec: ModelSpec, requested?: number): number {
  const n = requested ?? spec.retries ?? DEFAULT_RETRIES;
  return Number.isFinite(n) ? Math.max(0, Math.min(10, Math.floor(n))) : DEFAULT_RETRIES;
}

/** Sleep that an abort cuts short (rejects with AbortError). */
function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) return reject(Object.assign(new Error('aborted'), { name: 'AbortError' }));
    const t = setTimeout(() => {
      signal?.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(t);
      reject(Object.assign(new Error('aborted'), { name: 'AbortError' }));
    };
    signal?.addEventListener('abort', onAbort, { once: true });
  });
}

/** Retry-After in ms (seconds or an HTTP date), capped; undefined when absent or bad. */
function retryAfterMs(res: Response): number | undefined {
  const h = res.headers.get('retry-after');
  if (!h) return undefined;
  const secs = Number(h);
  const ms = Number.isFinite(secs) ? secs * 1000 : Date.parse(h) - Date.now();
  return Number.isFinite(ms) && ms >= 0 ? Math.min(ms, 60_000) : undefined;
}

/**
 * POST JSON and return the response. Network failures get the underlying cause
 * (DNS, refused, TLS, proxy) instead of Node's bare "fetch failed". A request that
 * fails before any output (network error, 429, 5xx) is retried up to `retries` times
 * with exponential backoff; `onRetry` is told before each wait. Auth and other 4xx
 * errors are not retried. Once the response is returned, streaming is not retried.
 */
export async function postJson(
  spec: ModelSpec,
  url: string,
  headers: Record<string, string>,
  body: unknown,
  signal?: AbortSignal,
  opts: RetryOptions = {}
): Promise<Response> {
  const max = retryCount(spec, opts.retries);
  const base = opts.baseMs ?? RETRY_BASE_MS;
  const host = new URL(url).host;
  for (let attempt = 0; ; attempt++) {
    let res: Response | undefined;
    let reason: string;
    let netError: Error | undefined;
    try {
      res = await fetch(url, {
        method: 'POST',
        headers: { 'content-type': 'application/json', 'user-agent': TANGO_UA, ...headers },
        body: JSON.stringify(body),
        signal
      });
    } catch (e: any) {
      if (e?.name === 'AbortError' || signal?.aborted) throw e;
      reason = describeNetError(e);
      netError = new Error(`${spec.id}: cannot reach ${host}: ${reason}`);
    }
    if (res && (res.ok && res.body)) return res;
    if (res && !RETRY_STATUS.has(res.status)) return fail(spec, url, res, attempt);
    if (attempt >= max) {
      if (netError) throw new Error(netError.message + (attempt ? ` (gave up after ${attempt} ${attempt === 1 ? 'retry' : 'retries'})` : ''));
      return fail(spec, url, res!, attempt);
    }
    reason = res ? `HTTP ${res.status}` : reason!;
    const wait = (res && retryAfterMs(res)) ?? Math.min(base * 2 ** attempt, RETRY_MAX_MS);
    await res?.body?.cancel().catch(() => {});
    opts.onRetry?.({ attempt: attempt + 1, max, reason, delayMs: wait });
    await sleep(wait, signal);
  }
}

async function fail(spec: ModelSpec, url: string, res: Response, retried: number): Promise<never> {
  const text = (await res.text().catch(() => '')).slice(0, 2000);
  const env = keyEnvName(spec);
  const host = new URL(url).host;
  const after = retried ? ` (gave up after ${retried} ${retried === 1 ? 'retry' : 'retries'})` : '';
  const hint =
    res.status === 401 || res.status === 403
      ? `\n${host} rejected the key ${env ? `from ${env}` : 'set in apiKey'}. ` +
        'China and international accounts use different keys; check the key matches this endpoint.'
      : res.status >= 500
        ? `\nthe provider failed upstream${after}. Try again later, or switch with /model.`
        : res.status === 429
          ? `\nrate limited${after}. Wait a moment, or switch with /model.`
          : '';
  throw new Error(`${spec.id}: HTTP ${res.status} ${text}`.trim() + hint);
}

export function describeNetError(e: any): string {
  const c = e?.cause ?? e;
  const code: string | undefined = c?.code;
  const hints: Record<string, string> = {
    ENOTFOUND: 'DNS lookup failed (offline, or a proxy is needed: set https_proxy)',
    EAI_AGAIN: 'DNS lookup timed out',
    ECONNREFUSED: 'connection refused',
    ECONNRESET: 'connection reset',
    ETIMEDOUT: 'connection timed out',
    UND_ERR_CONNECT_TIMEOUT: 'connection timed out',
    CERT_HAS_EXPIRED: 'TLS certificate expired',
    UNABLE_TO_VERIFY_LEAF_SIGNATURE: 'TLS certificate not trusted'
  };
  if (code && hints[code]) return `${hints[code]} [${code}]`;
  return [c?.message ?? String(c), code && `[${code}]`].filter(Boolean).join(' ');
}

/** Parse a tool-call argument string; returns the raw string when it is not a JSON object. */
export function parseArgs(raw: string): unknown {
  if (!raw.trim()) return {};
  try {
    return JSON.parse(raw);
  } catch {
    return raw;
  }
}

/** Tool input as sent back to the API: always an object, never a raw string. */
export function argsObject(input: unknown): Record<string, unknown> {
  return input && typeof input === 'object' && !Array.isArray(input) ? (input as Record<string, unknown>) : {};
}

/** Yield the `data:` payloads of an SSE byte stream; give up if the stream goes silent. */
export async function* sseLines(body: ReadableStream<Uint8Array>, idleMs = STREAM_IDLE_MS): AsyncIterable<string> {
  const reader = body.getReader();
  const dec = new TextDecoder();
  let buf = '';
  try {
    while (true) {
      let timer: NodeJS.Timeout | undefined;
      const idle = new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error(`stream stalled: no data for ${Math.round(idleMs / 1000)}s`)), idleMs);
      });
      let chunk: ReadableStreamReadResult<Uint8Array>;
      try {
        chunk = await Promise.race([reader.read(), idle]);
      } finally {
        clearTimeout(timer);
      }
      if (chunk.done) break;
      buf += dec.decode(chunk.value, { stream: true });
      let i: number;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i).replace(/\r$/, '');
        buf = buf.slice(i + 1);
        if (line.startsWith('data:')) yield line.slice(5).trim();
      }
    }
    const rest = buf.trim();
    if (rest.startsWith('data:')) yield rest.slice(5).trim();
  } finally {
    reader.cancel().catch(() => {});
  }
}

/** JSON.parse an SSE payload, with a readable error when a gateway sends something else. */
export function parseEvent(spec: ModelSpec, data: string): any {
  try {
    return JSON.parse(data);
  } catch {
    throw new Error(`${spec.id}: malformed stream event: ${data.slice(0, 200)}`);
  }
}

/** The line shown to the user before a retry, e.g. "⟳ 请求失败（HTTP 503），2 秒后第 1/10 次重试…". */
export function retryText(info: import('../types.js').RetryInfo): string {
  const secs = Math.max(1, Math.round(info.delayMs / 1000));
  return `⟳ 请求失败（${info.reason}），${secs} 秒后第 ${info.attempt}/${info.max} 次重试…`;
}
