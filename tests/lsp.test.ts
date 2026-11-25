import { test, after } from 'node:test';
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { lspTool, positionOn, formatHover } from '../src/core/tools/lsp.js';
import { stopAllLsp } from '../src/core/lsp.js';
import { decide } from '../src/core/permissions.js';

const textOf = (r: any) => r.content.map((b: any) => b.text).join('');
const fx = (lang: string) => ({ cwd: join(import.meta.dirname, 'fixtures', 'lsp', lang), signal: new AbortController().signal });
after(() => stopAllLsp());

test('positionOn: whole-word symbol, column, first non-space', () => {
  assert.equal(positionOn('  const three = add(1, 2);', 'add'), 16);
  assert.equal(positionOn('addAll(add(1))', 'add'), 7, 'whole word, not the prefix of addAll');
  assert.equal(positionOn('x = foo', undefined, 5), 4);
  assert.equal(positionOn('    return a', undefined), 4);
  assert.equal(positionOn('nothing here', 'zzz'), undefined);
  assert.equal(formatHover({ contents: [{ language: 'ts', value: 'fn add' }, 'Adds.'] }), 'fn add\n\nAdds.');
});

test('lsp is a read in every mode', () => {
  const call = { type: 'tool_call' as const, id: '1', name: 'lsp', input: { op: 'hover', path: 'x.ts', line: 1 } };
  assert.equal(decide('ask', call, '/w').action, 'allow');
  assert.equal(decide('readonly', call, '/w').action, 'allow');
});

test('lsp (typescript): definition, references, hover, symbols, incoming calls', { timeout: 90_000 }, async () => {
  const ctx = fx('ts');
  const def = await lspTool.execute({ op: 'definition', path: 'use.ts', line: 3, symbol: 'add' }, ctx);
  assert.equal(textOf(def), 'math.ts:2:17  export function add(a: number, b: number): number {');
  const refs = textOf(await lspTool.execute({ op: 'references', path: 'math.ts', line: 2, symbol: 'add' }, ctx)).split('\n');
  assert.deepEqual(refs.map((l) => l.split('  ')[0]).sort(), ['math.ts:2:17', 'use.ts:1:10', 'use.ts:3:22', 'use.ts:4:21']);
  const hover = textOf(await lspTool.execute({ op: 'hover', path: 'use.ts', line: 3, symbol: 'add' }, ctx));
  assert.match(hover, /add\(a: number, b: number\): number/);
  assert.match(hover, /Add two numbers/);
  assert.match(textOf(await lspTool.execute({ op: 'symbols', path: 'use.ts' }, ctx)), /constant three {2}3\nconstant five {2}4/);
  const callers = textOf(await lspTool.execute({ op: 'incoming_calls', path: 'math.ts', line: 2, symbol: 'add' }, ctx));
  assert.match(callers, /use\.ts:3:/);
});

test('lsp (python via pyright): definition across files', { timeout: 90_000 }, async () => {
  const r = await lspTool.execute({ op: 'definition', path: 'main.py', line: 3, symbol: 'greet' }, fx('py'));
  assert.equal(textOf(r), 'lib.py:1:5  def greet(name: str) -> str:');
});

test('lsp: clear errors for bad input and unsupported files', async () => {
  const ctx = fx('ts');
  assert.match(textOf(await lspTool.execute({ op: 'hover', path: 'use.ts', line: 3, symbol: 'nope' }, ctx)), /"nope" is not on line 3/);
  assert.match(textOf(await lspTool.execute({ op: 'hover', path: 'use.ts', line: 99 }, ctx)), /past the end/);
  assert.match(textOf(await lspTool.execute({ op: 'definition', path: 'use.ts' }, ctx)), /needs line/);
  assert.match(textOf(await lspTool.execute({ op: 'hover', path: 'tsconfig.json', line: 1 }, ctx)), /no language server for \.json/);
  assert.ok((await lspTool.execute({ op: 'bogus' as any, path: 'use.ts' }, ctx)).isError);
});
