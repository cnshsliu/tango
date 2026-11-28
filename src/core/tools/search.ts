import { spawn, spawnSync } from 'node:child_process';
import { readdir, stat } from 'node:fs/promises';
import { join, resolve } from 'node:path';
import type { Tool, ToolContext } from '../types.js';
import { IS_WINDOWS } from '../shell.js';
import { error, text, truncate } from './registry.js';

const hasRg = spawnSync('rg', ['--version'], { stdio: 'ignore' }).status === 0;
/** Windows has no grep, and its `find` searches strings, not files: there ripgrep is required. */
const NEED_RG = IS_WINDOWS && !hasRg ? 'grep and find need ripgrep on Windows: install it with `winget install BurntSushi.ripgrep.MSVC`' : '';

/** Run a command without a shell (no quoting issues); resolve with stdout, stderr and exit code. */
function run(cmd: string, args: string[], ctx: ToolContext): Promise<{ out: string; err: string; code: number | null }> {
  return new Promise((done) => {
    const child = spawn(cmd, args, { cwd: ctx.cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    let err = '';
    child.stdout.on('data', (b: Buffer) => (out += b.toString()));
    child.stderr.on('data', (b: Buffer) => (err += b.toString()));
    const onAbort = () => child.kill('SIGKILL');
    ctx.signal.addEventListener('abort', onAbort, { once: true });
    child.on('error', (e) => done({ out: '', err: String(e.message), code: -1 }));
    child.on('close', (code) => {
      ctx.signal.removeEventListener('abort', onAbort);
      done({ out, err, code });
    });
  });
}

/**
 * rg/grep/find exit 2 when any path was unreadable (macOS privacy-protected folders, root-owned
 * dirs) even though the rest was searched. Results win: keep them and note what was skipped in
 * one line. Only a run with no results and a real error is a failure.
 */
export function searchOutcome(tool: string, r: { out: string; err: string; code: number | null }, empty: string, body: (out: string) => string) {
  const denied = r.err.split('\n').filter((l) => /Operation not permitted|Permission denied/.test(l));
  const other = r.err.split('\n').filter((l) => l.trim() && !denied.includes(l));
  const note = denied.length ? `\n[skipped ${denied.length} unreadable path${denied.length > 1 ? 's' : ''} (no permission)]` : '';
  if (r.out.trim()) return text(body(r.out) + note);
  if (r.code === 0 || r.code === 1 || (r.code === 2 && !other.length)) return text(empty + note);
  return error(`${tool} failed: ${(other.join('\n') || r.err).trim().slice(0, 2000)}`);
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
    if (NEED_RG) return error(NEED_RG);
    const r = await run(hasRg ? 'rg' : 'grep', args, ctx);
    return searchOutcome('grep', r, 'no matches', (out) => truncate(limitLines(out, limit)));
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
    if (NEED_RG) return error(NEED_RG);
    const r = hasRg
      ? await run('rg', ['--files', '--color', 'never', '-g', pattern], { ...ctx, cwd: resolve(ctx.cwd, path) })
      : await run('find', [path, '-path', '*/node_modules', '-prune', '-o', '-path', '*/.git', '-prune', '-o', '-type', 'f', '-name', pattern.split('/').pop() ?? pattern, '-print'], ctx);
    return searchOutcome('find', r, 'no files found', (out) =>
      truncate(limitLines(out.trimEnd().split('\n').sort().join('\n'), limit))
    );
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
