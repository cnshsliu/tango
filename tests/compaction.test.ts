import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { Agent, COMPACT_KEEP_TOKENS, keptTail, messageTokens, splitPoint } from '../src/core/agent.js';
import { SessionStore } from '../src/core/session.js';
import { ToolRegistry } from '../src/core/tools/registry.js';
import type { Message } from '../src/core/types.js';

const user = (t: string): Message => ({ role: 'user', content: [{ type: 'text', text: t }] });
const asst = (t: string): Message => ({ role: 'assistant', content: [{ type: 'text', text: t }] });
const call = (id: string): Message => ({ role: 'assistant', content: [{ type: 'tool_call', id, name: 'bash', input: { command: 'ls' } }] });
const result = (id: string, t: string): Message => ({ role: 'user', content: [{ type: 'tool_result', toolCallId: id, content: [{ type: 'text', text: t }] }] });
const big = (tokens: number) => 'x'.repeat(Math.round(tokens * 3.5));
const textOf = (m: Message) => m.content.map((b: any) => b.text ?? '').join('');

test('keptTail keeps whole recent turns within the budget and cuts at a prompt', () => {
  const msgs = [user(big(5000)), asst('a'), user('q2'), call('c1'), result('c1', big(3000)), asst('b'), user('q3'), asst('c')];
  // budget covers turns q2 and q3, not q1
  assert.equal(keptTail(msgs, 4000), 6);
  // budget only covers q3; the cut never lands on the tool_result (a user message too)
  assert.equal(keptTail(msgs, 100), 2);
  // nothing fits
  assert.equal(keptTail([user(big(500))], 100), 0);
});

test('splitPoint halves one oversized turn at an assistant boundary', () => {
  const msgs = [user('do it'), call('c1'), result('c1', big(4000)), call('c2'), result('c2', big(4000)), asst('done')];
  const i = splitPoint(msgs);
  assert.equal(msgs[i].role, 'assistant');
  assert.equal(i, 3, 'kept part opens with the second tool call');
  const keptTok = msgs.slice(i).reduce((n, m) => n + messageTokens(m), 0);
  assert.ok(keptTok < msgs.reduce((n, m) => n + messageTokens(m), 0));
});

