#!/usr/bin/env node
// Minimal MCP server for tests: one tool `echo` that returns its input, plus a `boom`
// tool that the client can use to test error propagation. Speaks newline-delimited
// JSON-RPC 2.0 over stdio (MCP transport).
import { stdin as input, stdout as output } from 'node:process';
import readline from 'node:readline';

const rl = readline.createInterface({ input });
const send = (obj) => output.write(JSON.stringify(obj) + '\n');

rl.on('line', (line) => {
  if (!line.trim()) return;
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return;
  }
  if (msg.method === 'initialize') {
    send({ jsonrpc: '2.0', id: msg.id, result: { protocolVersion: '2024-11-05', capabilities: { tools: {} }, serverInfo: { name: 'mock-mcp', version: '1.0.0' } } });
    return;
  }
  if (msg.method === 'notifications/initialized') return;
  if (msg.method === 'tools/list') {
    send({
      jsonrpc: '2.0', id: msg.id,
      result: {
        tools: [
          { name: 'echo', description: 'Echo the message back', inputSchema: { type: 'object', properties: { message: { type: 'string' } }, required: ['message'] } },
          { name: 'boom', description: 'Always fails', inputSchema: { type: 'object', properties: {} } }
        ]
      }
    });
    return;
  }
  if (msg.method === 'tools/call') {
    const tool = msg.params?.name;
    if (tool === 'echo') {
      send({ jsonrpc: '2.0', id: msg.id, result: { content: [{ type: 'text', text: `echo: ${msg.params?.arguments?.message ?? ''}` }] } });
    } else if (tool === 'boom') {
      send({ jsonrpc: '2.0', id: msg.id, result: { content: [{ type: 'text', text: 'boom failed' }], isError: true } });
    } else {
      send({ jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: `unknown tool ${tool}` } });
    }
    return;
  }
  if (msg.id !== undefined) send({ jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: 'method not found' } });
});
