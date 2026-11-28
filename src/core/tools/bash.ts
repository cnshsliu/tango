import { spawn } from 'node:child_process';
import { existsSync } from 'node:fs';
import type { Tool } from '../types.js';
import { describe, startProcess, startup, tail } from '../processes.js';
import { DETACHED, IS_WINDOWS, killProcessTree, shellInvocation } from '../shell.js';

/** What the model writes commands for: PowerShell on Windows, a login bash elsewhere. */
const SHELL_NAME = IS_WINDOWS ? 'PowerShell' : 'bash, a login shell';
import { error, text, truncate } from './registry.js';

export const bashTool: Tool<{ command: string; timeout?: number; background?: boolean }> = {
  name: 'bash',
  writeEffect: true,
  description:
    `Run a shell command in cwd and return stdout+stderr (${SHELL_NAME}); write the command for that shell. Use for ls, grep, find, git, tests, etc. Long output is truncated. ` +
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
      // a short grace period catches commands that fail right away (see startup.graceMs)
      await Promise.race([p.exited, new Promise((r) => setTimeout(r, startup.graceMs))]);
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
      const shell = shellInvocation(command); // bash -lc, PowerShell on Windows
      const child = spawn(shell.file, shell.args, {
        cwd: ctx.cwd,
        env: process.env,
        stdio: ['ignore', 'pipe', 'pipe'],
        detached: DETACHED,
        windowsHide: true
      });
      const killGroup = () => killProcessTree(child.pid);
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

      // 'error' with no listener kills the whole serve process (Node's default).
      // A missing cwd fails this way: spawn() itself does not throw.
      let started = false;
      let settled = false;
      const finish = (text: string, isError: boolean) => {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        ctx.signal.removeEventListener('abort', onAbort);
        resolvePromise({ content: [{ type: 'text', text }], isError });
      };
      child.on('spawn', () => { started = true; });
      child.on('error', (e) => {
        if (started) return; // the close handler reports the exit
        const why = existsSync(ctx.cwd)
          ? `cannot run command: ${e.message}`
          : `cannot run command: cwd does not exist: ${ctx.cwd}`;
        finish(why, true);
      });
      child.on('close', (code) => {
        if (!started) return;
        const body = truncate(out.trimEnd());
        // killed by the user's interrupt: say so instead of the meaningless `[exit null]`
        const tailNote = code === 0 ? '' : ctx.signal.aborted ? '\n[interrupted]' : `\n[exit ${code}]`;
        finish(body + tailNote, code !== 0);
      });
    });
  }
};