test('store: keep re-injects the verbatim tail after the summary, and across a second compaction', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-cmp-'));
  try {
    const store = await SessionStore.create(dir, dir);
    for (const m of [user('q1'), asst('a1'), user('q2'), asst('a2')]) await store.appendMessage(m);
    await store.compact('S1', 2);
    let ctx = store.messages();
    assert.deepEqual(ctx.map(textOf), ['<summary of earlier conversation>\nS1\n</summary>', 'q2', 'a2']);
    assert.equal(store.entriesOf('compaction')[0].droppedCount, 2);

    await store.appendMessage(user('q3'));
    await store.appendMessage(asst('a3'));
    // context is [S1, q2, a2, q3, a3]; keep 3 → a2, q3, a3 (a2 came from S1's kept tail)
    await store.compact('S2', 3);
    ctx = store.messages();
    assert.deepEqual(ctx.slice(1).map(textOf), ['a2', 'q3', 'a3']);
    assert.equal(store.lastSummary(), 'S2');

    // keep larger than what was in context never resurrects summarized messages
    await store.compact('S3', 50);
    assert.deepEqual(store.messages().slice(1).map(textOf), ['a2', 'q3', 'a3']);

    // old entries without keep still mean "everything summarized"
    const reopened = await SessionStore.open(dir, store.id);
    assert.equal(reopened.messages().length, 4);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

/** fake summarizer: records each request text and answers with a numbered summary */
async function withSummarizer(fn: (port: number, seen: string[]) => Promise<void>) {
  const seen: string[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      const body = JSON.parse(raw);
      const flat = (c: any) => (typeof c === 'string' ? c : c.map((p: any) => p.text ?? '').join(''));
      seen.push(body.messages.map((m: any) => flat(m.content)).join('\n'));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      const delta = { content: `## Goal\nSUMMARY-${seen.length}` };
      res.write(`data: ${JSON.stringify({ choices: [{ index: 0, delta, finish_reason: 'stop' }] })}\n\n`);
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  try {
    await fn((server.address() as AddressInfo).port, seen);
  } finally {
    server.close();
  }
}

test('agent.compact: summarizes only the older part, keeps the recent tail, updates incrementally', async () => {
  await withSummarizer(async (port, seen) => {
    const dir = await mkdtemp(join(tmpdir(), 'tango-cmp-'));
    try {
      const store = await SessionStore.create(dir, dir);
      const agent = new Agent({
        store, tools: new ToolRegistry(), cwd: dir,
        model: { id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 't' }
      });
      // an old turn bigger than the keep budget, then two small recent turns
      for (const m of [user('OLD-' + big(COMPACT_KEEP_TOKENS)), asst('old answer'), user('recent q'), call('c1'), result('c1', 'R'.repeat(5000)), asst('recent a')])
        await store.appendMessage(m);
      assert.equal(await agent.compact(), true);
      assert.match(seen[0], /OLD-/);
      assert.doesNotMatch(seen[0], /recent q/, 'the kept tail is not sent to the summarizer');
      assert.match(seen[0], /## Goal, ## Constraints, ## Progress, ## Key Decisions/);
      assert.doesNotMatch(seen[0], /previous summary/);
      let ctx = store.messages();
      assert.match(textOf(ctx[0]), /SUMMARY-1/);
      assert.deepEqual(ctx.slice(1).map((m) => m.role), ['user', 'assistant', 'user', 'assistant']);
      assert.equal(textOf(ctx[1]), 'recent q');

      // more work, then a second compaction pushes the first tail out and updates SUMMARY-1
      for (const m of [user('NEW-' + big(COMPACT_KEEP_TOKENS)), asst('new answer'), user('latest q'), asst('latest a')])
        await store.appendMessage(m);
      assert.equal(await agent.compact(), true);
      assert.match(seen[1], /<previous summary>\n## Goal\nSUMMARY-1/);
      assert.match(seen[1], /Write an updated summary/);
      assert.match(seen[1], /recent q/);
      assert.ok(!seen[1].includes('R'.repeat(2001)), 'tool results are cut to 2000 chars');
      ctx = store.messages();
      assert.match(textOf(ctx[0]), /SUMMARY-2/);
      assert.deepEqual(ctx.slice(1).map(textOf), ['latest q', 'latest a']);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

test('agent.compact: one oversized turn is split, its first half summarized', async () => {
  await withSummarizer(async (port, seen) => {
    const dir = await mkdtemp(join(tmpdir(), 'tango-cmp-'));
    try {
      const store = await SessionStore.create(dir, dir);
      const agent = new Agent({
        store, tools: new ToolRegistry(), cwd: dir,
        model: { id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 't' }
      });
      const chunkTok = COMPACT_KEEP_TOKENS / 2;
      for (const m of [user('one big task'), call('c1'), result('c1', 'FIRST-' + big(chunkTok)), call('c2'), result('c2', 'SECOND-' + big(chunkTok)), call('c3'), result('c3', 'THIRD-' + big(chunkTok))])
        await store.appendMessage(m);
      assert.equal(await agent.compact(), true);
      assert.match(seen[0], /one big task/);
      const ctx = store.messages();
      assert.equal(ctx[1].role, 'assistant', 'kept part starts at an assistant tool call');
      // every kept tool_result has its tool_call in context
      const calls = new Set(ctx.flatMap((m) => m.content.filter((b: any) => b.type === 'tool_call').map((b: any) => b.id)));
      for (const m of ctx) for (const b of m.content as any[]) if (b.type === 'tool_result') assert.ok(calls.has(b.toolCallId));
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});

test('agent.compact: a small conversation (manual /compact) keeps only the latest turn', async () => {
  await withSummarizer(async (port, seen) => {
    const dir = await mkdtemp(join(tmpdir(), 'tango-cmp-'));
    try {
      const store = await SessionStore.create(dir, dir);
      const agent = new Agent({
        store, tools: new ToolRegistry(), cwd: dir,
        model: { id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 't' }
      });
      for (const m of [user('q1'), asst('a1'), user('q2'), asst('a2')]) await store.appendMessage(m);
      assert.equal(await agent.compact(), true);
      assert.match(seen[0], /q1/);
      assert.doesNotMatch(seen[0], /q2/);
      assert.deepEqual(store.messages().slice(1).map(textOf), ['q2', 'a2']);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  });
});
