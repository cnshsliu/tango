import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtemp, readdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { formatDoctor, runDoctor } from '../src/core/doctor.js';

/** The CLI from source, in a throwaway tango home without this shell's TANGO_* variables. */
function doctor(home: string, env: Record<string, string | undefined>, ...args: string[]) {
  const clean = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith('TANGO_')));
  for (const [k, v] of Object.entries(env)) {
    if (v === undefined) delete clean[k];
    else clean[k] = v;
  }
  return spawnSync(process.execPath, ['--import', 'tsx', 'src/cli.ts', 'doctor', ...args], {
    cwd: process.cwd(),
    encoding: 'utf8',
    env: { ...clean, TANGO_HOME: home }
  });
}

const plain = (s: string) => s.replace(/\x1b\[[0-9;]*m/g, '');

test('tango doctor prints the /doctor checks and opens no session', { timeout: 60_000 }, async () => {
  const home = await mkdtemp(join(tmpdir(), 'tango-doctor-'));
  try {
    const r = doctor(home, { ZHIPU_CODING_PLAN_API_KEY: 'a-test-key-that-is-not-real-0000' });
    assert.equal(r.status, 0, r.stderr);
    const out = plain(r.stdout);
    for (const name of [
      'git',
      'TANGO_HOME writable',
      'sessions dir',
      'model key',
      'proxy',
      'node',
      'secret redaction',
      'lsp diagnostics',
      'lsp typescript',
      'lsp python',
      'lsp rust',
      'lsp go',
      'lsp c'
    ]) {
      assert.match(out, new RegExp(`[✓✗] ${name}\\b`), `a line for ${name}:\n${out}`);
    }
    assert.match(out, /✓ node\s+v\d+\.\d+/);
    assert.match(out, new RegExp(`✓ sessions dir\\s+${home.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}/sessions`), 'the temp home is the one checked');
    // no key value ever reaches the output, only whether one resolves
    assert.ok(!out.includes('a-test-key-that-is-not-real-0000'));
    const sessions = await readdir(join(home, 'sessions')).catch(() => [] as string[]);
    assert.deepEqual(sessions.filter((f) => f.endsWith('.jsonl')), [], 'doctor is not a session');
    assert.deepEqual(sessions.filter((f) => f.startsWith('doctor-')), [], 'the scratch directory is removed');
  } finally {
    await rm(home, { recursive: true, force: true });
  }
});

test('tango doctor: a missing key and a non-git directory are reported as ✗; -m picks the model', { timeout: 60_000 }, async () => {
  const home = await mkdtemp(join(tmpdir(), 'tango-doctor-'));
  try {
    const r = doctor(home, { ZHIPU_CODING_PLAN_API_KEY: undefined }, '-m', 'zhipu-plan/glm-5.4', '--cwd', home);
    assert.equal(r.status, 0, r.stderr);
    const out = plain(r.stdout);
    assert.match(out, /✗ model key\s+zhipu-plan\/glm-5\.4: /);
    assert.match(out, /ZHIPU_CODING_PLAN_API_KEY/, 'the line names the variable, never a value');
    assert.match(out, /✗ git\s+not a git repo/);
    assert.match(out, /✓ TANGO_HOME writable/);
    // a language server that is not installed says how to get it
    const go = out.split('\n').find((l) => /lsp go\b/.test(l))!;
    assert.ok(/✓|✗/.test(go));
    if (go.startsWith('✗')) assert.match(go, /not installed · /);
  } finally {
    await rm(home, { recursive: true, force: true });
  }
});

/** An environment without a single API key in it. */
function keylessEnv(home: string): NodeJS.ProcessEnv {
  const keep = /^(PATH|HOME|USER|LOGNAME|SHELL|TERM|TMPDIR|TEMP|LANG|LANGUAGE|LC_[A-Z]+|SystemRoot|ComSpec|APPDATA|LOCALAPPDATA|USERPROFILE|PATHEXT|PROGRAMFILES|PROGRAMDATA|windir)$/i;
  const env: NodeJS.ProcessEnv = Object.fromEntries(Object.entries(process.env).filter(([k]) => keep.test(k)));
  env.TANGO_HOME = home;
  return env;
}

test('tango doctor with no key anywhere reports it as the model finding and still runs every check', { timeout: 60_000 }, async () => {
  const home = await mkdtemp(join(tmpdir(), 'tango-doctor-'));
  try {
    const r = spawnSync(process.execPath, ['--import', 'tsx', 'src/cli.ts', 'doctor'], {
      cwd: process.cwd(),
      encoding: 'utf8',
      env: keylessEnv(home)
    });
    assert.equal(r.status, 0, r.stderr);
    const out = plain(r.stdout);
    assert.match(out, /✗ model key\s+\(default model\): no model configured and no API key found/);
    assert.match(out, /✓ node\s+v\d+/, 'the rest of the checks still ran');
    assert.match(out, /lsp typescript/);
    // an unknown -m is a finding too
    const u = spawnSync(process.execPath, ['--import', 'tsx', 'src/cli.ts', 'doctor', '-m', 'nope/x'], { cwd: process.cwd(), encoding: 'utf8', env: keylessEnv(home) });
    assert.equal(u.status, 0, u.stderr);
    assert.match(plain(u.stdout), /✗ model key\s+nope\/x: unknown model "nope\/x"/);
  } finally {
    await rm(home, { recursive: true, force: true });
  }
});

test('runDoctor and formatDoctor: the same checks serve sends to /doctor', { timeout: 60_000 }, async () => {
  const home = await mkdtemp(join(tmpdir(), 'tango-doctor-unit-'));
  process.env.TANGO_HOME = home;
  try {
    const checks = await runDoctor({
      cwd: home,
      model: { id: 'mock/m', provider: 'mock', model: 'm', api: 'openai-completions', baseUrl: 'http://127.0.0.1:9/v1', apiKey: 'ZHIPU_CODING_PLAN_API_KEY' } as any,
      config: { redactSecrets: false, lspDiagnostics: false }
    });
    const names = checks.map((c) => c.name);
    assert.deepEqual(names.slice(0, 8), ['git', 'TANGO_HOME writable', 'sessions dir', 'model key', 'proxy', 'node', 'secret redaction', 'lsp diagnostics']);
    assert.equal(checks.length, 8 + 5, 'one line per language server');
    assert.equal(checks.find((c) => c.name === 'secret redaction')?.detail, 'off (redactSecrets: false)');
    assert.equal(checks.find((c) => c.name === 'lsp diagnostics')?.detail, 'off (lspDiagnostics: false)');
    const text = formatDoctor(checks);
    assert.match(text, /^(✓|✗) git  /);
    assert.equal(text.split('\n').length, checks.length, 'one line per check');
    assert.ok(!/[✓✗] [a-z ]+$/.test(text.split('\n')[3]), 'the model key line carries its detail');
  } finally {
    delete process.env.TANGO_HOME;
    await rm(home, { recursive: true, force: true });
  }
});
