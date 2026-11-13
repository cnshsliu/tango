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

/** Yield the `data:` payloads of an SSE byte stream. */
export async function* sseLines(body: ReadableStream<Uint8Array>): AsyncIterable<string> {
  const reader = body.getReader();
  const dec = new TextDecoder();
  let buf = '';
  while (true) {
    const { value, done } = await reader.read();
    if (done) break;
    buf += dec.decode(value, { stream: true });
    let i: number;
    while ((i = buf.indexOf('\n')) >= 0) {
      const line = buf.slice(0, i).replace(/\r$/, '');
      buf = buf.slice(i + 1);
      if (line.startsWith('data:')) yield line.slice(5).trim();
    }
  }
}
