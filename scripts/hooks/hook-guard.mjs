#!/usr/bin/env node
/**
 * Repo-local guard for tango's own hook code, itself run as a tango hook.
 *
 *   PostToolUse (edit|write): an edit to hook code → check every HOOK_EVENTS entry has an
 *     end-to-end test named "<Event>: …" in tests/hooks-e2e.test.ts; missing → exit 2.
 *   Stop: hook code changed (uncommitted, or committed but not pushed) → coverage check
 *     plus every hook test; any failure → exit 2 so the model goes back and fixes it.
 *
 * Outside the tango repo (no src/core/hooks.ts in the cwd) it does nothing.
 * Usage: node scripts/hooks/hook-guard.mjs [--check]   (--check: run everything, no stdin)
 */
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, readdirSync, readFileSync, realpathSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
// Run directly? `import.meta.url === file://…argv[1]` never holds on Windows (backslashes, drive
// letter case), which left every one of these scripts doing nothing there: real paths compared.
const isMain = (() => {
  try {
    return realpathSync.native(fileURLToPath(import.meta.url)) === realpathSync.native(process.argv[1]);
  } catch {
    return false;
  }
})();

const ROOT = realpathSync(process.cwd());
/** Resolve symlinks (macOS /var → /private/var) so paths compare; a new file → its dir's real path. */
const real = (p) => {
  try {
    return realpathSync(p);
  } catch {
    return join(real(dirname(p)), p.slice(dirname(p).length + 1));
  }
};
const HOOKS_SRC = join(ROOT, 'src/core/hooks.ts');
const E2E = join(ROOT, 'tests/hooks-e2e.test.ts');
/** files whose change means "hook code changed" */
export const HOOK_PATHS = ['src/core/hooks.ts', 'src/host.ts', 'src/core/agent.ts', 'src/core/types.ts', 'tests/hooks.test.ts', 'tests/hooks-e2e.test.ts', 'tests/hook-guard.test.ts', 'tests/fixtures/hooks', 'scripts/hooks'];
const HOOK_TESTS = ['tests/hooks.test.ts', 'tests/hooks-e2e.test.ts', 'tests/hook-guard.test.ts'];

/** Events declared in HOOK_EVENTS that have no e2e test titled "<Event>: …". */
export function uncoveredEvents(hooksSrc, e2eSrc) {
  const m = /HOOK_EVENTS\s*=\s*\[([^\]]*)\]/.exec(hooksSrc);
  if (!m) return ['(HOOK_EVENTS not found in src/core/hooks.ts)'];
  const events = [...m[1].matchAll(/'([A-Za-z]+)'/g)].map((x) => x[1]);
  return events.filter((ev) => !new RegExp(`test\\(\\s*['"\`]${ev}:`).test(e2eSrc));
}

function coverage() {
  const missing = uncoveredEvents(readFileSync(HOOKS_SRC, 'utf8'), existsSync(E2E) ? readFileSync(E2E, 'utf8') : '');
  if (!missing.length) return null;
  return (
    `hook events without an end-to-end test: ${missing.join(', ')}.\n` +
    `Add at least one test titled "<Event>: …" to tests/hooks-e2e.test.ts for each (a blocking case with exit 2 ` +
    `and a payload case via tests/fixtures/hooks/record.sh), then run: npm run test:hooks`
  );
}

function runHookTests() {
  const r = spawnSync(process.execPath, ['--import', 'tsx', '--test', ...HOOK_TESTS], { cwd: ROOT, encoding: 'utf8' });
  if (r.status === 0) return null;
  const out = `${r.stdout}\n${r.stderr}`;
  const fails = out.split('\n').filter((l) => /^not ok|^\s+(error|expected|actual):/.test(l)).slice(0, 30).join('\n');
  return `hook tests failed (npm run test:hooks):\n${fails || out.slice(-2000)}`;
}

const git = (...args) => spawnSync('git', args, { cwd: ROOT, encoding: 'utf8' }).stdout ?? '';

/** Hook files changed in the working tree, or in commits not yet on the upstream. */
function changedHookFiles() {
  const dirty = git('status', '--porcelain', '--', ...HOOK_PATHS).split('\n').filter(Boolean).map((l) => l.slice(3));
  const hasUpstream = spawnSync('git', ['rev-parse', '@{u}'], { cwd: ROOT }).status === 0;
  const ahead = hasUpstream ? git('diff', '--name-only', '@{u}', 'HEAD', '--', ...HOOK_PATHS).split('\n').filter(Boolean) : [];
  return [...new Set([...dirty, ...ahead])];
}

/** Content hash of every hook file: the cache key for "already verified". */
function hookHash() {
  const h = createHash('sha256');
  const walk = (p) => {
    const abs = join(ROOT, p);
    if (!existsSync(abs)) return;
    if (statSync(abs).isDirectory()) for (const f of readdirSync(abs).sort()) walk(join(p, f));
    else h.update(p + '\0').update(readFileSync(abs));
  };
  HOOK_PATHS.forEach(walk);
  return h.digest('hex');
}
/**
 * Where git keeps this checkout's metadata: `.git` in a plain clone, the `.git/worktrees/<name>`
 * directory `git worktree` made (its `.git` is a file, so nothing can be written under it), or
 * null outside a repository.
 */
export function gitDir(cwd = process.cwd()) {
  const r = spawnSync('git', ['rev-parse', '--git-dir'], { cwd, encoding: 'utf8' });
  if (r.status !== 0) return null;
  const dir = resolve(cwd, r.stdout.trim());
  return existsSync(dir) && statSync(dir).isDirectory() ? dir : null;
}
const STAMP = gitDir() && join(gitDir(), 'hook-guard-ok');
const verified = () => !!STAMP && existsSync(STAMP) && readFileSync(STAMP, 'utf8') === hookHash();
const markVerified = () => !!STAMP && writeFileSync(STAMP, hookHash());

const isHookPath = (p) => HOOK_PATHS.some((h) => p === h || p.startsWith(h + '/'));

function block(reason) {
  process.stderr.write(reason + '\n');
  process.exit(2);
}

function main() {
  if (!existsSync(HOOKS_SRC)) return; // not the tango repo
  if (process.argv.includes('--check')) {
    const problem = coverage() ?? runHookTests();
    if (problem) block(problem);
    markVerified();
    process.stdout.write('hook coverage ok, hook tests pass\n');
    return;
  }
  let ev = {};
  try {
    ev = JSON.parse(readFileSync(0, 'utf8') || '{}');
  } catch {
    return;
  }
  if (ev.hook_event_name === 'PostToolUse') {
    const p = ev.tool_input?.path;
    if (typeof p !== 'string') return;
    const rel = relative(ROOT, real(resolve(ev.cwd ?? ROOT, p)));
    if (!isHookPath(rel)) return;
    const problem = coverage();
    if (problem) block(problem);
    return;
  }
  if (ev.hook_event_name === 'Stop') {
    if (!changedHookFiles().length || verified()) return;
    const problem = coverage() ?? runHookTests();
    if (!problem) return markVerified();
    // keeps blocking while it fails; tango stops honoring a Stop hook after 3 sends
    block(problem);
  }
}

if (isMain) main();
