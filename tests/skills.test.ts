import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { existsSync } from 'node:fs';
import { extractSteps, promoteDraft, saveDraft, slugify } from '../src/core/skills.js';
import type { Message } from '../src/core/types.js';

const home = await mkdtemp(join(tmpdir(), 'tango-home-'));
process.env.TANGO_HOME = home;

const user = (text: string): Message => ({ role: 'user', content: [{ type: 'text', text }] });
const asst = (blocks: Message['content']): Message => ({ role: 'assistant', content: blocks });

test('slugify keeps CJK, collapses junk', () => {
  assert.equal(slugify('  Daily News  Digest '), 'daily-news-digest');
  assert.equal(slugify('每日新闻 摘要!!'), '每日新闻-摘要');
  assert.equal(slugify('***'), 'skill');
});

test('extractSteps keeps user turns and tool calls, drops summaries', () => {
  const steps = extractSteps([
    user('把今天的 AI 新闻整理成摘要'),
    asst([{ type: 'tool_call', id: '1', name: 'web_search', input: { query: 'AI news' } } as any]),
    asst([{ type: 'text', text: '好了' }]),
    { role: 'user', content: [{ type: 'text', text: '<summary of earlier conversation>\nx\n</summary>' }] }
  ]);
  assert.match(steps[0], /^用户: 把今天的 AI 新闻整理成摘要$/);
  assert.match(steps[1], /^第 1 轮工具: web_search\(AI news\)$/);
  assert.equal(steps.length, 2);
});

test('saveDraft writes under TANGO_HOME/skill-drafts and refuses to clobber', async () => {
  const d = await saveDraft({ title: 'Weekly Report', messages: [user('写周报'), asst([{ type: 'tool_call', id: '1', name: 'read', input: { path: 'notes.md' } } as any])], cwd: '/tmp/proj' });
  assert.ok(d.file.startsWith(join(home, 'skill-drafts')));
  const body = await readFile(d.file, 'utf8');
  assert.match(body, /^---\nname: Weekly Report/);
  assert.match(body, /read\(notes\.md\)/);
  await assert.rejects(() => saveDraft({ title: 'weekly report', messages: [], cwd: '' }), /already exists/);
});

test('promoteDraft needs a real description, then lands in skills/<slug>/SKILL.md', async () => {
  const d = await saveDraft({ title: '新闻摘要', messages: [user('整理新闻')], cwd: '/p' });
  const raw = await readFile(d.file, 'utf8');
  const bad = d.file.replace('新闻摘要', 'bad-desc');
  const good = d.file.replace('新闻摘要', 'ok-desc');
  await writeFile(bad, raw, 'utf8');
  await assert.rejects(() => promoteDraft(bad, join(home, 'skills')), /fill in a real/);
  await writeFile(good, raw.replace(/^description: .*$/m, 'description: 每日 AI 新闻搜集与摘要流程'), 'utf8');
  const s = await promoteDraft(good, join(home, 'skills'));
  assert.equal(s.file, join(home, 'skills', '新闻摘要', 'SKILL.md'));
  assert.match(await readFile(s.file, 'utf8'), /每日 AI 新闻/);
  assert.ok(!existsSync(good) && existsSync(good + '.done'));
  await assert.rejects(() => promoteDraft(good, join(home, 'skills')), /not found/);
});
