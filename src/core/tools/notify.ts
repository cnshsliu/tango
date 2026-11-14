import type { Tool } from '../types.js';
import { error, text } from './registry.js';
import { feishuWebhook, sendFeishu } from '../scheduler.js';

export const notifyTool: Tool<{ title?: string; text: string }> = {
  name: 'notify',
  description:
    'Send a message to the user\'s Feishu/Lark bot webhook (config feishuWebhook). Use for results the user asked to be ' +
    'pushed to chat: finished scheduled work, findings, reminders. Not for files — say the path instead.',
  input: {
    type: 'object',
    properties: {
      title: { type: 'string', description: 'One-line card title' },
      text: { type: 'string', description: 'Markdown message body' }
    },
    required: ['text']
  },
  async execute({ title, text: body }, ctx) {
    try {
      const hook = await feishuWebhook();
      if (!hook) return error('no feishuWebhook in ~/.tango/config.json — set it or ask the user where to send');
      await sendFeishu(hook, title ?? 'tango', body, ctx.signal);
      return text('sent');
    } catch (e: any) {
      return error(`notify failed: ${e.message}`);
    }
  }
};
