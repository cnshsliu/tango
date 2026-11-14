import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { existsSync } from 'node:fs';
import { dueTasks, lastRunAt, parseInterval, runDueOnce, sendFeishu, slugify } from '../src/core/scheduler.js';
import { addTask, removeTask, loadTasks, summarizeOutput } from '../src/core/scheduler.js';

const home = await mkdtemp(join(tmpdir(), 'tango-task-'));
process.env.TANGO_HOME = home;

test('parseInterval', () => {
  assert.equal(parseInterval('45'), 45);
  assert.equal(parseInterval('90m'), 90);
  assert.equal(parseInterval('2h'), 120);
  assert.equal(parseInterval('36H'), 2160);
  assert.equal(parseInterval('1d'), 1440);
  assert.throws(() => parseInterval('soon'), /bad interval/);
});

test('tasks add / due / run-once / rm lifecycle', async () => {
  await addTask({ name: '新闻摘要', prompt: '整理新闻并发飞书', everyMinutes: 60, cwd: home, mode: 'readonly' });
  await assert.rejects(() => addTask({ name: '新闻摘要', prompt: 'x', everyMinutes: 1 }), /already exists/);
  assert.equal((await loadTasks()).length, 1);

  const runs: string[] = [];
  const run = async (t: { name: string }) => {
    runs.push(t.name);
    return { ok: true, output: 'done: 3 条新闻' };
  };
  const t0 = Date.UTC(2026, 10, 14, 8, 0, 0);
  const clock = (at: number) => () => new Date(at);
  assert.equal((await dueTasks(t0)).length, 1); // never ran → due
  assert.equal(await runDueOnce(run, t0, clock(t0)), 1);
  assert.deepEqual(runs, ['新闻摘要']);

  const last = await lastRunAt('新闻摘要');
  assert.ok(last?.ok);
  assert.equal((await dueTasks(t0 + 59 * 60_000)).length, 0); // 59m later → not due
  assert.equal((await dueTasks(t0 + 61 * 60_000)).length, 1); // 61m later → due

  // a failing run is recorded and does not throw
  const t1 = t0 + 61 * 60_000;
  assert.equal(await runDueOnce(() => { throw new Error('model down'); }, t1, clock(t1)), 1);
  assert.equal((await lastRunAt('新闻摘要'))?.ok, false);

  const names = await readdir(join(home, 'sessions'));
  const taskFiles = names.filter((f) => f.startsWith('task-'));
  assert.ok(taskFiles.length >= 2 && taskFiles.every((n) => n.startsWith('task-新闻摘要-')), String(taskFiles));
  const body = await readFile(join(home, 'sessions', taskFiles.sort().at(-1)!), 'utf8');
  assert.match(body, /scheduled task \\"新闻摘要\\"/);

  assert.equal((await removeTask('新闻摘要')).name, '新闻摘要');
  assert.equal((await loadTasks()).length, 0);
});

test('summarizeOutput truncates', () => {
  assert.equal(summarizeOutput('  hi \n'), 'hi');
  assert.equal(summarizeOutput('x'.repeat(1000), 800).length, 801);
});

test('sendFeishu posts a card and treats body code!=0 as failure', async () => {
  const seen: any[] = [];
  let fail = false;
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      seen.push(JSON.parse(raw));
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(fail ? '{"code":19024,"msg":"bad webhook"}' : '{"code":0,"msg":"ok"}');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  const url = `http://127.0.0.1:${(server.address() as AddressInfo).port}/hook`;
  try {
    await sendFeishu(url, 'tango · 日报', '**ok** 内容');
    assert.equal(seen[0].msg_type, 'interactive');
    assert.equal(seen[0].card.header.title.content, 'tango · 日报');
    assert.equal(seen[0].card.elements[0].content, '**ok** 内容');
    fail = true;
    await assert.rejects(() => sendFeishu(url, 'x', 'y'), /code 19024/);
  } finally {
    server.close();
  }
});
