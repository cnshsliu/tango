import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { mkdir, mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { EventBus } from '../src/core/events.js';
import { installLspDiagnostics, toProblems } from '../src/core/lsp-diagnostics.js';
import { stopAllLsp } from '../src/core/lsp.js';
import type { LspProblem, ToolCallBlock, ToolResult } from '../src/core/types.js';

const REPO = process.cwd();
after(() => stopAllLsp());

/** A project whose node_modules/.bin is this repo's, so typescript-language-server is "installed". */
async function project(name = 'tango-diag-') {
  const dir = await mkdtemp(join(tmpdir(), name));
  await writeFile(join(dir, 'tsconfig.json'), JSON.stringify({ compilerOptions: { strict: true, noEmit: true } }));
  await mkdir(join(dir, 'node_modules'), { recursive: true });
  await symlink(join(REPO, 'node_modules/.bin'), join(dir, 'node_modules/.bin'), 'dir');
  // Windows runs npm's commands through their package's entry script (npm-bin.ts): the packages too
  for (const pkg of ['typescript-language-server', 'typescript', 'pyright'])
    await symlink(join(REPO, 'node_modules', pkg), join(dir, 'node_modules', pkg), 'dir');
  return dir;
}

async function edit(events: EventBus, name: 'write' | 'edit', path: string, isError = false): Promise<void> {
  const call = { type: 'tool_call', id: 'c', name, input: { path } } as ToolCallBlock;
  const result: ToolResult = { content: [{ type: 'text', text: 'ok' }], isError };
  await events.emit('after_tool', { call, result, ms: 1 });
}

async function until(cond: () => boolean, ms = 60_000): Promise<void> {
  const t0 = Date.now();
  while (!cond()) {
    if (Date.now() - t0 > ms) throw new Error('timed out waiting for a diagnostics event');
    await new Promise((r) => setTimeout(r, 50));
  }
}

const framesOf = (events: EventBus) => {
  const frames: { file: string; problems: LspProblem[] }[] = [];
  events.on('diagnostics', (d) => void frames.push(d));
  return frames;
};

test('an edit hands the file to its language server; what it publishes is emitted, and a fix clears it', { timeout: 120_000 }, async () => {
  const proj = await project();
  const bad = join(proj, 'bad.ts');
  await writeFile(bad, 'export const a: number = "s";\n');
  const events = new EventBus();
  const frames = framesOf(events);
  installLspDiagnostics(events, proj);
  try {
    await edit(events, 'edit', bad);
    await until(() => frames.length > 0);
    const first = frames[0];
    assert.equal(first.file, 'bad.ts', 'the path is relative to the working directory');
    assert.equal(first.problems.length, 1);
    assert.equal(first.problems[0].severity, 'error');
    assert.equal(first.problems[0].line, 1);
    assert.equal(first.problems[0].col, 14, 'the string literal, where the server puts it');
    assert.match(first.problems[0].message, /Type 'string' is not assignable to type 'number'/);
    assert.equal(first.problems[0].code, '2322');
    assert.equal(first.problems[0].source, 'typescript');

    await writeFile(bad, 'export const a: number = 1;\n');
    const before = frames.length;
    await edit(events, 'edit', bad);
    await until(() => frames.length > before && frames.at(-1)!.problems.length === 0);
    assert.equal(frames.at(-1)!.file, 'bad.ts', 'a clean file is announced as clean');
  } finally {
    stopAllLsp();
    await rm(proj, { recursive: true, force: true });
  }
});

test('nothing happens for a file type without a server, a failed tool call, or with lspDiagnostics off', async () => {
  const proj = await project();
  const events = new EventBus();
  const frames = framesOf(events);
  installLspDiagnostics(events, proj);
  try {
    await edit(events, 'edit', join(proj, 'notes.txt'));
    await edit(events, 'edit', join(proj, 'x.ts'), true);
    await new Promise((r) => setTimeout(r, 200));
    assert.equal(frames.length, 0);
  } finally {
    await rm(proj, { recursive: true, force: true });
  }

  const off = new EventBus();
  const offFrames = framesOf(off);
  installLspDiagnostics(off, proj, () => false);
  await edit(off, 'write', join(proj, 'y.ts'));
  await new Promise((r) => setTimeout(r, 200));
  assert.equal(offFrames.length, 0);
});

test('a file outside the working directory is not forwarded', { timeout: 120_000 }, async () => {
  const proj = await project();
  const elsewhere = await mkdtemp(join(tmpdir(), 'tango-diag-outside-'));
  const outside = join(elsewhere, 'outside.ts');
  await writeFile(outside, 'export const q: number = "s";\n');
  const events = new EventBus();
  const frames = framesOf(events);
  installLspDiagnostics(events, proj);
  try {
    await edit(events, 'edit', outside);
    // give the server time to start and publish; nothing may come through
    await new Promise((r) => setTimeout(r, 4000));
    assert.equal(frames.length, 0);
  } finally {
    stopAllLsp();
    await rm(proj, { recursive: true, force: true });
    await rm(elsewhere, { recursive: true, force: true });
  }
});

test('toProblems: severities, errors first, one-line messages, caps', () => {
  const raw = [
    { range: { start: { line: 9, character: 2 } }, severity: 2, message: 'unused variable\n  x', source: 'ts', code: 6133 },
    { range: { start: { line: 3, character: 0 } }, severity: 1, message: "Type 'string' is not assignable to type 'number'.", code: 2322, source: 'ts' },
    { range: { start: { line: 1, character: 4 } }, message: 'no severity means error' },
    { range: { start: { line: 0, character: 0 } }, severity: 4, message: '', code: { value: 'hint' } },
    { range: { start: { line: 5, character: 1 } }, severity: 3, message: 'info', tags: [1] },
    'garbage',
    null
  ];
  const problems = toProblems(raw as any[]);
  assert.deepEqual(
    problems.map((p) => [p.line, p.col, p.severity, p.message, p.code, p.source]),
    [
      [2, 5, 'error', 'no severity means error', undefined, undefined],
      [4, 1, 'error', "Type 'string' is not assignable to type 'number'.", '2322', 'ts'],
      [10, 3, 'warning', 'unused variable x', '6133', 'ts'],
      [6, 2, 'info', 'info', undefined, undefined]
    ]
  );
  assert.equal(toProblems(Array.from({ length: 300 }, (_, i) => ({ range: { start: { line: i } }, message: 'm' }))).length, 200, 'at most 200 per file');
  assert.equal(toProblems([{ message: 'x'.repeat(1000) }])[0].message.length, 400, 'messages are capped');
});
