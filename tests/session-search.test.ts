import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const home = await mkdtemp(join(tmpdir(), 'tango-home-'));
process.env.TANGO_HOME = home;
const { sessionSearchTool } = await import('../src/core/tools/session-search.js');
const { buildContext } = await import('../src/core/context.js');

const dir = join(home, 'sessions');
await mkdir(dir, { recursive: true });
const line = (o: unknown) => JSON.stringify(o) + '\n';
const msg = (role: string, text: string, ts = '2026-11-20T09:53:00.000Z') =>
  line({ kind: 'message', ts, message: { role, content: [{ type: 'text', text }] } });

await writeFile(
  join(dir, 'old.jsonl'),
  line({ kind: 'meta', id: 'old', createdAt: '2026-11-18T00:00:00.000Z', cwd: '/p' }) +
    msg('user', '我的 claude code 用了智谱的 coding plan 的 key') +
    msg('assistant', 'Coding Plan 的端点是 /api/coding/paas/v4')
);
await writeFile(
  join(dir, 'cur.jsonl'),
  line({ kind: 'meta', id: 'cur', createdAt: '2026-11-20T00:00:00.000Z', cwd: '/p' }) + msg('user', '我们聊过智谱 coding plan 吗')
);
await writeFile(join(dir, 'other.jsonl'), line({ kind: 'meta', id: 'other', createdAt: '2026-11-19T00:00:00.000Z', cwd: '/q' }) + msg('user', 'fix the tui'));

const run = (input: any) =>
  sessionSearchTool.execute(input, { cwd: '/p', signal: new AbortController().signal, sessionId: 'cur' });
const out = (r: any) => r.content[0].text as string;

test('finds past sessions, skips the current one', async () => {
  const r = out(await run({ query: '智谱|zhipu|coding plan' }));
  assert.match(r, /^old /);
  assert.match(r, /2 matching entries/);
  assert.match(r, /topic: 我的 claude code/);
  assert.doesNotMatch(r, /\bcur\b/);
  assert.doesNotMatch(r, /\bother\b/);
});

test('invalid regex falls back to literal; no match is reported', async () => {
  assert.match(out(await run({ query: 'paas/v4(' })), /no past session/);
  assert.match(out(await run({ query: '/api/coding/paas' })), /^old /);
});

test('cwd filter', async () => {
  assert.match(out(await run({ query: 'coding', cwd: '/q' })), /no past session/);
});

test('system prompt points at session_search for earlier conversations', async () => {
  const { systemPrompt } = await buildContext('/p');
  assert.match(systemPrompt, /session_search/);
  assert.match(systemPrompt, /我们聊过/);
});
