import { test } from 'node:test';
import assert from 'node:assert/strict';
import { McpServer } from '../src/core/mcp.js';
import { expandHeaders } from '../src/core/mcp-remote.js';
import { startMcpHttp } from './fixtures/mock-mcp-http.js';

process.env.TANGO_TEST_MCP_TOKEN = 't0ken';
const headers = { Authorization: 'Bearer ${TANGO_TEST_MCP_TOKEN}' };

async function roundTrip(spec: object) {
  const s = new McpServer('remote', spec as any);
  await s.start();
  try {
    assert.ok(s.connected);
    assert.deepEqual((await s.listTools()).map((t) => t.name), ['echo']);
    return await s.callTool('echo', { message: 'hi' });
  } finally {
    s.stop();
  }
}

test('remote MCP over streamable HTTP: JSON and SSE replies, session id echoed, headers sent', async () => {
  for (const sseReplies of [false, true]) {
    const srv = await startMcpHttp('http', { sseReplies });
    try {
      assert.deepEqual(await roundTrip({ url: srv.url, headers, transport: 'http' }), { text: 'echo: hi', isError: false });
      const posts = srv.seen.filter((r) => r.method === 'POST');
      assert.ok(posts.every((r) => r.headers.authorization === 'Bearer t0ken'), 'headers on every request');
      assert.equal(posts[0].headers['mcp-session-id'], undefined, 'initialize has no session yet');
      assert.ok(posts.slice(1).every((r) => r.headers['mcp-session-id'] === 'sess-42'), 'later requests echo it');
    } finally {
      srv.close();
    }
  }
});

test('remote MCP over legacy SSE, and auto falls back to it when POST gets a 4xx', async () => {
  for (const transport of ['sse', 'auto', undefined]) {
    const srv = await startMcpHttp('sse');
    try {
      assert.deepEqual(await roundTrip({ url: srv.url, headers, transport }), { text: 'echo: hi', isError: false }, String(transport));
      assert.ok(srv.seen.some((r) => r.method === 'POST' && r.url === '/messages?session=abc'));
      // auto (the default) tried streamable HTTP first and fell back after the 405
      if (transport !== 'sse') assert.equal(srv.seen[0].method, 'POST');
    } finally {
      srv.close();
    }
  }
});

test('remote MCP: a bad token fails the connect with the HTTP status; headers need their env vars', async () => {
  const srv = await startMcpHttp('http');
  try {
    const s = new McpServer('remote', { url: srv.url, headers: { Authorization: 'Bearer wrong' }, transport: 'http' });
    await assert.rejects(s.start(), /HTTP 401/);
    assert.ok(!s.connected);
    s.stop();
  } finally {
    srv.close();
  }
  assert.throws(() => expandHeaders({ Authorization: 'Bearer ${TANGO_NO_SUCH_VAR}' }), /TANGO_NO_SUCH_VAR is not set/);
  assert.deepEqual(expandHeaders({ 'X-Plain': 'v' }), { 'X-Plain': 'v' });
});

