import { test, afterEach } from 'node:test';
import assert from 'node:assert/strict';
import { tmpdir } from 'node:os';

import { bashTool } from '../src/core/tools/bash.js';
import { processListTool, processLogsTool, processStopTool } from '../src/core/tools/process.js';
import { MAX_RUNNING, _resetProcesses, getProcess, startProcess, startup, stopProcess } from '../src/core/processes.js';
import { decide } from '../src/core/permissions.js';

const ctx = () => ({ cwd: tmpdir(), signal: new AbortController().signal });
const out = (r: { content: { type: string; text?: string }[] }) => r.content.map((c) => c.text ?? '').join('');

afterEach(() => _resetProcesses());

test('bash background returns an id at once; logs, list and stop work on it', async () => {
  const t0 = Date.now();
  const r = await bashTool.execute({ command: 'echo ready; sleep 30', background: true }, ctx());
  assert.ok(Date.now() - t0 < 5000, 'does not wait for the command');
  assert.match(out(r), /^started p\d+ pid \d+ \[running/);
  assert.match(out(r), /ready/);
  const id = /started (p\d+)/.exec(out(r))![1];

  assert.match(out(await processListTool.execute({}, ctx())), new RegExp(`${id} .*running.*sleep 30`));
  assert.match(out(await processLogsTool.execute({ id }, ctx())), /ready/);

  const stopped = await processStopTool.execute({ id }, ctx());
  // the process group gets SIGTERM here; Windows ends the tree with taskkill, which reads as exit 1
  assert.match(out(stopped), process.platform === 'win32' ? /stopped: .*exited 1/ : /stopped: .*killed \(SIGTERM\)/);
  assert.equal(getProcess(id)!.done, true);
  assert.match(out(await processStopTool.execute({ id }, ctx())), /already finished/);
});

test('stop escalates to SIGKILL when SIGTERM is ignored, and reaches grandchildren', { skip: process.platform === 'win32' && 'Windows has no SIGTERM to ignore: taskkill /F ends the tree at once' }, async () => {
  const p = startProcess("trap '' TERM; echo armed; sleep 30 & wait", tmpdir());
  // under load the shell may not have set its trap yet; wait until it says so
  while (!p.output.includes('armed')) await new Promise((r) => setTimeout(r, 20));
  const t0 = Date.now();
  await stopProcess(p, 300);
  assert.ok(Date.now() - t0 < 3000);
  assert.equal(p.signal, 'SIGKILL');
});

test('a command that fails right away reports the error', async () => {
  // under load the login shell alone can outlast the 2 s default; the command exits at once,
  // so a long grace costs nothing here and keeps the test from racing shell startup
  const grace = startup.graceMs;
  startup.graceMs = 30_000;
  let r;
  try {
    // node writes the error and the exit code the same way in every shell
    r = await bashTool.execute({ command: `node -e "process.stderr.write('boom'); process.exit(3)"`, background: true }, ctx());
  } finally {
    startup.graceMs = grace;
  }
  assert.equal(r.isError, true);
  assert.match(out(r), /exited 3/);
  assert.match(out(r), /boom/);
});

test('logs keep only the last N lines; unknown ids are errors', async () => {
  const r = await bashTool.execute({ command: 'seq 1 100', background: true }, ctx());
  const id = /started (p\d+)/.exec(out(r))![1];
  await getProcess(id)!.exited;
  const logs = out(await processLogsTool.execute({ id, lines: 3 }, ctx()));
  assert.match(logs, /exited 0/);
  assert.match(logs, /\n98\n99\n100$/);
  assert.equal((await processLogsTool.execute({ id: 'p999' }, ctx())).isError, true);
});

test(`at most ${MAX_RUNNING} run at once`, async () => {
  for (let i = 0; i < MAX_RUNNING; i++) startProcess('sleep 30', tmpdir());
  const r = await bashTool.execute({ command: 'sleep 30', background: true }, ctx());
  assert.equal(r.isError, true);
  assert.match(out(r), /already 8 background processes/);
});

test('permissions: logs/list are reads, stop never asks, readonly refuses stop', () => {
  const call = (name: string, input = {}) => ({ type: 'tool_call' as const, id: 'x', name, input });
  assert.equal(decide('ask', call('process_logs', { id: 'p1' }), '/').action, 'allow');
  assert.equal(decide('ask', call('process_list'), '/').action, 'allow');
  assert.equal(decide('ask', call('process_stop', { id: 'p1' }), '/').action, 'allow');
  assert.equal(decide('readonly', call('process_stop', { id: 'p1' }), '/').action, 'deny');
  assert.equal(decide('readonly', call('process_list'), '/').action, 'allow');
});
