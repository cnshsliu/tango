import { test } from 'node:test';
import { readFileSync } from 'node:fs';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
// @ts-expect-error plain .mjs script, no types
import { gaps, mentions, section } from '../scripts/docs-check.mjs';
// @ts-expect-error plain .mjs script, no types
import { isCode, isDoc, stopVerdict } from '../scripts/hooks/docs-guard.mjs';

const GUARD = join(import.meta.dirname, '..', 'scripts', 'hooks', 'docs-guard.mjs');

test('docs coverage: every tool, flag, config key, serve command, env var and key action is documented', () => {
  const found = gaps();
  assert.deepEqual(found, [], found.map((g: any) => `${g.kind} ${g.name} → ${g.where}`).join('\n'));
});

test('docs check: a name counts only in backticks or a code block, inside its section', () => {
  const md = '## A\nuse `--yes` here\n```\ntango task add x\n```\n## B\nplain --model text\n';
  assert.ok(mentions(section(md, 'A'), '--yes'));
  assert.ok(mentions(section(md, 'A'), 'add'), 'fenced block counts');
  assert.ok(!mentions(section(md, 'B'), '--model'), 'plain prose does not count');
  assert.ok(!mentions(section(md, 'B'), '--yes'), 'other section does not count');
  assert.ok(!mentions('`set_bubble`', 'bubble'), 'part of a longer name does not count');
});

test('docs guard verdict: code without docs is sent back once; gaps always; tests or docs alone pass', () => {
  assert.ok(isCode('src/core/agent.ts') && isCode('tui/src/app.rs') && !isCode('tests/x.test.ts'));
  assert.ok(isDoc('README.md') && isDoc('docs/handbook.md') && !isDoc('docs/notes/x.md'));
  assert.equal(stopVerdict(['tests/a.test.ts', 'README.md'], [], false), null);
  assert.match(stopVerdict(['src/core/agent.ts'], [], false), /changed code but no documentation/);
  assert.equal(stopVerdict(['src/core/agent.ts'], [], true), null, 'second stop may finish');
  assert.equal(stopVerdict(['src/core/agent.ts', 'docs/handbook.md'], [], false), null);
  const gap = [{ kind: 'tool', name: 'lsp', where: 'README.md' }];
  assert.match(stopVerdict(['src/core/tools/lsp.ts', 'README.md'], gap, true), /tool `lsp` → README.md/);
});

test('docs guard end to end: the turn baseline separates this turn from earlier dirty files', async () => {
  const repo = await mkdtemp(join(tmpdir(), 'tango-docs-guard-'));
  const sh = (...a: string[]) => spawnSync('git', a, { cwd: repo, encoding: 'utf8' });
  const hook = (ev: object) =>
    spawnSync(process.execPath, [GUARD], { cwd: repo, input: JSON.stringify(ev), encoding: 'utf8', env: { ...process.env, TANGO_PROJECT_DIR: repo } });
  // the hook's own output travels with a failed status: what it thought changed is in there
  const assertStatus = (r: { status: number | null; stdout: string; stderr: string }, expected: number, msg?: string) =>
    assert.equal(r.status, expected, `${msg ?? ''}\n${r.stdout}${r.stderr}`);
  try {
    sh('init', '-q', '-b', 'main');
    sh('config', 'user.email', 't@t');
    sh('config', 'user.name', 't');
    await mkdir(join(repo, 'scripts'));
    await mkdir(join(repo, 'src'));
    await writeFile(join(repo, 'scripts', 'docs-check.mjs'), "console.log('{\"gaps\":[]}')\n");
    await writeFile(join(repo, 'src', 'a.ts'), 'export const a = 1;\n');
    await writeFile(join(repo, 'README.md'), '# r\n');
    sh('add', '.');
    sh('commit', '-qm', 'init');
    // an edit left over from before this turn does not count against it
    await writeFile(join(repo, 'src', 'a.ts'), 'export const a = 2;\n');
    assertStatus(hook({ hook_event_name: 'UserPromptSubmit', prompt: 'x' }), 0);
    assertStatus(hook({ hook_event_name: 'Stop', stop_hook_active: false }), 0, 'nothing changed this turn');

    // this turn edits code and commits it, without docs → sent back once
    await writeFile(join(repo, 'src', 'b.ts'), 'export const b = 1;\n');
    sh('add', 'src/b.ts');
    sh('commit', '-qm', 'b');
    const r = hook({ hook_event_name: 'Stop', stop_hook_active: false });
    assert.equal(r.status, 2, `stdout: ${r.stdout}\nstderr: ${r.stderr}\nstatus now: ${sh('status', '--porcelain', '--untracked-files=all').stdout}\nlog: ${sh('log', '--oneline').stdout}\nbase: ${sh('rev-parse', '--absolute-git-dir').stdout.trim()}\n${readFileSync(join(sh('rev-parse', '--absolute-git-dir').stdout.trim(), 'docs-guard-base.json'), 'utf8')}`);
    assert.match(r.stderr, /src\/b\.ts/);
    assert.doesNotMatch(r.stderr, /src\/a\.ts/, 'the earlier edit is not blamed on this turn');
    assertStatus(hook({ hook_event_name: 'Stop', stop_hook_active: true }), 0);

    // a doc edit in the same turn satisfies it
    await writeFile(join(repo, 'README.md'), '# r\n\nb exists\n');
    assertStatus(hook({ hook_event_name: 'Stop', stop_hook_active: false }), 0);
  } finally {
    await rm(repo, { recursive: true, force: true });
  }
});
