import { spawn, spawnSync } from 'node:child_process';
import { readdir, stat } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import type { Tool, ToolContext } from '../types.js';
import { error, text, truncate } from './registry.js';

const hasRg = spawnSync('rg', ['--version'], { stdio: 'ignore' }).status === 0;

/** Run a command without a shell (no quoting issues); resolve with stdout and exit code. */
function run(cmd: string, args: string[], ctx: ToolContext): Promise<{ out: string; code: number | null }> {
  return new Promise((done) => {
    const child = spawn(cmd, args, { cwd: ctx.cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    child.stdout.on('data', (b: Buffer) => (out += b.toString()));
    child.stderr.on('data', (b: Buffer) => (out += b.toString()));
    const onAbort = () => child.kill('SIGKILL');
    ctx.signal.addEventListener('abort', onAbort, { once: true });
    child.on('error', (e) => done({ out: String(e.message), code: -1 }));
    child.on('close', (code) => {
      ctx.signal.removeEventListener('abort', onAbort);
      done({ out, code });
    });
  });
}

const limitLines = (s: string, n: number) => {
  const lines = s.trimEnd().split('\n');
  return lines.length > n ? lines.slice(0, n).join('\n') + `\n…[${lines.length - n} more lines]` : lines.join('\n');
};

export const grepTool: Tool<{ pattern: string; path?: string; glob?: string; ignoreCase?: boolean; limit?: number }> = {
  name: 'grep',
  description:
    'Search file contents with a regular expression. Returns path:line:text matches. Respects .gitignore. Prefer this over bash grep.',
  input: {
    type: 'object',
    properties: {
      pattern: { type: 'string', description: 'Regular expression' },
      path: { type: 'string', description: 'File or directory to search (default cwd)' },
      glob: { type: 'string', description: 'Only search files matching this glob, e.g. "*.ts"' },
      ignoreCase: { type: 'boolean' },
      limit: { type: 'number', description: 'Max matching lines (default 200)' }
    },
    required: ['pattern']
  },
  async execute({ pattern, path = '.', glob, ignoreCase, limit = 200 }, ctx) {
    const args = hasRg
      ? ['-n', '--no-heading', '--color', 'never', '-M', '300']
      : ['-rnE', '--exclude-dir=node_modules', '--exclude-dir=.git'];
    if (ignoreCase) args.push('-i');
    if (glob) args.push(hasRg ? '-g' : '--include', glob);
    args.push('-e', pattern, path);
    const { out, code } = await run(hasRg ? 'rg' : 'grep', args, ctx);
    if (code === 1) return text('no matches');
    if (code !== 0) return error(`grep failed: ${out.trim()}`);
    return text(truncate(limitLines(out, limit)));
  }
};

export const findTool: Tool<{ pattern: string; path?: string; limit?: number }> = {
  name: 'find',
  description:
    'Find files by glob pattern, e.g. "**/*.ts" or "src/**/test*". Respects .gitignore. Returns paths relative to the search root.',
  input: {
    type: 'object',
    properties: {
      pattern: { type: 'string', description: 'Glob pattern' },
      path: { type: 'string', description: 'Directory to search (default cwd)' },
      limit: { type: 'number', description: 'Max results (default 500)' }
    },
    required: ['pattern']
  },
  async execute({ pattern, path = '.', limit = 500 }, ctx) {
    const { out, code } = hasRg
      ? await run('rg', ['--files', '--color', 'never', '-g', pattern], { ...ctx, cwd: resolve(ctx.cwd, path) })
      : await run('find', [path, '-path', '*/node_modules', '-prune', '-o', '-path', '*/.git', '-prune', '-o', '-type', 'f', '-name', pattern.split('/').pop() ?? pattern, '-print'], ctx);
    if (code === 1 || !out.trim()) return text('no files found');
    if (code !== 0) return error(`find failed: ${out.trim()}`);
    return text(truncate(limitLines(out.split('\n').sort().join('\n'), limit)));
  }
};

export const lsTool: Tool<{ path?: string }> = {
  name: 'ls',
  description: 'List a directory: one entry per line, directories end with "/", files show their size.',
  input: {
    type: 'object',
    properties: { path: { type: 'string', description: 'Directory (default cwd)' } }
  },
  async execute({ path = '.' }, ctx) {
    const dir = resolve(ctx.cwd, path);
    try {
      const names = (await readdir(dir)).sort();
      const lines: string[] = [];
      for (const n of names.slice(0, 1000)) {
        try {
          const s = await stat(join(dir, n));
          lines.push(s.isDirectory() ? `${n}/` : `${n}  ${s.size}B`);
        } catch {
          lines.push(`${n}  (unreadable)`);
        }
      }
      if (names.length > 1000) lines.push(`…[${names.length - 1000} more entries]`);
      return text(lines.length ? lines.join('\n') : '(empty directory)');
    } catch (e: any) {
      return error(`ls failed: ${e.message}`);
    }
  }
};
