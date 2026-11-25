import type { Tool } from '../types.js';
import { error, text } from './registry.js';
import { addTask, lastRunAt, loadTasks, parseInterval, removeTask } from '../scheduler.js';

/** The model's view of ~/.tango/tasks.json, the same list `tango task` manages. */
export const scheduleListTool: Tool<Record<string, never>> = {
  name: 'schedule_list',
  description: 'List scheduled tasks (tango task): name, interval, mode, last run, prompt.',
  input: { type: 'object', properties: {} },
  async execute() {
    try {
      const tasks = await loadTasks();
      if (!tasks.length) return text('no scheduled tasks');
      const rows = await Promise.all(
        tasks.map(async (t) => {
          const last = await lastRunAt(t.name);
          const when = last ? `${last.ts.slice(0, 16)} ${last.ok ? 'ok' : 'FAILED'}` : 'never';
          return `${t.name} · every ${t.everyMinutes}m · ${t.mode ?? 'readonly'} · last: ${when} · cwd ${t.cwd ?? '?'}\n  ${t.prompt}`;
        })
      );
      return text(rows.join('\n') + '\n\nThey run while `tango task run --loop` is running (or the launchd agent).');
    } catch (e: any) {
      return error(e.message);
    }
  }
};

export const scheduleCreateTool: Tool<{ name: string; every: string; prompt: string; mode?: string }> = {
  name: 'schedule_create',
  writeEffect: true,
  description:
    'Schedule a recurring headless prompt (tango task add). It runs in the current directory every `every` ' +
    '(e.g. 30m, 2h, 1d) and posts the result to Feishu when feishuWebhook is set. mode readonly (default) or ' +
    'auto (may edit and run commands unattended: only when the user asked for it).',
  input: {
    type: 'object',
    properties: {
      name: { type: 'string', description: 'Short unique name' },
      every: { type: 'string', description: 'Interval: 30m, 2h, 1d or minutes' },
      prompt: { type: 'string', description: 'What to do on each run; self-contained, it has no chat history' },
      mode: { type: 'string', enum: ['readonly', 'auto'] }
    },
    required: ['name', 'every', 'prompt']
  },
  async execute({ name, every, prompt, mode = 'readonly' }, ctx) {
    try {
      if (!name.trim() || !prompt.trim()) return error('name and prompt must not be empty');
      if (mode !== 'readonly' && mode !== 'auto') return error('mode must be readonly or auto (headless tasks never ask)');
      const everyMinutes = parseInterval(every);
      await addTask({ name: name.trim(), prompt: prompt.trim(), everyMinutes, cwd: ctx.cwd, mode });
      return text(`scheduled "${name.trim()}" every ${everyMinutes}m (${mode}) in ${ctx.cwd}. It runs while \`tango task run --loop\` is running.`);
    } catch (e: any) {
      return error(e.message);
    }
  }
};

export const scheduleRemoveTool: Tool<{ name: string }> = {
  name: 'schedule_remove',
  writeEffect: true,
  description: 'Remove a scheduled task by name (tango task rm).',
  input: { type: 'object', properties: { name: { type: 'string' } }, required: ['name'] },
  async execute({ name }) {
    try {
      return text(`removed "${(await removeTask(name)).name}"`);
    } catch (e: any) {
      return error(e.message);
    }
  }
};
