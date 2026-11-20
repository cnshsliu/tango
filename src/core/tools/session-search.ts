import { readFile, readdir } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { sessionsDir } from '../config.js';
import type { SessionEntry, Tool } from '../types.js';
import { error, text, truncate } from './registry.js';

interface Hit {
  id: string;
  createdAt: string;
  cwd: string;
  topic: string;
  snippets: string[];
  total: number;
}

/** Searchable prose of one entry: user/assistant text and summaries, not thinking, signatures or raw tool output. */
function entryText(e: SessionEntry): { who: string; ts: string; text: string } | undefined {
  if (e.kind === 'message') {
    const parts = e.message.content.filter((b: any) => b.type === 'text').map((b: any) => b.text as string);
    const t = parts.join('\n').trim();
    return t ? { who: e.message.role, ts: e.ts, text: t } : undefined;
  }
  if (e.kind === 'compaction') return { who: 'summary', ts: e.ts, text: e.summary };
  if (e.kind === 'handoff') return { who: 'handoff', ts: e.ts, text: e.summary };
  return undefined;
}

function snippet(s: string, at: number, len: number): string {
  const from = Math.max(0, at - 120);
  const to = Math.min(s.length, at + len + 240);
  return (from > 0 ? '…' : '') + s.slice(from, to).replace(/\s+/g, ' ') + (to < s.length ? '…' : '');
}

function compile(query: string): RegExp {
  try {
    return new RegExp(query, 'gi');
  } catch {
    return new RegExp(query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi');
  }
}

export const sessionSearchTool: Tool<{ query: string; limit?: number; cwd?: string }> = {
  name: 'session_search',
  description:
    "Search the user's past tango conversations (~/.tango/sessions) for a regex or keywords (case-insensitive; use a|b for alternatives, include both Chinese and English terms). " +
    'Use whenever the user refers to earlier conversations ("did we talk about", "last time", "之前/上次/我们聊过"). ' +
    'Returns matching sessions newest first with id, date, topic and snippets; read ~/.tango/sessions/<id>.jsonl for full detail. The current session is excluded.',
  input: {
    type: 'object',
    properties: {
      query: { type: 'string', description: 'Regex or keywords, e.g. "智谱|zhipu|coding plan"' },
      limit: { type: 'number', description: 'Max sessions to return (default 10)' },
      cwd: { type: 'string', description: 'Only sessions started in this directory' }
    },
    required: ['query']
  },
  async execute({ query, limit = 10, cwd }, ctx) {
    if (!query.trim()) return error('empty query');
    const dir = sessionsDir();
    if (!existsSync(dir)) return text('no sessions yet');
    const re = compile(query);
    const hits: Hit[] = [];
    for (const f of await readdir(dir)) {
      if (ctx.signal.aborted) return error('aborted');
      if (!f.endsWith('.jsonl')) continue;
      const id = f.slice(0, -'.jsonl'.length);
      if (id === ctx.sessionId) continue;
      const hit: Hit = { id, createdAt: '', cwd: '', topic: '', snippets: [], total: 0 };
      for (const line of (await readFile(join(dir, f), 'utf8')).split('\n')) {
        if (!line.trim()) continue;
        let e: SessionEntry;
        try {
          e = JSON.parse(line);
        } catch {
          continue;
        }
        if (e.kind === 'meta') {
          hit.createdAt = e.createdAt;
          hit.cwd = e.cwd;
          continue;
        }
        const t = entryText(e);
        if (!t) continue;
        if (!hit.topic && t.who === 'user') hit.topic = t.text.slice(0, 80).replace(/\s+/g, ' ');
        re.lastIndex = 0;
        const m = re.exec(t.text);
        if (!m) continue;
        hit.total++;
        if (hit.snippets.length < 3) hit.snippets.push(`  [${t.ts.slice(0, 16)} ${t.who}] ${snippet(t.text, m.index, m[0].length)}`);
      }
      if (hit.total && (!cwd || hit.cwd === cwd)) hits.push(hit);
    }
    if (!hits.length) return text(`no past session mentions /${query}/ (searched ${dir})`);
    hits.sort((a, b) => b.createdAt.localeCompare(a.createdAt));
    const out = hits.slice(0, limit).map(
      (h) => `${h.id}  ${h.createdAt.slice(0, 16)}  cwd=${h.cwd}  ${h.total} matching entries\n  topic: ${h.topic}\n${h.snippets.join('\n')}`
    );
    const more = hits.length > limit ? `\n…${hits.length - limit} more sessions match` : '';
    return text(truncate(out.join('\n\n') + more));
  }
};
