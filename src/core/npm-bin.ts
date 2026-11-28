/**
 * How to run a command that npm installed in a project.
 *
 * macOS and Linux: `node_modules/.bin/<name>` is a POSIX shim, spawned as it is.
 *
 * Windows: that shim is a `.cmd` file, which Node refuses to spawn without a shell since
 * CVE-2024-27980, and the extensionless file next to it is a shell script Windows cannot run.
 * There the package's own entry script is found through its package.json (`bin`) and run with
 * node: `tsc` is `typescript/bin/tsc`, `typescript-language-server` is `lib/cli.mjs` of its
 * package, `pyright-langserver` lives in the `pyright` package. A global install is looked up
 * the same way under `%APPDATA%\npm`.
 */
import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { IS_WINDOWS } from './shell.js';

/** The entry script `name` runs, among the packages under `mods` (a node_modules directory). */
function binEntry(mods: string, name: string): string | undefined {
  const candidates: string[] = [join(mods, name)];
  try {
    for (const d of readdirSync(mods, { withFileTypes: true })) {
      // pnpm and others link package directories: a symlink is a package here too
      if (!(d.isDirectory() || d.isSymbolicLink()) || d.name.startsWith('.')) continue;
      if (d.name.startsWith('@')) {
        for (const s of readdirSync(join(mods, d.name))) candidates.push(join(mods, d.name, s));
      } else {
        candidates.push(join(mods, d.name));
      }
    }
  } catch {}
  for (const dir of candidates) {
    try {
      const pkg = JSON.parse(readFileSync(join(dir, 'package.json'), 'utf8'));
      const bin = pkg.bin;
      // a string bin is named after the package (`pyright-langserver` needs the object form)
      const rel =
        typeof bin === 'string'
          ? pkg.name === name || String(pkg.name).endsWith(`/${name}`)
            ? bin
            : undefined
          : bin && typeof bin === 'object' && typeof bin[name] === 'string'
            ? bin[name]
            : undefined;
      if (!rel) continue;
      const entry = join(dir, rel);
      if (existsSync(entry)) return entry;
    } catch {}
  }
  return undefined;
}

/**
 * The command that runs npm bin `name` as installed in `root` (its node_modules, or a parent's):
 * the shim itself on macOS and Linux, `node <entry>` on Windows. `extra` follows.
 */
export function npmBinCommand(root: string, name: string, extra: string[] = []): [string, string[]] | undefined {
  if (!IS_WINDOWS) {
    const shim = join(root, 'node_modules', '.bin', name);
    return existsSync(shim) ? [shim, extra] : undefined;
  }
  const entry =
    binEntry(join(root, 'node_modules'), name) ??
    (process.env.APPDATA ? binEntry(join(process.env.APPDATA, 'npm', 'node_modules'), name) : undefined);
  return entry ? [process.execPath, [entry, ...extra]] : undefined;
}
