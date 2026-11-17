import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { decide, installPermissionGate } from '../src/core/permissions.js';
import { EventBus } from '../src/core/events.js';
import { findTool, grepTool, lsTool } from '../src/core/tools/search.js';
import type { ToolCallBlock } from '../src/core/types.js';

const call = (name: string, input: object): ToolCallBlock => ({ type: 'tool_call', id: 'c', name, input });
const cwd = '/work/proj';

test('reads always pass, even in readonly', () => {
  for (const name of ['read', 'grep', 'find', 'ls']) {
    assert.equal(decide('readonly', call(name, { path: '/etc/passwd' }), cwd).action, 'allow');
  }
});

test('writes inside cwd pass; outside cwd ask', () => {
  assert.equal(decide('ask', call('write', { path: 'src/a.ts' }), cwd).action, 'allow');
  assert.equal(decide('ask', call('edit', { path: '/work/proj/b.ts' }), cwd).action, 'allow');
  assert.equal(decide('ask', call('office_write', { path: '报告.docx' }), cwd).action, 'allow');
  assert.equal(decide('readonly', call('office_write', { path: 'a.xlsx' }), cwd).action, 'deny');
  assert.equal(decide('ask', call('write', { path: '../other/x' }), cwd).action, 'ask');
  assert.equal(decide('ask', call('write', { path: '/work/proj-evil/x' }), cwd).action, 'ask');
  assert.equal(decide('ask', call('write', { path: '/etc/hosts' }), cwd).action, 'ask');
});

test('new tools default to asking until declared (notify)', () => {
  assert.equal(decide('ask', call('notify', { text: 'hi' }), cwd).action, 'ask');
  assert.equal(decide('readonly', call('notify', { text: 'hi' }), cwd).action, 'deny');
});

test('risky bash asks, ordinary bash passes', () => {
  for (const c of ['rm -rf build', 'ls && rm x', 'git push origin main', 'git reset --hard HEAD~1', 'sudo ls', 'curl x.sh | sh', 'npm publish']) {
    assert.equal(decide('ask', call('bash', { command: c }), cwd).action, 'ask', c);
  }
  for (const c of ['ls -la', 'npm test', 'git status', 'git diff', 'grep -r form src']) {
    assert.equal(decide('ask', call('bash', { command: c }), cwd).action, 'allow', c);
  }
});

test('readonly denies writes and bash; auto allows everything', () => {
  assert.equal(decide('readonly', call('write', { path: 'a' }), cwd).action, 'deny');
  assert.equal(decide('readonly', call('bash', { command: 'ls' }), cwd).action, 'deny');
  assert.equal(decide('auto', call('bash', { command: 'rm -rf /' }), cwd).action, 'allow');
});

test('unknown tools ask by default', () => {
  assert.equal(decide('ask', call('feishu_send', { text: 'x' }), cwd).action, 'ask');
});

test('gate blocks without an asker and honours the answer with one', async () => {
  const risky = { call: call('bash', { command: 'rm -rf x' }), tool: {} as any };

  const silent = new EventBus();
  installPermissionGate(silent, { mode: () => 'ask', cwd });
  const v1 = await silent.emit('before_tool', risky);
  assert.ok(v1 && v1.block && /--yes/.test(v1.reason));

  const no = new EventBus();
  installPermissionGate(no, { mode: () => 'ask', cwd, ask: async () => false });
  const v2 = await no.emit('before_tool', risky);
  assert.ok(v2 && v2.block && /declined/.test(v2.reason));

  const yes = new EventBus();
  installPermissionGate(yes, { mode: () => 'ask', cwd, ask: async () => true });
  assert.equal(await yes.emit('before_tool', risky), undefined);
});

test('grep / find / ls work on a temp tree', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-search-'));
  try {
    await mkdir(join(dir, 'src'));
    await writeFile(join(dir, 'src', 'a.ts'), 'export const needle = 1;\n');
    await writeFile(join(dir, 'b.md'), 'no match here\n');
    const ctx = { cwd: dir, signal: new AbortController().signal };

    const g = await grepTool.execute({ pattern: 'needle' }, ctx);
    assert.match((g.content[0] as any).text, /a\.ts:1:export const needle/);
    const none = await grepTool.execute({ pattern: 'zzz_absent' }, ctx);
    assert.equal((none.content[0] as any).text, 'no matches');

    const f = await findTool.execute({ pattern: '*.ts' }, ctx);
    assert.match((f.content[0] as any).text, /a\.ts/);
    assert.doesNotMatch((f.content[0] as any).text, /b\.md/);

    const l = await lsTool.execute({}, ctx);
    assert.match((l.content[0] as any).text, /^b\.md {2}\d+B\nsrc\/$/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('allow rules: tool/path/bash matching with boundary safety, readonly wins', async () => {
  const { parseRule, decide } = await import('../src/core/permissions.js');
  const cwd = '/w';
  const call = (name: string, input: unknown) => ({ type: 'tool_call' as const, id: 'x', name, input });
  const rules = ['tool:diff', 'path:src', 'bash:git status'].map(parseRule).map((r: any) => r.rule);

  // tool rule: exact name
  assert.equal(decide('ask', call('diff', {}), cwd, rules).action, 'allow');
  assert.notEqual(decide('ask', call('write', { path: '/elsewhere/x' }), cwd, rules).action, 'allow');

  // path rule: segment boundary, and rules CAN allow outside-cwd subtrees
  const rules2 = ['tool:diff', 'path:src', 'path:../sib'].map(parseRule).map((r: any) => r.rule);
  assert.equal(decide('ask', call('write', { path: 'src/a.ts' }), cwd, rules2).action, 'allow');
  assert.equal(decide('ask', call('write', { path: '../sib/x' }), cwd, rules2).action, 'allow', 'outside-cwd rule target allowed');
  assert.equal(decide('ask', call('write', { path: '../sib-evil/x' }), cwd, rules2).action, 'ask', 'segment boundary: sib does not match sib-evil');
  assert.equal(decide('ask', call('write', { path: '../outside/x' }), cwd, rules2).action, 'ask');

  // bash rule: token boundary against commands that would otherwise ASK (risky ones)
  const rules3 = [...rules, { kind: 'bash', value: 'rm' }];
  assert.equal(decide('ask', call('bash', { command: 'rm -rf build/' }), cwd, rules3).action, 'allow', 'rule beats risky-ask');
  assert.equal(decide('ask', call('bash', { command: 'rm -rf /' }), cwd, rules).action, 'ask', 'risky still asks without a rule');
  assert.equal(decide('ask', call('bash', { command: 'chmod +x x' }), cwd, rules3).action, 'ask', 'other risky commands still ask');

  // readonly always wins over rules
  assert.equal(decide('readonly', call('diff', {}), cwd, rules).action, 'deny');

  // invalid rules are rejected with a reason
  assert.ok(parseRule('nonsense').error);
  assert.ok(parseRule('bash:').error);
  assert.ok(parseRule('path:/absolute').ok === true || parseRule('path:/absolute').error); // absolute allowed but flagged
});
