#!/usr/bin/env node
/**
 * Docs guard for tango itself, run as a tango hook. Goal: every feature ships with its docs.
 *
 *   UserPromptSubmit: remember where the turn started (HEAD + a hash of each dirty file).
 *   Stop: if this turn changed code (src/, tui/src/, scripts/, bin/):
 *     1. scripts/docs-check.mjs must pass (every tool, flag, config key, serve command, …
 *        named in the docs) — otherwise exit 2 with the gaps, every time;
 *     2. if no doc file changed this turn, exit 2 once with a reminder. When the model is sent
 *        back already (stop_hook_active) it may finish, e.g. after saying why no doc changes.
 *
 * Any internal error exits 0: a broken guard must never wedge a turn.
 * Usage: node scripts/hooks/docs-guard.mjs          (hook mode: event JSON on stdin)
 *        node scripts/hooks/docs-guard.mjs --check  (just the coverage check)
 */
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

const ROOT = process.env.TANGO_PROJECT_DIR || process.cwd();
const git = (...args) => spawnSync('git', args, { cwd: ROOT, encoding: 'utf8' }).stdout ?? '';

/** paths whose change can change behaviour a user sees */
export const isCode = (f) => /^(src|tui\/src|scripts|bin)\//.test(f) && !/\.test\.|\/tests?\//.test(f);
/** paths that count as documentation */
export const isDoc = (f) => f === 'README.md' || f === 'AGENTS.md' || (/^docs\/.+\.md$/.test(f) && !f.startsWith('docs/notes/'));

const hashOf = (f) => {
  try {
    return createHash('sha1').update(readFileSync(join(ROOT, f))).digest('hex');
  } catch {
    return 'gone';
  }
};

/** Dirty paths (tracked changes and untracked files) with a content hash each. */
function dirty() {
  const out = {};
  for (const line of git('status', '--porcelain', '--untracked-files=all').split('\n')) {
    if (!line.trim()) continue;
    const f = line.slice(3).split(' -> ').pop().replace(/^"|"$/g, '');
    out[f] = hashOf(f);
  }
  return out;
}

const baseFile = () => {
  const dir = git('rev-parse', '--absolute-git-dir').trim();
  return dir ? join(dir, 'docs-guard-base.json') : '';
};

export function recordBase() {
  const f = baseFile();
  if (f) writeFileSync(f, JSON.stringify({ head: git('rev-parse', 'HEAD').trim(), dirty: dirty() }));
}

/** Files changed since the turn started: new commits plus working-tree edits. */
export function changedSinceBase() {
  const f = baseFile();
  const now = dirty();
  if (!f || !existsSync(f)) return Object.keys(now); // no baseline: judge by what is dirty
  const base = JSON.parse(readFileSync(f, 'utf8'));
  const head = git('rev-parse', 'HEAD').trim();
  const changed = new Set(base.head && base.head !== head ? git('diff', '--name-only', base.head, head).split('\n').filter(Boolean) : []);
  for (const [p, h] of Object.entries(now)) if (base.dirty?.[p] !== h) changed.add(p);
  for (const p of Object.keys(base.dirty ?? {})) if (!(p in now)) changed.add(p); // committed or reverted
  return [...changed];
}

function coverageGaps() {
  const r = spawnSync(process.execPath, [join(ROOT, 'scripts/docs-check.mjs'), '--json'], { cwd: ROOT, encoding: 'utf8' });
  return JSON.parse(r.stdout || '{"gaps":[]}').gaps;
}

/** Decide for a Stop event: a message means "send the model back with it". */
export function stopVerdict(changed, gaps, stopHookActive) {
  const code = changed.filter(isCode);
  if (!code.length) return null;
  if (gaps.length) {
    const list = gaps.slice(0, 40).map((g) => `- ${g.kind} \`${g.name}\` → ${g.where}`).join('\n');
    return `docs coverage: ${gaps.length} item(s) are not documented. Add each to the doc section named:\n${list}\nCheck with: node scripts/docs-check.mjs`;
  }
  if (changed.some(isDoc) || stopHookActive) return null;
  return (
    `This turn changed code but no documentation:\n${code.slice(0, 15).map((f) => `- ${f}`).join('\n')}\n` +
    'If behaviour, commands, tools, flags, config keys or the serve protocol changed, update README.md ' +
    '(reference) and docs/handbook.md (user guide; slash commands also need SLASH in tui/src/app.rs). ' +
    'If nothing user-visible changed, say so in one line and finish.'
  );
}

async function main() {
  if (!existsSync(join(ROOT, 'scripts/docs-check.mjs'))) return 0; // not a tango checkout
  if (process.argv.includes('--check')) {
    const gaps = coverageGaps();
    for (const g of gaps) console.log(`${g.kind}: ${g.name} → ${g.where}`);
    return gaps.length ? 1 : 0;
  }
  let raw = '';
  for await (const chunk of process.stdin) raw += chunk;
  const ev = raw.trim() ? JSON.parse(raw) : {};
  if (ev.hook_event_name === 'UserPromptSubmit') {
    recordBase();
    return 0;
  }
  if (ev.hook_event_name !== 'Stop') return 0;
  const msg = stopVerdict(changedSinceBase(), coverageGaps(), ev.stop_hook_active === true);
  if (!msg) return 0;
  process.stderr.write(msg + '\n');
  return 2;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().then(
    (code) => (process.exitCode = code),
    (e) => {
      process.stderr.write(`docs-guard: ${e?.message ?? e} (ignored)\n`);
      process.exitCode = 0;
    }
  );
}