test('serve: a remote MCP server bridges its tools; /mcp add writes the catalog entry', { timeout: 60_000 }, async () => {
  const { mkdtemp, readFile, rm, writeFile } = await import('node:fs/promises');
  const { tmpdir } = await import('node:os');
  const { join } = await import('node:path');
  const { spawn } = await import('node:child_process');
  const { createServer } = await import('node:http');
  const root = join(import.meta.dirname, '..');
  const home = await mkdtemp(join(tmpdir(), 'tango-remote-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-remote-work-'));
  // a mock model: one tool call, then a plain answer
  const model = createServer((req, res) => {
    let raw = '';
    req.on('data', (c) => (raw += c));
    req.on('end', () => {
      const call = JSON.parse(raw).messages.some((m: any) => m.role === 'tool');
      const delta = call
        ? { content: 'done' }
        : { tool_calls: [{ index: 0, id: 'c1', function: { name: 'mcp_ctx_echo', arguments: JSON.stringify({ message: 'REMOTE' }) } }] };
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.end(`data: ${JSON.stringify({ choices: [{ delta, finish_reason: call ? 'stop' : 'tool_calls' }] })}\n\ndata: [DONE]\n\n`);
    });
  });
  await new Promise<void>((r) => model.listen(0, '127.0.0.1', r));
  const remote = await startMcpHttp('http');
  try {
    await writeFile(join(home, 'models.json'), JSON.stringify([{ id: 'mock/mock', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${(model.address() as any).port}/v1`, apiKey: 'MOCK_SERVE_KEY', contextWindow: 8000 }]));
    await writeFile(join(home, 'config.json'), JSON.stringify({
      mcpServers: { ctx: { url: remote.url, headers: { Authorization: 'Bearer ${TANGO_TEST_MCP_TOKEN}' } } },
      permissions: { allow: ['tool:mcp_ctx_echo'] }
    }));
    const env: NodeJS.ProcessEnv = { ...process.env, TANGO_HOME: home, MOCK_SERVE_KEY: 'sk-test' };
    for (const k of ['TANGO_RESUME', 'TANGO_MODE', 'TANGO_MODEL']) delete env[k];
    const child = spawn(process.execPath, ['--import', 'tsx', 'src/cli.ts', 'serve', '-m', 'mock/mock', '--cwd', work], { cwd: root, env });
    let buf = '';
    const frames: any[] = [];
    const exited = new Promise<number>((r) => child.on('exit', r));
    child.stderr.on('data', (b: Buffer) => process.stderr.write(`[serve] ${b}`));
    child.stdout.on('data', (b: Buffer) => {
      buf += b.toString();
      let i: number;
      while ((i = buf.indexOf('\n')) >= 0) {
        const line = buf.slice(0, i);
        buf = buf.slice(i + 1);
        if (line.trim()) frames.push(JSON.parse(line));
      }
    });
    const waitFor = async (pred: (f: any) => boolean, what: string) => {
      for (let t = 0; t < 200; t++) {
        const f = frames.find(pred);
        if (f) return f;
        await new Promise((r) => setTimeout(r, 50));
      }
      throw new Error(`no frame: ${what}`);
    };
    const hello = await waitFor((f) => f.type === 'hello', 'hello');
    assert.ok(hello.tools.some((t: any) => t.name === 'mcp_ctx_echo'), 'remote tool bridged');
    child.stdin.write(JSON.stringify({ id: 1, cmd: 'prompt', text: 'go' }) + '\n');
    const toolResult = await waitFor((f) => f.type === 'tool_result' && f.call?.name === 'mcp_ctx_echo', 'tool result');
    assert.ok(String(toolResult.output).includes('echo: REMOTE'));
    await waitFor((f) => f.type === 'result' && f.id === 1, 'result');
    // every request the remote server saw carried the token from the environment
    assert.ok(remote.seen.length >= 3 && remote.seen.every((r) => r.headers.authorization === 'Bearer t0ken'));
    // /mcp add: the catalog entry lands in the global config
    child.stdin.write(JSON.stringify({ id: 2, cmd: 'mcp_add', name: 'context7' }) + '\n');
    const added = await waitFor((f) => f.type === 'result' && f.id === 2, 'mcp_add');
    assert.ok(added.ok, JSON.stringify(added));
    assert.deepEqual(JSON.parse(await readFile(join(home, 'config.json'), 'utf8')).mcpServers.context7, { url: 'https://mcp.context7.com/mcp' });
    assert.ok(JSON.parse(await readFile(join(home, 'config.json'), 'utf8')).mcpServers.ctx, 'existing servers kept');
    child.stdin.write(JSON.stringify({ id: 3, cmd: 'mcp_add', name: 'context7' }) + '\n');
    const twice = await waitFor((f) => f.type === 'result' && f.id === 3, 'mcp_add twice');
    assert.equal(twice.ok, false, 'not added twice');
    child.kill();
    await exited;
  } finally {
    remote.close();
    model.closeAllConnections();
    model.close();
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});
