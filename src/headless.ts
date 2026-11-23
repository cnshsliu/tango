/**
 * `tango --format json "<prompt>"`: one JSON object per line on stdout, for scripts and CI.
 * Events: start, step_start, text, tool_use, step_finish, error, done. Nothing else is
 * written to stdout in this mode (warnings stay on stderr).
 */
import type { Agent } from './core/agent.js';
import type { Message } from './core/types.js';

/** tool output in tool_use events is cut to this many chars */
export const JSON_TOOL_OUTPUT_CHARS = 2000;

export type Emit = (event: Record<string, unknown>) => void;

export const stdoutEmit: Emit = (event) => process.stdout.write(JSON.stringify(event) + '\n');

const textOf = (m: Message) =>
  m.content
    .filter((b) => b.type === 'text')
    .map((b) => (b as { text: string }).text)
    .join('');

/** Run one prompt and report it as JSON lines. Resolves to the process exit code. */
export async function runJson(agent: Agent, input: string, sessionId: string, emit: Emit = stdoutEmit): Promise<number> {
  const t0 = Date.now();
  let step = 0;
  let tools = 0;
  const total = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
  let cost = 0;
  const off = [
    agent.events.on('before_request', () => {
      step++;
      emit({ type: 'step_start', step });
    }),
    agent.events.on('message', ({ message }) => {
      const text = textOf(message);
      if (message.role === 'assistant' && text) emit({ type: 'text', step, text });
    }),
    agent.events.on('after_tool', ({ call, result, ms }) => {
      tools++;
      const output = result.content.map((c) => (c.type === 'text' ? c.text : '[image]')).join('\n');
      emit({
        type: 'tool_use', step, id: call.id, name: call.name, input: call.input ?? {},
        output: output.length > JSON_TOOL_OUTPUT_CHARS ? output.slice(0, JSON_TOOL_OUTPUT_CHARS) + `…[${output.length - JSON_TOOL_OUTPUT_CHARS} more chars]` : output,
        isError: result.isError === true, ms
      });
    }),
    agent.events.on('retry', ({ attempt, max, reason, delayMs }) => {
      emit({ type: 'retry', step, attempt, max, reason, delayMs });
    }),
    agent.events.on('turn_end', ({ stopReason, usage, costUsd }) => {
      total.input += usage.input;
      total.output += usage.output;
      total.cacheRead += usage.cacheRead ?? 0;
      total.cacheWrite += usage.cacheWrite ?? 0;
      cost += costUsd;
      emit({ type: 'step_finish', step, stopReason, usage, costUsd });
    })
  ];
  emit({ type: 'start', session: sessionId, model: agent.model.id, cwd: agent.cwd });
  let stopReason: string = 'stop';
  let code = 0;
  try {
    const msg = await agent.prompt(input);
    stopReason = msg.stopReason ?? 'stop';
    if (stopReason === 'aborted') code = 130;
  } catch (e: any) {
    emit({ type: 'error', step, message: String(e?.message ?? e) });
    stopReason = 'error';
    code = 1;
  } finally {
    off.forEach((f) => f());
  }
  emit({ type: 'done', session: sessionId, stopReason, steps: step, toolCalls: tools, usage: total, costUsd: cost, ms: Date.now() - t0 });
  return code;
}
