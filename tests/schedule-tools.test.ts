import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { scheduleCreateTool, scheduleListTool, scheduleRemoveTool } from '../src/core/tools/schedule.js';
import { decide } from '../src/core/permissions.js';

const textOf = (r: any) => r.content.map((b: any) => b.text).join('');

test('schedule_create / list / remove manage ~/.tango/tasks.json like tango task', async () => {
  const home = await mkdtemp(join(tmpdir(), 'tango-sched-'));
  const prev = process.env.TANGO_HOME;
  process.env.TANGO_HOME = home;
  const ctx = { cwd: '/work/repo', signal: new AbortController().signal };
  try {
    assert.equal(textOf(await scheduleListTool.execute({}, ctx)), 'no scheduled tasks');
    const made = await scheduleCreateTool.execute({ name: 'ci', every: '2h', prompt: 'check the build' }, ctx);
    assert.ok(!made.isError, textOf(made));
    const saved = JSON.parse(await readFile(join(home, 'tasks.json'), 'utf8'));
    assert.deepEqual(saved, [{ name: 'ci', prompt: 'check the build', everyMinutes: 120, cwd: '/work/repo', mode: 'readonly' }]);
    assert.match(textOf(await scheduleListTool.execute({}, ctx)), /^ci · every 120m · readonly · last: never · cwd \/work\/repo\n  check the build/);
    assert.ok((await scheduleCreateTool.execute({ name: 'ci', every: '1h', prompt: 'x' }, ctx)).isError, 'duplicate name');
    assert.ok((await scheduleCreateTool.execute({ name: 'y', every: 'soon', prompt: 'x' }, ctx)).isError, 'bad interval');
    assert.ok((await scheduleCreateTool.execute({ name: 'z', every: '1h', prompt: 'x', mode: 'ask' }, ctx)).isError, 'headless never asks');
    assert.equal(textOf(await scheduleRemoveTool.execute({ name: 'ci' }, ctx)), 'removed "ci"');
    assert.ok((await scheduleRemoveTool.execute({ name: 'ci' }, ctx)).isError);
  } finally {
    if (prev === undefined) delete process.env.TANGO_HOME;
    else process.env.TANGO_HOME = prev;
    await rm(home, { recursive: true, force: true });
  }
});

test('schedule_list is a read; creating or removing a schedule asks in ask mode', () => {
  const call = (name: string) => ({ type: 'tool_call' as const, id: '1', name, input: {} });
  assert.equal(decide('ask', call('schedule_list'), '/w').action, 'allow');
  assert.equal(decide('readonly', call('schedule_list'), '/w').action, 'allow');
  assert.equal(decide('ask', call('schedule_create'), '/w').action, 'ask');
  assert.equal(decide('ask', call('schedule_remove'), '/w').action, 'ask');
  assert.equal(decide('readonly', call('schedule_create'), '/w').action, 'deny');
});
