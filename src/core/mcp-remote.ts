/**
 * Remote MCP transports. Streamable HTTP (MCP 2025-03-26): every JSON-RPC message is a POST to the
 * server URL; the reply is JSON or an SSE stream carrying it, and an `Mcp-Session-Id` header from
 * initialize is echoed on later requests. Legacy HTTP+SSE (2024-11-05): a GET opens an event
 * stream whose first `endpoint` event names the URL to POST to; replies arrive on that stream.
 * `auto` tries streamable HTTP and falls back to SSE when initialize gets a 4xx, as the spec says.
 */

export type RemoteKind = 'http' | 'sse' | 'auto';

/** `${VAR}` in a header value reads the environment, so tokens stay out of config files. */
export function expandHeaders(headers: Record<string, string> | undefined): Record<string, string> {
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(headers ?? {})) {
    out[k] = String(v).replace(/\$\{([A-Za-z_][A-Za-z0-9_]*)\}/g, (_, name) => {
      const val = process.env[name];
      if (val === undefined) throw new Error(`header ${k}: environment variable ${name} is not set`);
      return val;
    });
  }
  return out;
}

/** Yield each SSE event's (event, data) from a response body. */
export async function* sseEvents(body: ReadableStream<Uint8Array>): AsyncIterable<{ event: string; data: string }> {
  const reader = body.getReader();
  const dec = new TextDecoder();
  let buf = '';
  let event = 'message';
  let data: string[] = [];
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      buf += dec.decode(value, { stream: true });
      let i: number;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i).replace(/\r$/, '');
        buf = buf.slice(i + 1);
        if (line === '') {
          if (data.length) yield { event, data: data.join('\n') };
          event = 'message';
          data = [];
        } else if (line.startsWith('event:')) event = line.slice(6).trim();
        else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
      }
    }
    if (data.length) yield { event, data: data.join('\n') };
  } finally {
    reader.cancel().catch(() => {});
  }
}

export class HttpStatusError extends Error {
  constructor(readonly status: number, message: string) {
    super(message);
  }
}

/** One remote connection. Messages from the server (replies, also unsolicited ones) go to onMessage. */
export class RemoteTransport {
  private sessionId?: string;
  private postUrl?: string;
  private sse?: AbortController;
  private closed = false;
  kind: 'http' | 'sse';

  constructor(
    private url: string,
    private headers: Record<string, string>,
    kind: RemoteKind,
    private onMessage: (msg: any) => void,
    private onClose: (why: string) => void
  ) {
    this.kind = kind === 'sse' ? 'sse' : 'http';
  }

  get alive(): boolean {
    return !this.closed;
  }

  /** For SSE: open the stream and wait for the endpoint event. HTTP needs no setup. */
  async open(): Promise<void> {
    if (this.kind === 'sse') await this.openSse();
  }

  /** auto: a 4xx from a streamable-HTTP initialize means a legacy SSE server. */
  async fallBackToSse(): Promise<void> {
    this.kind = 'sse';
    this.sessionId = undefined;
    await this.openSse();
  }

  private async openSse(): Promise<void> {
    this.sse = new AbortController();
    // a server that never answers must not hold up startup: 15 s to open the stream
    const opening = setTimeout(() => this.sse?.abort(new Error(`GET ${this.url}: no response in 15s`)), 15_000);
    let res: Response;
    try {
      res = await fetch(this.url, { headers: { ...this.headers, accept: 'text/event-stream' }, signal: this.sse.signal });
    } finally {
      clearTimeout(opening);
    }
    if (!res.ok || !res.body) throw new HttpStatusError(res.status, `GET ${this.url}: HTTP ${res.status}`);
    const events = sseEvents(res.body)[Symbol.asyncIterator]();
    const first = await events.next();
    if (first.done || first.value.event !== 'endpoint') throw new Error(`${this.url}: the SSE stream did not start with an endpoint event`);
    this.postUrl = new URL(first.value.data.trim(), this.url).href;
    void (async () => {
      try {
        for (let e = await events.next(); !e.done; e = await events.next()) if (e.value.event === 'message') this.deliver(e.value.data);
        if (!this.closed) this.lost('the SSE stream ended');
      } catch (err: any) {
        if (!this.closed) this.lost(`the SSE stream failed: ${err?.message ?? err}`);
      }
    })();
  }

  private lost(why: string) {
    this.closed = true;
    this.onClose(why);
  }

  private deliver(data: string) {
    let msg: any;
    try {
      msg = JSON.parse(data);
    } catch {
      return;
    }
    for (const m of Array.isArray(msg) ? msg : [msg]) this.onMessage(m);
  }

  async send(msg: object, signal?: AbortSignal): Promise<void> {
    if (this.closed) throw new Error('connection closed');
    const target = this.kind === 'sse' ? this.postUrl! : this.url;
    const headers: Record<string, string> = { ...this.headers, 'content-type': 'application/json', accept: 'application/json, text/event-stream' };
    if (this.sessionId) headers['mcp-session-id'] = this.sessionId;
    const res = await fetch(target, { method: 'POST', headers, body: JSON.stringify(msg), signal });
    const sid = res.headers.get('mcp-session-id');
    if (sid) this.sessionId = sid;
    if (!res.ok) {
      const body = (await res.text().catch(() => '')).slice(0, 300);
      throw new HttpStatusError(res.status, `POST ${target}: HTTP ${res.status}${body ? ` ${body}` : ''}`);
    }
    // SSE transport: the reply comes on the GET stream; 202 / empty bodies carry nothing
    if (this.kind === 'sse' || res.status === 202 || !res.body) {
      await res.body?.cancel().catch(() => {});
      return;
    }
    const type = res.headers.get('content-type') ?? '';
    if (type.includes('text/event-stream')) {
      for await (const e of sseEvents(res.body)) if (e.event === 'message') this.deliver(e.data);
    } else {
      const text = await res.text();
      if (text.trim()) this.deliver(text);
    }
  }

  close(): void {
    this.closed = true;
    this.sse?.abort();
    // streamable HTTP: tell the server the session is over (best effort)
    if (this.kind === 'http' && this.sessionId)
      void fetch(this.url, { method: 'DELETE', headers: { ...this.headers, 'mcp-session-id': this.sessionId } }).catch(() => {});
  }
}
