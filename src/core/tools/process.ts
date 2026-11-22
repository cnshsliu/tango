import type { Tool } from '../types.js';
import { describe, getProcess, listProcesses, stopProcess, tail } from '../processes.js';
import { error, text, truncate } from './registry.js';

const missing = (id: string) => error(`no background process "${id}"; process_list shows the ids`);

export const processLogsTool: Tool<{ id: string; lines?: number }> = {
  name: 'process_logs',
  description: 'Show the last N lines of output (stdout+stderr) of a background process started by bash with background: true.',
  input: {
    type: 'object',
    properties: {
      id: { type: 'string', description: 'Process id, e.g. p1' },
      lines: { type: 'number', description: 'Lines from the end (default 50)' }
    },
    required: ['id']
  },
  async execute({ id, lines = 50 }) {
    const p = getProcess(id);
    if (!p) return missing(id);
    const body = tail(p, lines);
    return text(truncate(`${describe(p)}\n${body || '(no output yet)'}`));
  }
};

export const processStopTool: Tool<{ id: string }> = {
  name: 'process_stop',
  writeEffect: true,
  description: 'Stop a background process: SIGTERM to its process group, SIGKILL after 3 seconds.',
  input: {
    type: 'object',
    properties: { id: { type: 'string', description: 'Process id, e.g. p1' } },
    required: ['id']
  },
  async execute({ id }) {
    const p = getProcess(id);
    if (!p) return missing(id);
    if (p.done) return text(`already finished: ${describe(p)}`);
    await stopProcess(p);
    return text(`stopped: ${describe(p)}\n${tail(p, 10)}`.trimEnd());
  }
};

export const processListTool: Tool<Record<string, never>> = {
  name: 'process_list',
  description: 'List background processes started in this session, running and finished.',
  input: { type: 'object', properties: {} },
  async execute() {
    const all = listProcesses();
    return text(all.length ? all.map(describe).join('\n') : 'no background processes');
  }
};
