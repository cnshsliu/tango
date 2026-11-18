import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { imageMimeFor, loadImages, MAX_IMAGE_BYTES } from '../src/core/images.js';
import { Agent } from '../src/core/agent.js';
import { SessionStore } from '../src/core/session.js';
import { ToolRegistry } from '../src/core/tools/registry.js';

// a real 1x1 transparent PNG
const PNG_BYTES = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
  'base64'
);

test('imageMimeFor maps extensions case-insensitively', () => {
  assert.equal(imageMimeFor('/a/b.PNG'), 'image/png');
  assert.equal(imageMimeFor('photo.jpeg'), 'image/jpeg');
  assert.equal(imageMimeFor('x.webp'), 'image/webp');
  assert.equal(imageMimeFor('notes.txt'), undefined);
  assert.equal(imageMimeFor('noext'), undefined);
});

test('loadImages reads files into base64 blocks with their path', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-img-'));
  try {
    const p = join(dir, 'shot.png');
    await writeFile(p, PNG_BYTES);
    const blocks = await loadImages([p], dir);
    assert.equal(blocks.length, 1);
    assert.equal(blocks[0].type, 'image');
    assert.equal(blocks[0].mimeType, 'image/png');
    assert.equal(blocks[0].path, p);
    assert.equal(Buffer.from(blocks[0].data, 'base64').equals(PNG_BYTES), true);
    // relative paths resolve against cwd
    const rel = await loadImages(['shot.png'], dir);
    assert.equal(rel[0].path, p);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('loadImages rejects missing, unsupported and oversized files', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-img-'));
  try {
    await assert.rejects(() => loadImages([join(dir, 'nope.png')], dir), /不存在/);
    const txt = join(dir, 'a.txt');
    await writeFile(txt, 'hi');
    await assert.rejects(() => loadImages([txt], dir), /不支持的图片格式/);
    const big = join(dir, 'big.png');
    await writeFile(big, Buffer.alloc(MAX_IMAGE_BYTES + 1));
    await assert.rejects(() => loadImages([big], dir), /图片过大/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

const chunk = (delta: object, finish: string | null = null) =>
  `data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: finish }] })}\n\n`;

test('agent.prompt uploads pasted images as image_url content', async () => {
  const bodies: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      bodies.push(JSON.parse(raw));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write(chunk({ content: '看到了' }, 'stop'));
      res.write(`data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 10, completion_tokens: 5 } })}\n\n`);
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;
  const dir = await mkdtemp(join(tmpdir(), 'tango-img-agent-'));
  try {
    const p = join(dir, 'shot.png');
    await writeFile(p, PNG_BYTES);
    const images = await loadImages([p], dir);
    const store = await SessionStore.create(dir, dir);
    const agent = new Agent({
      store,
      tools: new ToolRegistry(),
      cwd: dir,
      model: {
        id: 'fake/v', provider: 'fake', model: 'v', api: 'openai-completions',
        baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'test', vision: true,
        cost: { input: 1, output: 1 }
      }
    });
    const final = await agent.prompt('[图片 #1] 这是什么？', undefined, images);
    assert.equal(final.stopReason, 'stop');
    const userMsg = bodies[0].messages.find((m: any) => m.role === 'user');
    assert.ok(Array.isArray(userMsg.content));
    const img = userMsg.content.find((c: any) => c.type === 'image_url');
    assert.ok(img, 'image_url block present');
    assert.match(img.image_url.url, /^data:image\/png;base64,/);
    // the image is persisted in the session too
    const stored = store.messages()[0];
    assert.ok(stored.content.some((b) => b.type === 'image'));
  } finally {
    server.close();
    await rm(dir, { recursive: true, force: true });
  }
});

test('agent.supplement carries images into the running turn', async () => {
  const bodies: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      bodies.push(JSON.parse(raw));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      if (bodies.length === 1) {
        // first call asks for a tool so the loop runs again and flushes the supplement
        res.write(chunk({ tool_calls: [{ index: 0, id: 'c1', function: { name: 'noop', arguments: '{}' } }] }, 'tool_calls'));
      } else {
        res.write(chunk({ content: 'done' }, 'stop'));
      }
      res.write(`data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 10, completion_tokens: 5 } })}\n\n`);
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const port = (server.address() as AddressInfo).port;
  const dir = await mkdtemp(join(tmpdir(), 'tango-img-supp-'));
  try {
    const p = join(dir, 'shot.png');
    await writeFile(p, PNG_BYTES);
    const images = await loadImages([p], dir);
    const store = await SessionStore.create(dir, dir);
    const tools = new ToolRegistry();
    tools.register({
      name: 'noop', description: 'noop', input: { type: 'object', properties: {} },
      execute: async () => ({ content: [{ type: 'text', text: 'ok' }] })
    });
    const agent = new Agent({
      store, tools, cwd: dir,
      model: {
        id: 'fake/v', provider: 'fake', model: 'v', api: 'openai-completions',
        baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 'test', cost: { input: 1, output: 1 }
      }
    });
    // supplement lands before the second model call
    const pending = agent.prompt('开始');
    agent.supplement('补充一张图', images);
    await pending;
    const secondUser = bodies[1].messages.filter((m: any) => m.role === 'user').pop();
    assert.ok(JSON.stringify(secondUser).includes('image_url'), 'supplement image reached the next request');
    assert.ok(JSON.stringify(secondUser).includes('（补充）补充一张图'));
  } finally {
    server.close();
    await rm(dir, { recursive: true, force: true });
  }
});