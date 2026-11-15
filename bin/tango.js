#!/usr/bin/env node
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const dist = join(here, '..', 'dist', 'cli.js');

if (!existsSync(dist)) {
  console.error('tango: not built yet. Run `npm run build`, or during development use `npm run dev -- ...`.');
  process.exit(1);
}

await import(pathToFileURL(dist).href);
