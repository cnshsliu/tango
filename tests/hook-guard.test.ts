import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdir, mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
// @ts-expect-error plain .mjs script, no types
import { uncoveredEvents } from '../scripts/hooks/hook-guard.mjs';

const GUARD = join(import.meta.dirname, '..', 'scripts', 'hooks', 'hook-guard.mjs');

test('hook-guard: finds HOOK_EVENTS entries without a "<Event>: …" e2e test', () => {
  const src = `export const HOOK_EVENTS = ['PreToolUse', 'Stop', 'SessionStart'] as const;`;
  const e2e = `test('PreToolUse: blocks', …)\ntest("Stop: continues", …)\ntest('a SessionStart thing', …)`;
  assert.deepEqual(uncoveredEvents(src, e2e), ['SessionStart'], 'the title must start with the event name');
  assert.deepEqual(uncoveredEvents(src, e2e + `\ntest(\`SessionStart: fires\`, …)`), []);
  assert.equal(uncoveredEvents('nothing here', '').length, 1, 'a missing HOOK_EVENTS is reported, not ignored');
});

/** A throwaway repo shaped like tango: src/core/hooks.ts + tests/hooks-e2e.test.ts. */
async function fakeRepo(events: string[], tested: string[]) {
  const root = await mkdtemp(join(tmpdir(), 'tango-guard-'));
  await mkdir(join(root, 'src/core'), { recursive: true });
  await mkdir(join(root, 'tests'), { recursive: true });
  await writeFile(join(root, 'src/core/hooks.ts'), `export const HOOK_EVENTS = [${events.map((e) => `'${e}'`).join(', ')}] as const;\n`);
  await writeFile(join(root, 'tests/hooks-e2e.test.ts'), tested.map((e) => `test('${e}: x', () => {});`).join('\n'));
  return root;
}
const guard = (root: string, event: object) =>
  spawnSync(process.execPath, [GUARD], { cwd: root, input: JSON.stringify(event), encoding: 'utf8' });

test('hook-guard PostToolUse: editing hook code with an untested event blocks with instructions', async () => {
  const root = await fakeRepo(['PreToolUse', 'SessionStart'], ['PreToolUse']);
  try {
    const r = guard(root, { hook_event_name: 'PostToolUse', cwd: root, tool_name: 'edit', tool_input: { path: 'src/core/hooks.ts' } });
    assert.equal(r.status, 2);
    assert.match(r.stderr, /without an end-to-end test: SessionStart/);
    assert.match(r.stderr, /tests\/hooks-e2e\.test\.ts/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('hook-guard PostToolUse: unrelated files and covered events pass', async () => {
  const root = await fakeRepo(['PreToolUse', 'SessionStart'], ['PreToolUse']);
  try {
    const other = guard(root, { hook_event_name: 'PostToolUse', cwd: root, tool_input: { path: 'README.md' } });
    assert.equal(other.status, 0, 'not hook code: silent');
    const abs = guard(root, { hook_event_name: 'PostToolUse', cwd: root, tool_input: { path: join(root, 'src/core/hooks.ts') } });
    assert.equal(abs.status, 2, 'absolute paths are recognized too');
    await writeFile(join(root, 'tests/hooks-e2e.test.ts'), `test('PreToolUse: a', () => {});\ntest('SessionStart: b', () => {});`);
    const ok = guard(root, { hook_event_name: 'PostToolUse', cwd: root, tool_input: { path: 'src/core/hooks.ts' } });
    assert.equal(ok.status, 0);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('hook-guard Stop: dirty hook code that lacks coverage sends the model back', async () => {
  const root = await fakeRepo(['Stop'], []);
  try {
    spawnSync('git', ['init', '-q', '-b', 'main'], { cwd: root });
    const r = guard(root, { hook_event_name: 'Stop', cwd: root, stop_hook_active: false });
    assert.equal(r.status, 2);
    assert.match(r.stderr, /without an end-to-end test: Stop/);
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('hook-guard: outside a tango checkout it does nothing', async () => {
  const root = await mkdtemp(join(tmpdir(), 'tango-guard-other-'));
  try {
    const r = guard(root, { hook_event_name: 'Stop', cwd: root });
    assert.equal(r.status, 0);
    assert.equal(r.stderr, '');
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});

test('hook-guard as a real tango PostToolUse hook: the model is told to add the missing test', async () => {
  const { createServer } = await import('node:http');
  const { makeAgent } = await import('../src/host.js');
  const { SessionStore } = await import('../src/core/session.js');
  const root = await fakeRepo(['PreToolUse', 'SessionStart'], ['PreToolUse']);
  const bodies: any[] = [];
  const sse = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      bodies.push(JSON.parse(raw));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      if (bodies.length === 1) {
        const args = JSON.stringify({ path: 'src/core/hooks.ts', old: "'SessionStart'", new: "'SessionStart'" });
        res.end(sse({ choices: [{ delta: { tool_calls: [{ index: 0, id: 'e1', function: { name: 'edit', arguments: args } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n');
      } else res.end(sse({ choices: [{ delta: { content: 'ok' }, finish_reason: 'stop' }] }) + 'data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  try {
    const port = (server.address() as import('node:net').AddressInfo).port;
    const store = await SessionStore.create(join(root, '.sessions'), root);
    const agent = makeAgent(
      { id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${port}/v1`, apiKey: 't' },
      store, root, { mode: 'auto' },
      { checkpoint: 'off', hooks: { PostToolUse: [{ matcher: 'edit|write', command: `node '${GUARD}'` }] } }
    );
    await agent.prompt('add an event');
    const tool = bodies[1].messages.find((m: any) => m.role === 'tool');
    assert.match(tool.content, /\[PostToolUse hook\] hook events without an end-to-end test: SessionStart/);
  } finally {
    server.close();
    await rm(root, { recursive: true, force: true });
  }
});
