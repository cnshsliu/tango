import { test } from 'node:test';
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { McpServer } from '../src/core/mcp.js';

const SPEC = { command: process.execPath, args: [join(import.meta.dirname, 'fixtures', 'mock-mcp-server.mjs')] };

test('MCP: lazy reconnect after the server dies; name collisions get suffixed', async () => {
  const s = new McpServer('demo', SPEC);
  await s.start();
  assert.ok(s.connected);
  const r = await s.callTool('echo', { message: 'first' });
  assert.match(r.text, /echo: first/);

  // kill the underlying server; connected drops
  (s as any).child.kill('SIGKILL');
  await new Promise((r2) => setTimeout(r2, 200));
  assert.ok(!s.connected);

  // a call transparently reconnects (lazy restart with re-handshake)
  const r2 = await s.callTool('echo', { message: 'second' });
  assert.ok(s.connected, 'reconnected');
  assert.match(r2.text, /echo: second/);
  s.stop();

  // collision suffixing: same tool name registered twice under different servers is the
  // serve bridge's job; here we verify the toolName helper used by the bridge
  const { bridgeName } = await import('../src/core/mcp.js');
  const taken = new Set<string>();
  const n1 = bridgeName('a', 'b_c', taken);
  const n2 = bridgeName('a_b', 'c', taken); // flattens to the same raw name → suffixed
  assert.equal(n1, 'mcp_a_b_c');
  assert.notEqual(n1, n2);
  assert.match(n2, /^mcp_a_b_c_\d+$/);
});
