import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { runHooks } from '../src/core/hooks.js';

// Hooks written as node one-liners run in every shell tango uses — bash on macOS and Linux,
// PowerShell on Windows — so this is the hook runner's coverage on Windows too, where
// tests/hooks.test.ts (shell-script hooks) skips.
test('hooks in any shell: the payload reaches the command, exit 2 blocks with its stderr, stdout JSON adds context, a slow hook is killed', { timeout: 90_000 }, async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-hooks-any-'));
  try {
    const out = join(dir, 'in.json');
    const reads = `node -e "require('fs').writeFileSync(process.argv[1], require('fs').readFileSync(0, 'utf8'))" "${out}"`;
    assert.deepEqual(await runHooks({ PreToolUse: [{ command: reads }] }, 'PreToolUse', { tool_name: 'bash' }, { cwd: dir, tool: 'bash' }), {});
    const got = JSON.parse(await readFile(out, 'utf8'));
    assert.equal(got.hook_event_name, 'PreToolUse');
    assert.equal(got.tool_name, 'bash');

    const blocked = await runHooks(
      {
        PreToolUse: [
          { command: `node -e "process.exit(1)"` },
          { command: `node -e "process.stderr.write('no rm'); process.exit(2)"` },
          { command: `node -e "require('fs').writeFileSync('after', '')"` }
        ]
      },
      'PreToolUse',
      {},
      { cwd: dir, tool: 'bash' }
    );
    assert.equal(blocked.block, 'no rm');
    await assert.rejects(readFile(join(dir, 'after')), 'the chain stops at the first block');

    const skipped = await runHooks({ PreToolUse: [{ matcher: 'edit', command: `node -e "process.exit(2)"` }] }, 'PreToolUse', {}, { cwd: dir, tool: 'bash' });
    assert.deepEqual(skipped, {}, 'non-matching hooks never run');

    const context = await runHooks(
      { PostToolUse: [{ command: `node -e "process.stdout.write(JSON.stringify({ additionalContext: 'from the hook' }))"` }] },
      'PostToolUse',
      {},
      { cwd: dir, tool: 'bash' }
    );
    assert.deepEqual(context.context, ['from the hook'], 'stdout JSON is collected for the model');

    const slow = await runHooks({ Stop: [{ command: `node -e "setTimeout(() => {}, 5000)"`, timeout: 0.5 }] }, 'Stop', {}, { cwd: dir });
    assert.deepEqual(slow, {}, 'a timed-out hook is killed and does not block');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
