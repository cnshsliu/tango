import type { ModelSpec } from '../types.js';

export function resolveApiKey(spec: ModelSpec): string {
  const raw = spec.apiKey;
  if (raw?.startsWith('$')) {
    const v = process.env[raw.slice(1)];
    if (!v) throw new Error(`${spec.id}: environment variable ${raw.slice(1)} is not set`);
    return v;
  }
  if (raw) return raw;
  throw new Error(`${spec.id}: no apiKey configured`);
}

/** No bytes for this long while streaming means the connection is dead. */
export const STREAM_IDLE_MS = 180_000;

/**
 * POST JSON and return the response. Network failures get the underlying cause
 * (DNS, refused, TLS, proxy) instead of Node's bare "fetch failed".
 */
export async function postJson(
  spec: ModelSpec,
  url: string,
  headers: Record<string, string>,
  body: unknown,
  signal?: AbortSignal
): Promise<Response> {
  let res: Response;
  try {
    res = await fetch(url, {
      method: 'POST',
      headers: { 'content-type': 'application/json', ...headers },
      body: JSON.stringify(body),
      signal
    });
  } catch (e: any) {
    if (e?.name === 'AbortError') throw e;
    throw new Error(`${spec.id}: cannot reach ${new URL(url).host}: ${describeNetError(e)}`);
  }
  if (!res.ok || !res.body) {
    const text = (await res.text().catch(() => '')).slice(0, 2000);
    throw new Error(`${spec.id}: HTTP ${res.status} ${text}`.trim());
  }
  return res;
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
