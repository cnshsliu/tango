import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';

/** A 200 whose body is not an SSE stream must surface as an error, not silence —
 * the zhipu coding-plan gateway answers wrong paths exactly that way. */
test('anthropic client rejects a 200 with a non-SSE body instead of going silent', async () => {
  const server = createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'application/json' });
    res.end('{"code":500,"msg":"404 NOT_FOUND","success":false}');
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as { port: number }).port;
  try {
    const { adapterFor } = await import('../src/core/models/registry.js');
    const adapter = adapterFor({
      id: 'test/gate',
      provider: 'test',
      model: 'm',
      api: 'anthropic-messages',
      baseUrl: `http://127.0.0.1:${port}`,
      apiKey: 'k',
      contextWindow: 1000,
      maxTokens: 100
    } as any);
    const events: unknown[] = [];
    const spec = {
      id: 'test/gate',
      provider: 'test',
      model: 'm',
      api: 'anthropic-messages',
      baseUrl: `http://127.0.0.1:${port}`,
      apiKey: 'k',
      contextWindow: 1000,
      maxTokens: 100
    } as any;
    const req = {
      messages: [{ role: 'user', content: [{ type: 'text', text: 'hi' }] }],
      tools: []
    } as any;
    await assert.rejects(
      async () => {
        for await (const ev of adapter.stream(spec, req)) {
          events.push(ev);
        }
      },
      /no SSE events|baseUrl|model/i,
      'a garbage 200 must throw, not end the stream silently'
    );
    assert.equal(events.length, 0);
  } finally {
    server.close();
  }
});
