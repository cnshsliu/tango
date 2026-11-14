import { readFile, readdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { tangoDir } from './config.js';
import { skillsDir } from './skills.js';

export interface SkillInfo {
  name: string;
  description: string;
  path: string;
}

const BASE_PROMPT = `You are Tango, a work agent: you get things done with code, files and the open web.

Working rules:
- Act, don't narrate. If a tool can do it now, do it instead of describing what you would do.
- Read before you edit. Prefer edit for targeted changes, write for new files.
- Verify before declaring done: run the tests, re-read what you wrote, check the output.
- Be concise in chat. Put long detail into files when the user asked for a deliverable.
- If the request is ambiguous and a wrong guess is cheap to undo, guess and say so. Ask only when guessing is expensive.`;

/** Parse leading `---` frontmatter into key/value pairs. */
function frontmatter(src: string): Record<string, string> {
  const m = /^---\r?\n([\s\S]*?)\r?\n---/.exec(src);
  const out: Record<string, string> = {};
  if (!m) return out;
  for (const line of m[1].split('\n')) {
    const i = line.indexOf(':');
    if (i > 0) out[line.slice(0, i).trim()] = line.slice(i + 1).trim();
  }
  return out;
}

/**
 * Skills live in <root>/<name>/SKILL.md, global (~/.tango/skills) and project (.tango/skills).
 * Only name + description go into the system prompt; the model reads the file when it matches.
 */
export async function discoverSkills(cwd: string): Promise<SkillInfo[]> {
  const roots = [skillsDir(), join(cwd, '.tango', 'skills')];
  const skills: SkillInfo[] = [];
  const seen = new Set<string>();
  for (const root of roots) {
    if (!existsSync(root)) continue;
    for (const dir of await readdir(root)) {
      const file = join(root, dir, 'SKILL.md');
      if (!existsSync(file)) continue;
      const fm = frontmatter(await readFile(file, 'utf8'));
      const name = fm.name || dir;
      if (seen.has(name)) continue;
      seen.add(name);
      skills.push({ name, description: fm.description ?? '', path: file });
    }
  }
  return skills;
}

/** AGENTS.md from cwd upward to the filesystem root, plus global ~/.tango/AGENTS.md. Global first, nearest last. */
export async function loadAgentsFiles(cwd: string): Promise<string[]> {
  const local: string[] = [];
  let dir = cwd;
  while (true) {
    const f = join(dir, 'AGENTS.md');
    if (existsSync(f)) {
      const body = (await readFile(f, 'utf8')).trim();
      if (body) local.unshift(body);
    }
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  const out: string[] = [];
  const global = join(tangoDir(), 'AGENTS.md');
  if (existsSync(global)) {
    const body = (await readFile(global, 'utf8')).trim();
    if (body) out.push(body);
  }
  out.push(...local);
  return out;
}

export interface ContextInfo {
  systemPrompt: string;
  skills: SkillInfo[];
}

/** Assemble the system prompt: base + AGENTS.md + skills index + environment. */
export async function buildContext(cwd: string): Promise<ContextInfo> {
  const parts = [BASE_PROMPT];

  const agents = await loadAgentsFiles(cwd);
  if (agents.length) parts.push('# Project instructions\n\n' + agents.join('\n\n---\n\n'));

  const skills = await discoverSkills(cwd);
  if (skills.length) {
    const lines = skills.map((s) => `- ${s.name}: ${s.description} (at ${s.path}; read it before following it)`);
    parts.push('# Skills\n\nAvailable skills. When one matches the task, read its SKILL.md and follow it:\n\n' + lines.join('\n'));
  }

  parts.push(
    '# Environment\n\n' +
      `cwd: ${cwd}\n` +
      `platform: ${process.platform}\n` +
      `date: ${new Date().toISOString().slice(0, 10)}`
  );

  return { systemPrompt: parts.join('\n\n'), skills };
}
