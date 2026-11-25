import { test } from 'node:test';
import assert from 'node:assert/strict';
import { chmod, mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { findTool, grepTool, searchOutcome } from '../src/core/tools/search.js';

const textOf = (r: any) => r.content.map((b: any) => b.text).join('');

test('searchOutcome: results win over unreadable paths; a real error with nothing found fails', () => {
  const body = (o: string) => o.trim();
  const denied = 'rg: ./Library/x: Operation not permitted (os error 1)\nrg: ./y: Permission denied (os error 13)';
  const hit = searchOutcome('find', { out: 'a.pptx\n', err: denied, code: 2 }, 'no files found', body);
  assert.ok(!hit.isError);
  assert.equal(textOf(hit), 'a.pptx\n[skipped 2 unreadable paths (no permission)]');
  const none = searchOutcome('find', { out: '', err: denied, code: 2 }, 'no files found', body);
  assert.ok(!none.isError, 'only unreadable paths: an empty search, not a failure');
  assert.match(textOf(none), /^no files found\n\[skipped 2/);
  const bad = searchOutcome('grep', { out: '', err: 'rg: regex parse error: unclosed group', code: 2 }, 'no matches', body);
  assert.ok(bad.isError);
  assert.equal(textOf(bad), 'grep failed: rg: regex parse error: unclosed group');
  assert.equal(textOf(searchOutcome('grep', { out: '', err: '', code: 1 }, 'no matches', body)), 'no matches');
});

test('find and grep keep their results when part of the tree is unreadable', { skip: process.getuid?.() === 0 }, async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-search-'));
  try {
    await writeFile(join(dir, 'deck-英智.pptx'), 'needle');
    await mkdir(join(dir, 'locked'));
    await writeFile(join(dir, 'locked', 'x.pptx'), 'needle');
    await chmod(join(dir, 'locked'), 0o000);
    const ctx = { cwd: dir, signal: new AbortController().signal };
    const f = await findTool.execute({ pattern: '*.pptx' }, ctx);
    assert.ok(!f.isError, textOf(f));
    assert.match(textOf(f), /deck-英智\.pptx/);
    const g = await grepTool.execute({ pattern: 'needle' }, ctx);
    assert.ok(!g.isError, textOf(g));
    assert.match(textOf(g), /deck-英智\.pptx:1:needle/);
  } finally {
    await chmod(join(dir, 'locked'), 0o755).catch(() => {});
    await rm(dir, { recursive: true, force: true });
  }
});
