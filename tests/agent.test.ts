import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { Agent } from '../src/core/agent.js';
import { SessionStore } from '../src/core/session.js';
import { ToolRegistry } from '../src/core/tools/registry.js';
import { bashTool } from '../src/core/tools/bash.js';

const chunk = (delta: object, finish: string | null = null) =>
  `data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;

test('agent runs model → tool → model over an OpenAI-compatible SSE endpoint', async () => {
  const bodies: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      const body = JSON.parse(raw);
      bodies.push(body);
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      if (bodies.length === 1) {
        res.write(chunk({ tool_calls: [{ index: 0, id: 'c1', function: { name: 'bash', arguments: '{"command":' } }] }));
        res.write(chunk({ tool_calls: [{ index: 0, function: { arguments: '"echo tango-ok"}' } }] }, 'tool_calls'));
      } else {
        res.write(chunk({ content: 'done' }, 'stop'));
      }
      res.write(`data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 10, completion_tokens: 5 } })}\n\n`);
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;
  const dir = await mkdtemp(join(tmpdir(), 'tango-agent-'));
  try {
    const store = await SessionStore.create(dir, dir);
    const tools = new ToolRegistry();
    tools.register(bashTool);
    const agent = new Agent({
      store,
      tools,
      cwd: dir,
      model: {
        id: 'fake/m',
        provider: 'fake',
        model: 'm',
        api: 'openai-completions',
        baseUrl: `http://127.0.0.1:${port}/v1`,
        apiKey: 'test',
        cost: { input: 1, output: 1 }
      }
    });
    const final = await agent.prompt('say hi');
    assert.equal(final.stopReason, 'stop');
    assert.equal(bodies.length, 2);
    const toolMsg = bodies[1].messages.find((m: any) => m.role === 'tool');
    assert.equal(toolMsg.tool_call_id, 'c1');
    assert.match(toolMsg.content, /tango-ok/);
    // user, assistant(tool_call), user(tool_result), assistant(final)
    assert.equal(store.messages().length, 4);
    assert.ok(agent.totalCostUsd > 0);
  } finally {
    server.close();
    await rm(dir, { recursive: true, force: true });
  }
});

test('openai messages: tool-call-only assistant turns omit content instead of sending null', async () => {
  const { toOpenAIMessages } = await import('../src/core/models/openai.js');
  const msgs = toOpenAIMessages('sys', [
    { role: 'assistant', content: [{ type: 'tool_call', id: 'c1', name: 'ls', input: {} }] },
    { role: 'assistant', content: [{ type: 'text', text: 'hi' }] }
  ] as any);
  assert.equal('content' in msgs[1], false);
  assert.equal(msgs[1].tool_calls.length, 1);
  assert.equal(msgs[2].content, 'hi');
});
