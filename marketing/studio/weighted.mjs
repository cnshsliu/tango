#!/usr/bin/env node
/**
 * X weighted length per post of a thread draft (classic rules: CJK and most non-Latin
 * characters count 2, URLs count 23, limit 280).
 *
 *   node weighted.mjs ../posts/<dir>/zh-thread.txt
 *
 * The draft uses "--- n/m ---" separators; lines starting with "#" are header notes and
 * "[媒体: …]" lines name the attachment, neither is part of the posted text.
 */
import { readFileSync } from 'node:fs';

const LIGHT = [[0, 4351], [8192, 8205], [8208, 8223], [8242, 8247]];
const weigh = (s) => {
  let n = 0;
  const rest = s.replace(/https?:\/\/\S+/g, () => ((n += 23), ''));
  for (const ch of rest) {
    const c = ch.codePointAt(0);
    n += LIGHT.some(([a, b]) => c >= a && c <= b) ? 1 : 2;
  }
  return n;
};

export function posts(text) {
  return text
    .split(/^--- \d+\/\d+ ---$/m)
    .slice(1)
    .map((p) => p.split('\n').filter((l) => !/^\[媒体[:：]/.test(l)).join('\n').trim());
}

const file = process.argv[2];
if (!file) {
  console.error('usage: weighted.mjs <zh-thread.txt>');
  process.exit(2);
}
let bad = 0;
posts(readFileSync(file, 'utf8')).forEach((p, i) => {
  const w = weigh(p);
  if (w > 280) bad++;
  const flags = [w > 280 && 'OVER 280', /本机/.test(p) && '含「本机」', /\b\d+\/\d+\b/.test(p.slice(0, 6)) && '1/n 编号'].filter(Boolean);
  console.log(`${i + 1}: ${w}${flags.length ? '  ← ' + flags.join(', ') : ''}`);
});
process.exit(bad ? 1 : 0);
