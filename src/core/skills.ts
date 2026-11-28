import { existsSync } from 'node:fs';
import { mkdir, readFile, readdir, rename, writeFile } from 'node:fs/promises';
import { renameRetry } from './fs-retry.js';
import { join, isAbsolute } from 'node:path';
import { tangoDir } from './config.js';
import type { Message } from './types.js';

/** Global skills root; follows TANGO_HOME (project skills stay <cwd>/.tango/skills). */
export const skillsDir = () => join(tangoDir(), 'skills');

/** Directory for drafts awaiting a title/description; discoverSkills() never reads it. */
export const draftsDir = () => join(tangoDir(), 'skill-drafts');

/** kebab-case slug; keeps CJK, ASCII letters/digits; other runs become single dashes. */
export function slugify(name: string): string {
  return (
    name
      .trim()
      .toLowerCase()
      .replace(/[\s_]+/g, '-')
      .replace(/[^\p{Letter}\p{Number}-]+/gu, '-')
      .replace(/-{2,}/g, '-')
      .replace(/^-+|-+$/g, '') || 'skill'
  );
}

const SUMMARY_CHARS = 1_200;

function textOf(m: Message): string {
  return m.content
    .filter((b) => b.type === 'text')
    .map((b) => (b as { text: string }).text)
    .join(' ')
    .replace(/\s+/g, ' ')
    .trim();
}

function summarizeCall(name: string, input: unknown): string {
  const i = (input ?? {}) as Record<string, unknown>;
  const key = ['command', 'path', 'url', 'query', 'pattern'].find((k) => typeof i[k] === 'string');
  const detail = String(key ? i[key] : JSON.stringify(i) ?? '');
  return (detail.length > 100 ? detail.slice(0, 100) + '…' : detail).replace(/\s+/g, ' ').trim();
}

/** Condense messages into the draft's transcript: every user turn, plus tool calls per assistant turn. */
export function extractSteps(messages: Message[]): string[] {
  const steps: string[] = [];
  let n = 0;
  for (const m of messages) {
    if (m.role === 'user') {
      const t = textOf(m);
      // skip harness-injected messages (compaction summaries, tool results only)
      if (t && !t.startsWith('<summary')) steps.push(`用户: ${t.slice(0, 400)}`);
    } else if (m.role === 'assistant') {
      const calls = m.content.filter((b) => b.type === 'tool_call') as { name: string; input: unknown }[];
      if (!calls.length) continue;
      n++;
      const parts = calls.map((c) => `${c.name}(${summarizeCall(c.name, c.input)})`).join(' → ');
      steps.push(`第 ${n} 轮工具: ${parts.slice(0, 600)}`);
    }
  }
  return steps;
}

function renderDraft(description: string, steps: string[], cwd: string): string {
  const summary = steps.join('\n').slice(0, SUMMARY_CHARS);
  return `---
name: ${description}
description: 由 tango 会话自动记录的工作流程草稿，补全触发场景和步骤后生效。
---

# ${description}

用户在 ${cwd} 的会话中走过以下流程（于 ${new Date().toISOString().slice(0, 10)} 保存）。
补全下面的「步骤」，写清触发场景，然后运行 /save-as-skill 定稿。

## 触发场景

（何时该用这个技能？写清楚关键词，模型靠它匹配。）

## 步骤

${steps.length ? steps.map((s) => `- ${s}`).join('\n') : '- （本次会话没有可提取的步骤）'}

## 注意

（容易出错的地方、依赖的命令、环境变量。）
`;
}

export interface SavedDraft {
  file: string;
  slug: string;
}

/** Write a draft under <tangoDir>/skill-drafts/. Never touches skills/. */
export async function saveDraft(args: { title: string; description?: string; messages: Message[]; cwd: string }): Promise<SavedDraft> {
  const slug = slugify(args.title);
  await mkdir(draftsDir(), { recursive: true });
  const file = join(draftsDir(), `${slug}.draft.md`);
  if (existsSync(file)) throw new Error(`draft already exists: ${file}\nedit or rename it first`);
  await writeFile(file, renderDraft(args.description?.trim() || args.title, extractSteps(args.messages), args.cwd), 'utf8');
  return { file, slug };
}

export interface PromotedSkill {
  dir: string;
  file: string;
}

/**
 * Turn a finished draft into <root>/<slug>/SKILL.md. The draft must carry a
 * `description:` line — the only thing the model sees before it reads the skill.
 */
export async function promoteDraft(draftFile: string, root: string): Promise<PromotedSkill> {
  // a path, not a bare name: `/` or `\` in it, or absolute (C:\…)
  const src = draftFile.includes('/') || draftFile.includes('\\') || isAbsolute(draftFile) ? draftFile : join(draftsDir(), draftFile);
  if (!existsSync(src)) throw new Error(`draft not found: ${draftFile} (see ${draftsDir()})`);
  const body = await readFile(src, 'utf8');
  const fm = /^---\r?\n([\s\S]*?)\r?\n---/.exec(body);
  const desc = fm && /^description:\s*(.+)$/m.exec(fm[1])?.[1]?.trim();
  if (!desc || desc.startsWith('由 tango 会话'))
    throw new Error(`${src}: fill in a real "description:" first — it is how the model knows when to use this skill`);
  const slug = slugify(fm![1].match(/^name:\s*(.+)$/m)?.[1]?.trim() || src.replace(/^.*\//, '').replace(/\.draft\.md$/, ''));
  const dir = join(root, slug);
  if (existsSync(dir)) throw new Error(`skill already exists: ${dir}\nremove it first to overwrite`);
  await mkdir(dir, { recursive: true });
  const file = join(dir, 'SKILL.md');
  await writeFile(file, body, 'utf8');
  await renameRetry(src, src + '.done');
  return { dir, file };
}

export async function listDrafts(): Promise<string[]> {
  if (!existsSync(draftsDir())) return [];
  return (await readdir(draftsDir()))
    .filter((f) => f.endsWith('.draft.md'))
    .sort()
    .map((f) => join(draftsDir(), f));
}
