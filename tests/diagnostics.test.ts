import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdir, mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { EventBus } from '../src/core/events.js';
import { installDiagnostics } from '../src/core/diagnostics.js';
import { diffTool } from '../src/core/tools/diff.js';
import type { ToolCallBlock, ToolResult } from '../src/core/types.js';

const REPO = process.cwd();
const out = (r: { content: any[] }) => r.content.map((c) => c.text ?? '').join('');

async function fakeTool(events: EventBus, name: string, path: string): Promise<ToolResult> {
  const call = { type: 'tool_call', id: 'c', name, input: { path } } as ToolCallBlock;
  const result: ToolResult = { content: [{ type: 'text', text: `ok wrote ${path}` }] };
  await events.emit('after_tool', { call, result, ms: 1 });
  return result;
}

test('typescript diagnostics reach the tool result after an edit', async () => {
  const proj = await mkdtemp(join(tmpdir(), 'tango-ts-'));
  await writeFile(join(proj, 'tsconfig.json'), JSON.stringify({ compilerOptions: { strict: true, noEmit: true } }));
  await mkdir(join(proj, 'node_modules'), { recursive: true });
  await symlink(join(REPO, 'node_modules/.bin'), join(proj, 'node_modules/.bin'), 'dir');
  // Windows runs npm's commands through their package's entry script (npm-bin.ts): the packages too
  for (const pkg of ['typescript-language-server', 'typescript', 'pyright'])
    await symlink(join(REPO, 'node_modules', pkg), join(proj, 'node_modules', pkg), 'dir');
  const bad = join(proj, 'bad.ts');
  await writeFile(bad, 'export const a: number = "s";\n');

  const events = new EventBus();
  installDiagnostics(events, proj);
  const res = await fakeTool(events, 'edit', bad);
  const text = out(res);
  assert.match(text, /\[diagnostics after this edit\]/);
  assert.match(text, /bad\.ts|2322|number/);

  // clean project → nothing appended (the bad file is what made tsc fail)
  await rm(bad);
  const good = join(proj, 'good.ts');
  await writeFile(good, 'export const b: number = 1;\n');
  assert.doesNotMatch(out(await fakeTool(events, 'write', good)), /diagnostics/);
  await rm(proj, { recursive: true, force: true });
});

test('python diagnostics via the project venv pyright', { skip: process.platform === 'win32' && 'a venv\'s bin/ layout is POSIX' }, async () => {
  const proj = await mkdtemp(join(tmpdir(), 'tango-py-'));
  await writeFile(join(proj, 'pyproject.toml'), '[project]\nname = "x"\nversion = "0"\n');
  await mkdir(join(proj, '.venv/bin'), { recursive: true });
  await symlink(join(REPO, 'node_modules/.bin/pyright'), join(proj, '.venv/bin/pyright'));
  const bad = join(proj, 'app.py');
  await writeFile(bad, 'def f(a: int) -> str:\n    return a + "x"\n');

  const events = new EventBus();
  installDiagnostics(events, proj);
  assert.match(out(await fakeTool(events, 'edit', bad)), /\[diagnostics after this edit\][\s\S]*line/);
  await rm(proj, { recursive: true, force: true });
});

test('no checker installed → silent, no nagging', async () => {
  const proj = await mkdtemp(join(tmpdir(), 'tango-none-'));
  await writeFile(join(proj, 'requirements.txt'), '');
  const f = join(proj, 'a.py');
  await writeFile(f, 'x = 1\n');
  const events = new EventBus();
  installDiagnostics(events, proj);
  assert.equal(out(await fakeTool(events, 'edit', f)), `ok wrote ${f}`);
  await rm(proj, { recursive: true, force: true });
});

test('diff tool: summary, per-file hunks, clean repo', async () => {
  const repo = await mkdtemp(join(tmpdir(), 'tango-git-'));
  const git = (...a: string[]) => spawnSync('git', ['-c', 'user.email=t@t', '-c', 'user.name=t', ...a], { cwd: repo, encoding: 'utf8' });
  git('init', '-q', '-b', 'main');
  await writeFile(join(repo, 'a.ts'), 'one\n');
  git('add', '-A');
  git('commit', '-qm', 'x');
  await writeFile(join(repo, 'a.ts'), 'one\ntwo\n');
  await writeFile(join(repo, 'new.ts'), 'n\n');
  const ctx = { cwd: repo, signal: new AbortController().signal };

  const summary = out(await diffTool.execute({}, ctx));
  assert.match(summary, /a\.ts/);
  assert.match(summary, /\?\? new\.ts/);

  const full = out(await diffTool.execute({ paths: ['a.ts'] }, ctx));
  assert.match(full, /^\+two$/m);

  git('add', '-A');
  git('commit', '-qm', 'y');
  assert.match(out(await diffTool.execute({}, ctx)), /clean/);
  await rm(repo, { recursive: true, force: true });
});
