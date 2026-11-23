import { spawn } from 'node:child_process';
import type { Tool } from '../types.js';
import { describe, startProcess, tail } from '../processes.js';
import { error, text, truncate } from './registry.js';

export const bashTool: Tool<{ command: string; timeout?: number; background?: boolean }> = {
  name: 'bash',
  writeEffect: true,
  description:
    'Run a shell command in cwd and return stdout+stderr. Use for ls, grep, find, git, tests, etc. Long output is truncated. ' +
    'Set background: true for dev servers, watchers and other long-running commands: it returns a process id at once ' +
    '(read output with process_logs, end it with process_stop, see all with process_list; at most 8 run at a time).',
  input: {
    type: 'object',
    properties: {
      command: { type: 'string' },
      timeout: { type: 'number', description: 'Seconds (default 120; ignored when background)' },
      background: { type: 'boolean', description: 'Run detached and return a process id immediately' }
    },
    required: ['command']
  },
  async execute({ command, timeout = 120, background }, ctx) {
    if (background) {
      let p;
      try {
        p = startProcess(command, ctx.cwd);
      } catch (e) {
        return error((e as Error).message);
      }
      // a short grace period catches commands that fail right away (typo, port in use);
      // a login shell alone can take ~0.8 s to start on a busy profile, so 1 s was too tight
      await Promise.race([p.exited, new Promise((r) => setTimeout(r, 2000))]);
      const head = tail(p, 20);
      return {
        content: [{ type: 'text', text: `started ${describe(p)}` + (head ? `\n${head}` : '') }],
        isError: p.done && p.exitCode !== 0
      };
    }
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
