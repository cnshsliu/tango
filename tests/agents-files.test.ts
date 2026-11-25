import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { findAgentsFiles, loadAgentsFiles } from '../src/core/context.js';

test('AGENTS.override.md replaces AGENTS.md in its own directory only', async () => {
  const root = await mkdtemp(join(tmpdir(), 'tango-agents-'));
  const home = join(root, 'home');
  const repo = join(root, 'repo');
  const sub = join(repo, 'pkg');
  const prev = process.env.TANGO_HOME;
  process.env.TANGO_HOME = home;
  try {
    await mkdir(home, { recursive: true });
    await mkdir(sub, { recursive: true });
    await writeFile(join(home, 'AGENTS.md'), 'global');
    await writeFile(join(repo, 'AGENTS.md'), 'repo shared');
    await writeFile(join(repo, 'AGENTS.override.md'), 'repo mine');
    await writeFile(join(sub, 'AGENTS.md'), 'pkg');
    assert.deepEqual(findAgentsFiles(sub), [join(home, 'AGENTS.md'), join(repo, 'AGENTS.override.md'), join(sub, 'AGENTS.md')]);
    assert.deepEqual(await loadAgentsFiles(sub), ['global', 'repo mine', 'pkg']);
    // the global file is read once even when cwd is inside TANGO_HOME
    assert.deepEqual(findAgentsFiles(home).filter((f) => f === join(home, 'AGENTS.md')).length, 1);
  } finally {
    if (prev === undefined) delete process.env.TANGO_HOME;
    else process.env.TANGO_HOME = prev;
    await rm(root, { recursive: true, force: true });
  }
});
