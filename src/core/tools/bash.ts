import { spawn } from 'node:child_process';
import type { Tool } from '../types.js';
import { text, truncate } from './registry.js';

export const bashTool: Tool<{ command: string; timeout?: number }> = {
  name: 'bash',
  writeEffect: true,
  description:
    'Run a shell command in cwd and return stdout+stderr. Use for ls, grep, find, git, tests, etc. Long output is truncated.',
  input: {
    type: 'object',
    properties: {
      command: { type: 'string' },
      timeout: { type: 'number', description: 'Seconds (default 120)' }
    },
    required: ['command']
  },
  execute({ command, timeout = 120 }, ctx) {
    return new Promise((resolvePromise) => {
      // detached → own process group: kills reach curl/sleep grandkids too.
      // Killing only the bash wrapper orphans them and they keep the output pipe
      // open, so the tool only "finishes" when the grandchild exits naturally —
      // the interrupt/timeout appeared to do nothing for whole downloads.
      const child = spawn('/bin/bash', ['-lc', command], {
        cwd: ctx.cwd,
        env: process.env,
        stdio: ['ignore', 'pipe', 'pipe'],
        detached: true
      });
      const killGroup = () => {
        try {
          process.kill(-child.pid!, 'SIGKILL');
        } catch {
          try { child.kill('SIGKILL'); } catch {}
        }
      };
      let out = '';
      const push = (b: Buffer) => {
        out += b.toString();
      };
      child.stdout.on('data', push);
      child.stderr.on('data', push);

      const timer = setTimeout(() => {
        killGroup();
        out += `\n[killed after ${timeout}s]`;
      }, timeout * 1000);
      const onAbort = killGroup;
      ctx.signal.addEventListener('abort', onAbort, { once: true });

      child.on('close', (code) => {
        clearTimeout(timer);
        ctx.signal.removeEventListener('abort', onAbort);
        const body = truncate(out.trimEnd());
        resolvePromise({
          content: [{ type: 'text', text: body + (code === 0 ? '' : `\n[exit ${code}]`) }],
          isError: code !== 0
        });
      });
    });
  }
};
