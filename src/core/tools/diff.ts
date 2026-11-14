import { spawn } from 'node:child_process';
import type { Tool } from '../types.js';
import { error, text, truncate } from './registry.js';

function git(args: string[], cwd: string, signal: AbortSignal): Promise<{ out: string; code: number | null }> {
  return new Promise((done) => {
    const child = spawn('git', args, { cwd, stdio: ['ignore', 'pipe', 'pipe'] });
    let out = '';
    child.stdout.on('data', (b: Buffer) => (out += b.toString()));
    child.stderr.on('data', (b: Buffer) => (out += b.toString()));
    const kill = () => child.kill('SIGKILL');
    const timer = setTimeout(kill, 30_000);
    signal.addEventListener('abort', kill, { once: true });
    child.on('error', (e) => { clearTimeout(timer); done({ out: e.message, code: -1 }); });
    child.on('close', (code) => { clearTimeout(timer); done({ out, code }); });
  });
}

interface Input {
  paths?: string[];
  staged?: boolean;
}

export const diffTool: Tool<Input> = {
  name: 'diff',
  description:
    'Show what changed in the git repository: modified/untracked files with insert/delete counts. ' +
    'With paths, shows the full diff of those files. Read-only; use bash for add/commit.',
  input: {
    type: 'object',
    properties: {
      paths: { type: 'array', items: { type: 'string' }, description: 'Files or directories to diff (default: summary of everything)' },
      staged: { type: 'boolean', description: 'Show staged (--cached) changes instead of the working tree' }
    }
  },
  async execute({ paths, staged }, ctx) {
    try {
      const root = await git(['rev-parse', '--show-toplevel'], ctx.cwd, ctx.signal);
      if (root.code !== 0) return error('not a git repository');
      const repo = root.out.trim();
      if (paths?.length) {
        const r = await git(['diff', ...(staged ? ['--cached'] : []), '--', ...paths], repo, ctx.signal);
        return text(truncate(r.out || '(no diff for those paths)'));
      }
      const [st, unstaged] = await Promise.all([
        git(['status', '--porcelain'], repo, ctx.signal),
        git(['diff', ...(staged ? ['--cached'] : []), '--stat'], repo, ctx.signal)
      ]);
      const body = [unstaged.out.trim(), st.out.trim() && `status:\n${st.out.trim()}`].filter(Boolean).join('\n\n');
      return text(truncate(body || 'clean — nothing changed'));
    } catch (e: any) {
      return error(`diff failed: ${e.message}`);
    }
  }
};
