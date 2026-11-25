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

test('skills are also found in .agents/skills, with quoted frontmatter values', async () => {
  const { discoverSkills } = await import('../src/core/context.js');
  const root = await mkdtemp(join(tmpdir(), 'tango-skills-'));
  const prev = process.env.TANGO_HOME;
  process.env.TANGO_HOME = join(root, 'home');
  try {
    await mkdir(join(root, '.tango', 'skills', 'deploy'), { recursive: true });
    await writeFile(join(root, '.tango', 'skills', 'deploy', 'SKILL.md'), '---\nname: deploy\ndescription: tango one\n---\n');
    await mkdir(join(root, '.agents', 'skills', 'deploy'), { recursive: true });
    await writeFile(join(root, '.agents', 'skills', 'deploy', 'SKILL.md'), '---\nname: deploy\ndescription: shadowed\n---\n');
    await mkdir(join(root, '.agents', 'skills', 'pdf'), { recursive: true });
    await writeFile(join(root, '.agents', 'skills', 'pdf', 'SKILL.md'), '---\nname: "pdf"\ndescription: "Use when reading PDFs"\n---\n');
    const found = (await discoverSkills(root)).filter((s) => s.path.startsWith(root));
    assert.deepEqual(
      found.map((s) => [s.name, s.description]),
      [['deploy', 'tango one'], ['pdf', 'Use when reading PDFs']]
    );
  } finally {
    if (prev === undefined) delete process.env.TANGO_HOME;
    else process.env.TANGO_HOME = prev;
    await rm(root, { recursive: true, force: true });
  }
});
