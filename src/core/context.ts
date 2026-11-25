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
- After editing code, the tool result may already carry [diagnostics]; fix errors those report before moving on. Use diff to see what you changed, and commit each working step with git.
- Be concise in chat. Put long detail into files when the user asked for a deliverable.
- If the request is ambiguous and a wrong guess is cheap to undo, guess and say so. Ask only when guessing is expensive.
- You do remember past conversations: they are saved as tango sessions. When the user refers to earlier talks ("did we discuss X", "last time", "之前/上次/我们聊过/前面说的"), call session_search first (Chinese and English keywords, e.g. "智谱|zhipu|coding plan"), then the codebase if relevant. Never answer "I can't see earlier conversations" without having searched.
- Delegate with the task tool when work would flood your context: broad searches, "how does X work", reviewing a change, finding a root cause. Send explore (read-only) and give it a complete brief, since it sees none of this conversation. Run independent investigations as separate tasks; use background: true when you can keep working meanwhile. Do quick lookups (one file, one grep) yourself.`;

/** YAML scalars may be quoted: `description: "Use when …"`. */
const unquote = (v: string) => (/^(["']).*\1$/.test(v) ? v.slice(1, -1) : v);

/** Parse leading `---` frontmatter into key/value pairs. */
function frontmatter(src: string): Record<string, string> {
  const m = /^---\r?\n([\s\S]*?)\r?\n---/.exec(src);
  const out: Record<string, string> = {};
  if (!m) return out;
  for (const line of m[1].split('\n')) {
    const i = line.indexOf(':');
    if (i > 0) out[line.slice(0, i).trim()] = unquote(line.slice(i + 1).trim());
  }
  return out;
}

/**
 * Skills live in <root>/<name>/SKILL.md, global (~/.tango/skills) and project (.tango/skills).
 * Projects may also use the shared location .agents/skills.
 * Only name + description go into the system prompt; the model reads the file when it matches.
 */
export async function discoverSkills(cwd: string): Promise<SkillInfo[]> {
  // .agents/skills is the project location several agent CLIs share; the first skill of a name
  // wins. ~/.agents/skills is not read: skill installers fill it with dozens of entries, and every
  // one would ride along in each request's system prompt.
  const roots = [skillsDir(), join(cwd, '.tango', 'skills'), join(cwd, '.agents', 'skills')];
  const skills: SkillInfo[] = [];
  const seen = new Set<string>();
  for (const root of roots) {
    if (!existsSync(root)) continue;
    for (const dir of (await readdir(root).catch(() => [] as string[])).sort()) {
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

/**
 * The instructions file of one directory: AGENTS.override.md replaces that directory's AGENTS.md
 * (a local, usually git-ignored variant), other directories are unaffected.
 */
function agentsFileIn(dir: string): string | undefined {
  for (const name of ['AGENTS.override.md', 'AGENTS.md']) {
    const f = join(dir, name);
    if (existsSync(f)) return f;
  }
  return undefined;
}

/** Paths of the AGENTS.md files loadAgentsFiles reads, in the same order (global first, nearest last). */
export function findAgentsFiles(cwd: string): string[] {
  const local: string[] = [];
  let dir = cwd;
  while (true) {
    const f = agentsFileIn(dir);
    if (f) local.unshift(f);
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }
  const global = agentsFileIn(tangoDir());
  return global && !local.includes(global) ? [global, ...local] : local;
}

/** AGENTS.md from cwd upward to the filesystem root, plus global ~/.tango/AGENTS.md. Global first, nearest last. */
export async function loadAgentsFiles(cwd: string): Promise<string[]> {
  const out: string[] = [];
  for (const f of findAgentsFiles(cwd)) {
    const body = (await readFile(f, 'utf8').catch(() => '')).trim();
    if (body) out.push(body);
  }
  return out;
}

export interface ContextInfo {
  systemPrompt: string;
  skills: SkillInfo[];
}

/** How the user's terminal renders decision asks — the TUI parses these markers. */
const ASK_PROTOCOL = `# Asking the user

The user may not be watching the screen when your turn ends. Never end a turn on a bare
question. Two markers, both understood by the terminal:

When you need the user's decision before proceeding, end your reply with a choice block:

?choice
| <optional preview line: the command or path the ask is about>
| <more preview lines as needed>
1. <concise option — put your recommendation first>
   <optional indented line: a short muted annotation explaining what this choice does>
2. <option>
3. <option>

Use 2-4 options. "| " lines render as a bar-indented preview block; a line indented by
three spaces annotates the option above it. The terminal renders all of this as a
clickable panel; the picked option comes back to you as the user's next message, so
treat it as their answer and continue. The user may also press tab to write their own
answer instead of picking — never design options that assume they can't. The
terminal also offers a built-in "explain these options" row; when the user picks
it you receive a prompt asking for a per-option walkthrough — explain each
briefly (what it does, consequences, when to choose it) and end with the same
?choice block again.

For minor suggestions raised while other work continues (nothing blocking), emit one line:

?suggest <one-line suggestion>

These collect into the user's review list and may be acted on much later. Never use ?choice
for rhetorical questions, and never propose a ?choice whose options all mean "keep waiting".`;

/** Assemble the system prompt: base + AGENTS.md + skills index + environment. */
export async function buildContext(cwd: string): Promise<ContextInfo> {
  const parts = [BASE_PROMPT, ASK_PROTOCOL];

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
