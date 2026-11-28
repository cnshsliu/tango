import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { existsSync, utimesSync } from 'node:fs';
import { chmod, mkdtemp, readdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Agent } from '../src/core/agent.js';
import { SessionStore } from '../src/core/session.js';
import { ToolRegistry } from '../src/core/tools/registry.js';
import type { ModelSpec } from '../src/core/types.js';

const model = { id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: 'http://127.0.0.1:9/v1', apiKey: 'test' } as ModelSpec;
const meta = (id: string) => JSON.stringify({ kind: 'meta', id, createdAt: '2026-01-01T00:00:00.000Z', cwd: '/work' }) + '\n';
const files = async (dir: string) => (await readdir(dir)).sort();
const aged = (file: string) => utimesSync(file, new Date('2026-01-01T00:00:00Z'), new Date('2026-01-01T00:00:00Z'));

test('a session nothing was said in is not listed, and end() removes its file', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-empty-'));
  try {
    const fresh = await SessionStore.create(dir, '/work');
    await fresh.setModel('fake/m'); // a model switch alone is not content
    assert.ok(existsSync(fresh.file), 'the file exists while the session is open');
    assert.deepEqual(await SessionStore.list(dir), [], 'but it is not a session yet');
    assert.equal(await SessionStore.open(dir, fresh.id).then((s) => s.holdsContent()), false);

    const used = await SessionStore.create(dir, '/work');
    await used.appendMessage({ role: 'user', content: [{ type: 'text', text: 'hi' }] });
    const mailed = await SessionStore.create(dir, '/work');
    await mailed.append({ kind: 'mail', ts: new Date().toISOString(), from: 'other', text: 'hello there' });
    assert.deepEqual(
      (await SessionStore.list(dir)).map((s) => s.id).sort(),
      [used.id, mailed.id].sort(),
      'what was said in it, or sent to it, makes it a session'
    );

    // every exit path runs end(): a never-prompted agent takes its empty file with it
    const tools = new ToolRegistry();
    const a1 = new Agent({ store: fresh, tools, cwd: dir, model });
    await a1.end('exit');
    assert.ok(!existsSync(fresh.file), 'the unused session is gone');
    await a1.end('exit'); // idempotent
    assert.equal(await fresh.removeIfEmpty(), false, 'and there is nothing left to remove');

    // a session that holds something stays, also when this process resumed it without prompting
    const a2 = new Agent({ store: used, tools, cwd: dir, model });
    await a2.end('sigterm');
    assert.ok(existsSync(used.file));
    assert.equal(await used.removeIfEmpty(), false);
    assert.ok(existsSync(mailed.file));
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('sweepEmpty removes old empty files and spares fresh, used and kept ones', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-sweep-'));
  try {
    const w = async (name: string, body: string) => {
      await writeFile(join(dir, name), body);
      await aged(join(dir, name));
    };
    // starts that were killed before they could clean up: meta only, and one that only switched model
    await w('a.jsonl', meta('a'));
    await w('b.jsonl', meta('b') + JSON.stringify({ kind: 'model', ts: '2026-01-01T00:00:01.000Z', modelId: 'x/y' }) + '\n');
    // a used one, and one a /send reached before its first prompt
    await w('d.jsonl', meta('d') + JSON.stringify({ kind: 'message', ts: '2026-01-01T00:00:01.000Z', message: { role: 'user', content: [] } }) + '\n');
    await w('f.jsonl', meta('f') + JSON.stringify({ kind: 'mail', ts: '2026-01-01T00:00:01.000Z', from: 'x', text: 'hi' }) + '\n');
    // the one this process has open (kept by id), a torn file, and a non-session file
    await w('e.jsonl', meta('e'));
    await w('g.jsonl', '{"kind":"meta","id":"g"');
    await w('notes.txt', 'not a session');
    // a start another tango made a moment ago: not ours to judge yet
    await writeFile(join(dir, 'c.jsonl'), meta('c'));

    assert.equal(await SessionStore.sweepEmpty(dir, { keep: 'e' }), 2, 'a and b');
    assert.deepEqual(await files(dir), ['c.jsonl', 'd.jsonl', 'e.jsonl', 'f.jsonl', 'g.jsonl', 'notes.txt']);

    // a sweep that does not know about e, with no age limit: e and c go too
    assert.equal(await SessionStore.sweepEmpty(dir, { keepAgeMs: 0 }), 2, 'c and e');
    assert.deepEqual(await files(dir), ['d.jsonl', 'f.jsonl', 'g.jsonl', 'notes.txt']);
    assert.equal(await SessionStore.sweepEmpty(dir, { keepAgeMs: 0 }), 0);
    assert.equal(await SessionStore.sweepEmpty(join(dir, 'no-such-dir')), 0);

    // an unreadable file is left alone rather than failing the sweep
    await w('z.jsonl', meta('z'));
    await chmod(join(dir, 'z.jsonl'), 0o000);
    try {
      assert.equal(await SessionStore.sweepEmpty(dir, { keepAgeMs: 0 }), 0);
      assert.ok(existsSync(join(dir, 'z.jsonl')));
    } finally {
      await chmod(join(dir, 'z.jsonl'), 0o600);
    }
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('append() after a sweep restarts the log with its meta line, so the session stays visible', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-restart-'));
  try {
    const store = await SessionStore.create(dir, '/work', 'named');
    await rm(store.file); // what a sweep does to a session that sat unused for a while
    await store.appendMessage({ role: 'user', content: [{ type: 'text', text: 'late' }] });
    const reopened = await SessionStore.open(dir, store.id);
    assert.equal(reopened.entries.length, 2, 'meta, then the message');
    const [listed, extra] = await SessionStore.list(dir);
    assert.equal(extra, undefined);
    assert.equal(listed.id, store.id);
    assert.equal(listed.name, 'named');
    assert.equal(listed.messages, 1);
    assert.equal(listed.topic, 'late');
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('cli: a bare -r hands the TUI the pick flag; with an id it resumes that one', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-pick-'));
  const fake = join(dir, 'fake-tui');
  try {
    // a stand-in for the TUI binary: it reports the environment launchTui prepared
    await writeFile(fake, '#!/bin/sh\necho "pick=${TANGO_PICK_SESSION:-} resume=${TANGO_RESUME:-} model=${TANGO_MODEL:-}"\n');
    await chmod(fake, 0o755);
    const run = (...args: string[]) =>
      spawnSync(process.execPath, ['--import', 'tsx', 'src/cli.ts', 'tui', ...args], {
        cwd: process.cwd(),
        encoding: 'utf8',
        // a test may run inside a tango session whose TANGO_* must not leak into the launch
        env: {
          ...Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('TANGO_'))),
          TANGO_TUI: fake,
          TANGO_HOME: dir
        }
      });
    assert.equal(run('-r').stdout.trim(), 'pick=1 resume= model=', 'bare -r: pick first');
    assert.equal(run('--resume').stdout.trim(), 'pick=1 resume= model=');
    assert.equal(run('-r', 'abc123def456').stdout.trim(), 'pick= resume=abc123def456 model=');
    assert.equal(run('-r', '-m', 'x/y').stdout.trim(), 'pick=1 resume= model=x/y', 'a flag after -r is not taken for an id');
    // -c is not a pick: with no session to continue it is the usual error
    const cont = run('-c', '--model', 'a/b');
    assert.equal(cont.status, 1);
    assert.match(cont.stderr, /no sessions to continue yet/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
