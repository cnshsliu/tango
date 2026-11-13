import { readFile, writeFile, mkdir } from 'node:fs/promises';
import { dirname, resolve } from 'node:path';
import type { Tool } from '../types.js';
import { error, text, truncate } from './registry.js';

const IMAGE_MIME: Record<string, string> = {
  '.png': 'image/png',
  '.jpg': 'image/jpeg',
  '.jpeg': 'image/jpeg',
  '.gif': 'image/gif',
  '.webp': 'image/webp'
};

export const readTool: Tool<{ path: string; offset?: number; limit?: number }> = {
  name: 'read',
  description:
    'Read a file. Text files return numbered lines (use offset/limit for large files). Images are returned as images.',
  input: {
    type: 'object',
    properties: {
      path: { type: 'string', description: 'File path, absolute or relative to cwd' },
      offset: { type: 'number', description: '1-based line to start from' },
      limit: { type: 'number', description: 'Max lines to return (default 2000)' }
    },
    required: ['path']
  },
  async execute({ path, offset = 1, limit = 2000 }, ctx) {
    const p = resolve(ctx.cwd, path);
    const ext = p.slice(p.lastIndexOf('.')).toLowerCase();
    try {
      if (IMAGE_MIME[ext]) {
        const data = (await readFile(p)).toString('base64');
        return { content: [{ type: 'image', mimeType: IMAGE_MIME[ext], data }] };
      }
      const lines = (await readFile(p, 'utf8')).split('\n');
      const slice = lines.slice(offset - 1, offset - 1 + limit);
      const body = slice.map((l, i) => `${String(offset + i).padStart(5)}  ${l}`).join('\n');
      const more = lines.length > offset - 1 + limit ? `\n…[${lines.length - (offset - 1 + limit)} more lines]` : '';
      return text(truncate(body + more));
    } catch (e: any) {
      return error(`read failed: ${e.message}`);
    }
  }
};

export const writeTool: Tool<{ path: string; content: string }> = {
  name: 'write',
  description: 'Create or overwrite a file with the given content. Creates parent directories.',
  input: {
    type: 'object',
    properties: {
      path: { type: 'string' },
      content: { type: 'string' }
    },
    required: ['path', 'content']
  },
  async execute({ path, content }, ctx) {
    const p = resolve(ctx.cwd, path);
    try {
      await mkdir(dirname(p), { recursive: true });
      await writeFile(p, content, 'utf8');
      return text(`wrote ${content.length} chars to ${p}`);
    } catch (e: any) {
      return error(`write failed: ${e.message}`);
    }
  }
};

export const editTool: Tool<{ path: string; old: string; new: string }> = {
  name: 'edit',
  description:
    'Replace exactly one occurrence of `old` with `new` in a file. `old` must match exactly once; include enough context to be unique.',
  input: {
    type: 'object',
    properties: {
      path: { type: 'string' },
      old: { type: 'string', description: 'Exact text to find (must be unique in file)' },
      new: { type: 'string', description: 'Replacement text' }
    },
    required: ['path', 'old', 'new']
  },
  async execute({ path, old, new: neu }, ctx) {
    const p = resolve(ctx.cwd, path);
    try {
      const src = await readFile(p, 'utf8');
      const first = src.indexOf(old);
      if (first < 0) return error('edit failed: `old` text not found');
      if (src.indexOf(old, first + 1) >= 0) return error('edit failed: `old` text matches more than once');
      await writeFile(p, src.slice(0, first) + neu + src.slice(first + old.length), 'utf8');
      return text(`edited ${p}`);
    } catch (e: any) {
      return error(`edit failed: ${e.message}`);
    }
  }
};
