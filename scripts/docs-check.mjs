#!/usr/bin/env node
/**
 * Docs coverage: everything a user or another agent can touch must be named in the docs.
 * Inventories come from the source; "documented" means the name appears in backticks (or in a
 * fenced block) in the doc section listed for it. Slash commands are checked by the cargo test
 * `slash_commands_stay_in_sync_with_help_and_docs`; hook-event tests by hook-guard.
 *
 *   node scripts/docs-check.mjs          → prints gaps, exit 1 when any
 *   node scripts/docs-check.mjs --json   → {gaps:[{kind,name,where}]}
 */
import { readFileSync, readdirSync, realpathSync } from 'node:fs';
import { dirname, join } from 'node:path';
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

const root = process.env.DOCS_CHECK_ROOT ?? join(dirname(fileURLToPath(import.meta.url)), '..');
const read = (p) => readFileSync(join(root, p), 'utf8');
const uniq = (xs) => [...new Set(xs)].sort();

/** names that are deliberately undocumented, with the reason */
export const EXEMPT = {
  env: {
    TANGO_TEST_LOOP_FLOOR: 'test-only knob',
    // Node's own, read by env-proxy.ts only to avoid a redundant --disable-warning on the relaunch
    NODE_NO_WARNINGS: "Node's own; checked, not configured by tango",
    NODE_OPTIONS: "Node's own; checked, not configured by tango"
  }
};

/** Text of `## title` up to the next `## ` heading (whole file when title is null). */
export function section(md, title) {
  if (!title) return md;
  const start = md.search(new RegExp(`^## ${title.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\s*$`, 'm'));
  if (start < 0) return '';
  const rest = md.slice(start + 3);
  const end = rest.search(/^## /m);
  return end < 0 ? rest : rest.slice(0, end);
}

/** `name` in backticks, or a fenced block line that contains it as a word. */
export function mentions(text, name) {
  const esc = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  if (new RegExp('`[^`\\n]*(?<![\\w-])' + esc + '(?![\\w-])[^`\\n]*`').test(text)) return true;
  const fences = text.match(/```[\s\S]*?```/g) ?? [];
  return fences.some((f) => new RegExp('(?<![\\w-])' + esc + '(?![\\w-])').test(f));
}

const tsFiles = (dir) =>
  readdirSync(join(root, dir), { recursive: true })
    .map(String)
    .filter((f) => f.endsWith('.ts'))
    .map((f) => join(dir, f));
const grab = (text, re) => [...text.matchAll(re)].map((m) => m[1]);

/** What the source exposes, by kind: where it must be documented and what is in it. */
export function inventory() {
  const cli = read('src/cli.ts');
  const serve = read('src/serve.ts');
  const config = read('src/core/config.ts');
  const iface = config.slice(config.indexOf('export interface Config {'));
  const configBody = iface.slice(0, iface.search(/^\}/m));
  const allSrc = tsFiles('src').map(read).join('\n');
  const keys = read('tui/src/keys.rs');
  return [
    { kind: 'tool', doc: 'README.md', section: null, names: grab(tsFiles('src/core/tools').map(read).join('\n'), /^\s*name: '([a-z_]+)'/gm) },
    { kind: 'cli flag', doc: 'README.md', section: 'CLI', names: grab(cli, /'(--[a-z][a-z-]*)'/g) },
    { kind: 'cli subcommand', doc: 'README.md', section: 'CLI', names: [...grab(cli, /rest\[0\] === '([a-z-]+)'/g), ...grab(cli, /sub === '([a-z-]+)'/g).map((s) => `task ${s}`)] },
    { kind: 'config key', doc: 'README.md', section: 'Config', names: grab(configBody, /^  ([a-zA-Z]+)\??:/gm) },
    { kind: 'serve command', doc: 'README.md', section: 'Serve protocol (the UI seam)', names: [...grab(serve, /case '([a-z_]+)':/g), ...grab(serve, /cmd\.cmd === '([a-z_]+)'/g)] },
    { kind: 'serve frame', doc: 'README.md', section: 'Serve protocol (the UI seam)', names: grab(serve, /type: '([a-z_]+)'/g).filter((t) => t !== 'text') },
    { kind: 'env var', doc: 'README.md', section: null, names: grab(allSrc, /(?:process\.)?env\.([A-Z][A-Z0-9_]+)/g).filter((n) => !EXEMPT.env[n]) },
    { kind: 'hook event', doc: 'README.md', section: 'Hooks', names: grab(read('src/core/hooks.ts'), /HOOK_EVENTS = \[([^\]]*)\]/g).flatMap((s) => grab(s, /'([A-Za-z]+)'/g)) },
    { kind: 'key action', doc: 'README.md', section: null, names: grab(keys, /Action::[A-Za-z]+ => "([a-zA-Z]+)"/g) }
  ].map((k) => ({ ...k, names: uniq(k.names) }));
}

export function gaps() {
  const docs = new Map();
  const out = [];
  for (const k of inventory()) {
    if (!docs.has(k.doc)) docs.set(k.doc, read(k.doc));
    const text = section(docs.get(k.doc), k.section);
    const where = k.section ? `${k.doc} "## ${k.section}"` : k.doc;
    for (const name of k.names) if (!mentions(text, name)) out.push({ kind: k.kind, name, where });
  }
  return out;
}

if (isMain) {
  const found = gaps();
  if (process.argv.includes('--json')) console.log(JSON.stringify({ gaps: found }));
  else if (!found.length) console.log('docs coverage ok');
  else {
    console.log(`docs coverage: ${found.length} undocumented`);
    for (const g of found) console.log(`  ${g.kind.padEnd(14)} ${g.name.padEnd(22)} → ${g.where}`);
  }
  process.exitCode = found.length ? 1 : 0;
}
