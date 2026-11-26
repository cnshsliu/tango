/**
 * In-process remote MCP server for tests. mode "http": streamable HTTP at /mcp (JSON replies, or
 * SSE replies when sseReplies), with an Mcp-Session-Id. mode "sse": legacy HTTP+SSE (GET /mcp opens
 * the stream, POST /messages?session=… answers on it; POST /mcp gets 405, like old servers).
 * Every request's headers are recorded.
 */
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import type { AddressInfo } from 'node:net';

export async function startMcpHttp(mode: 'http' | 'sse', opts: { sseReplies?: boolean } = {}) {
  const seen: { method: string; url: string; headers: IncomingMessage['headers']; body?: any }[] = [];
  let stream: ServerResponse | undefined;
  const answer = (msg: any) => {
    if (msg.id === undefined) return undefined; // notification
    if (msg.method === 'initialize')
      return { jsonrpc: '2.0', id: msg.id, result: { protocolVersion: '2025-03-26', capabilities: { tools: {} }, serverInfo: { name: 'mock-http', version: '1' } } };
    if (msg.method === 'tools/list')
      return { jsonrpc: '2.0', id: msg.id, result: { tools: [{ name: 'echo', description: 'Echo back', inputSchema: { type: 'object', properties: { message: { type: 'string' } } } }] } };
    if (msg.method === 'tools/call')
      return { jsonrpc: '2.0', id: msg.id, result: { content: [{ type: 'text', text: `echo: ${msg.params.arguments.message}` }] } };
    return { jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: 'no such method' } };
  };
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (c) => (raw += c));
    req.on('end', () => {
      const body = raw ? JSON.parse(raw) : undefined;
      seen.push({ method: req.method!, url: req.url!, headers: req.headers, body });
      if (req.headers.authorization !== 'Bearer t0ken') return void res.writeHead(401).end('unauthorized');
      if (mode === 'http' && req.url === '/mcp' && req.method === 'POST') {
        const reply = answer(body);
        const headers: Record<string, string> = { 'mcp-session-id': 'sess-42' };
        if (!reply) return void res.writeHead(202, headers).end();
        if (opts.sseReplies) {
          res.writeHead(200, { ...headers, 'content-type': 'text/event-stream' });
          return void res.end(`event: message\ndata: ${JSON.stringify(reply)}\n\n`);
        }
        return void res.writeHead(200, { ...headers, 'content-type': 'application/json' }).end(JSON.stringify(reply));
      }
      if (mode === 'http' && req.method === 'DELETE') return void res.writeHead(200).end();
      if (mode === 'sse' && req.url === '/mcp' && req.method === 'GET') {
        stream = res;
        res.writeHead(200, { 'content-type': 'text/event-stream' });
        return void res.write('event: endpoint\ndata: /messages?session=abc\n\n');
      }
      if (mode === 'sse' && req.url?.startsWith('/messages') && req.method === 'POST') {
        const reply = answer(body);
        res.writeHead(202).end('Accepted');
        if (reply) stream?.write(`event: message\ndata: ${JSON.stringify(reply)}\n\n`);
        return;
      }
      res.writeHead(405).end('method not allowed');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/mcp`;
  return {
    url,
    seen,
    close: () => {
      stream?.end();
      server.closeAllConnections();
      server.close();
    }
  };
}
