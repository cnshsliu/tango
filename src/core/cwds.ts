import { existsSync, mkdirSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tangoDir } from './config.js';

/** Newest first. Older entries fall off. */
const MAX = 30;

const file = () => join(tangoDir(), 'cwds.json');

function canon(dir: string): string | undefined {
  try {
    if (!dir || !existsSync(dir)) return;
    return realpathSync(dir);
  } catch {
    return;
  }
}

function readRaw(): string[] {
  try {
    const v = JSON.parse(readFileSync(file(), 'utf8'));
    if (!Array.isArray(v)) return [];
    return v.filter((x): x is string => typeof x === 'string');
  } catch {
    return [];
  }
}

/** Remember a directory the user actually worked in. Missing directories are not stored. */
export function rememberCwd(dir: string): void {
  const abs = canon(dir);
  if (!abs) return;
  const next = [abs, ...readRaw().filter((p) => p !== abs)].slice(0, MAX);
  mkdirSync(tangoDir(), { recursive: true });
  writeFileSync(file(), JSON.stringify(next, null, 2) + '\n');
}

/**
 * Directories the user can switch to, newest first. Only paths that exist now.
 * `extra` (other sessions' cwds) fills anything not already remembered.
 */
export function listCwds(extra: string[] = []): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  for (const dir of [...readRaw(), ...extra]) {
    const abs = canon(dir);
    if (!abs || seen.has(abs)) continue;
    seen.add(abs);
    out.push(abs);
  }
  return out;
}
