import { test as nodeTest } from 'node:test';
// Not on Windows: the hook commands are POSIX shell scripts: they cannot run where tango's shell is PowerShell.
const test = process.platform === 'win32' ? nodeTest.skip : nodeTest;
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { hookMatches, mergeHookConfigs, parseHookEntries, parseHookJson, runHooks } from '../src/core/hooks.js';

test('hook entries: flat and nested (Claude Code) forms, junk skipped', () => {
  const cmds = parseHookEntries([
    { matcher: 'bash', command: 'a.sh', timeout: 3 },
    { matcher: 'edit|write', hooks: [{ type: 'command', command: 'b.sh' }, { type: 'prompt', command: 'x' }] },
    { command: '   ' },
    'nope',
    null
  ]);
  assert.deepEqual(cmds, [
    { matcher: 'bash', command: 'a.sh', timeout: 3 },
    { matcher: 'edit|write', command: 'b.sh', timeout: undefined }
  ]);
  assert.deepEqual(parseHookEntries({ command: 'x' }), [], 'an event value must be an array');
});

test('hook merge: config first, then plugins; legacy keys ignored', () => {
  const merged = mergeHookConfigs([
    { beforeTool: 'old.sh', Stop: [{ command: 'one' }] },
    { Stop: [{ command: 'two' }], PreToolUse: [{ command: 'p' }] },
    undefined
  ]);
  assert.deepEqual(merged.Stop?.map((c) => c.command), ['one', 'two']);
  assert.equal(merged.PreToolUse?.length, 1);
  assert.equal(Object.keys(merged).length, 2);
});

test('hook matcher: anchored regex over the tool name', () => {
  assert.ok(hookMatches(undefined, 'bash'));
  assert.ok(hookMatches('*', 'bash'));
  assert.ok(hookMatches('bash|edit', 'edit'));
  assert.ok(!hookMatches('bash', 'bash_extra'), 'anchored');
  assert.ok(hookMatches('mcp_.*', 'mcp_fs_read'));
  assert.ok(!hookMatches('(', 'bash'), 'a broken regex matches nothing');
  assert.ok(hookMatches('bash', undefined), 'non-tool events ignore the matcher');
});

test('runHooks: stdin payload, exit 2 blocks with stderr, other exits only warn', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-hooks-'));
  try {
    const out = join(dir, 'in.json');
    const ok = await runHooks({ PreToolUse: [{ command: `cat > ${out}` }] }, 'PreToolUse', { tool_name: 'bash' }, { cwd: dir, tool: 'bash' });
    assert.deepEqual(ok, {});
    const got = JSON.parse(await readFile(out, 'utf8'));
    assert.equal(got.hook_event_name, 'PreToolUse');
    assert.equal(got.tool_name, 'bash');
    const blocked = await runHooks(
      { PreToolUse: [{ command: 'exit 1' }, { command: 'echo no rm >&2; exit 2' }, { command: `touch ${join(dir, 'after')}` }] },
      'PreToolUse', {}, { cwd: dir, tool: 'bash' });
    assert.equal(blocked.block, 'no rm');
    await assert.rejects(readFile(join(dir, 'after')), 'the chain stops at the first block');
    const skipped = await runHooks({ PreToolUse: [{ matcher: 'edit', command: 'exit 2' }] }, 'PreToolUse', {}, { cwd: dir, tool: 'bash' });
    assert.deepEqual(skipped, {}, 'non-matching hooks never run');
    const slow = await runHooks({ Stop: [{ command: 'sleep 5; exit 2', timeout: 0.2 }] }, 'Stop', {}, { cwd: dir });
    assert.deepEqual(slow, {}, 'a timed-out hook is killed and does not block');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('parseHookJson: only a JSON object counts; unknown or mistyped fields are dropped', () => {
  assert.deepEqual(parseHookJson(''), {});
  assert.deepEqual(parseHookJson('all good\n'), {});
  assert.deepEqual(parseHookJson('{not json'), {});
  assert.deepEqual(parseHookJson('[1,2]'), {});
  assert.deepEqual(parseHookJson(' {"decision":"block","reason":"r","extra":1}\n'), { decision: 'block', reason: 'r' });
  assert.deepEqual(parseHookJson('{"decision":"maybe","continue":"no","additionalContext":"  "}'), {});
  assert.deepEqual(parseHookJson('{"continue":false,"stopReason":"s","additionalContext":"c"}'), { continue: false, stopReason: 's', additionalContext: 'c' });
});
