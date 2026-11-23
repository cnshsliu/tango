#!/usr/bin/env node
/**
 * Scripted OpenAI-compatible model for recording demos. Tango and its tools run for real;
 * only the model's words come from a scenario file.
 *
 *   node mock-model.mjs <scenario.json> [port=7690]
 *
 * scenario.json: { "turns": [ [step, step, …], … ] }, one array per user prompt.
 * A step is { "thinking"?: str, "text"?: str, "tools"?: [{ "name", "args" }] }.
 * Which step to play is read off the request itself: turn = user prompts so far − 1,
 * step = assistant messages since the last user prompt. So retries and restarts stay in sync.
 * Steps marked "afterSupplement": true play after an Alt+Enter supplement lands in the turn.
 * Requests without tools (recap, compaction) get a one-line reply.
 * Every request is appended to $MOCK_LOG (default /tmp/mock-model.log) for debugging.
 */
import { createServer } from 'node:http';
import { readFileSync, appendFileSync } from 'node:fs';

const [file, port = '7690'] = process.argv.slice(2);
if (!file) {
  console.error('usage: mock-model.mjs <scenario.json> [port]');
  process.exit(2);
}
const LOG = process.env.MOCK_LOG ?? '/tmp/mock-model.log';
const CPS = Number(process.env.MOCK_CPS ?? 90); // streamed characters per second
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Text of a chat message (string or content-part array). */
const textOf = (m) => (typeof m.content === 'string' ? m.content : (m.content ?? []).map((c) => c.text ?? '').join(''));

function pick(body) {
  const scenario = JSON.parse(readFileSync(file, 'utf8')); // reread: edit scenarios live
  if (!body.tools?.length) return { text: '好的。' };
  const msgs = body.messages ?? [];
  // a prompt is a user message with text; mid-turn supplements (（补充）…) ride the same turn
  const isPrompt = (m) => m.role === 'user' && !textOf(m).startsWith('（补充）');
  const turn = msgs.filter(isPrompt).length - 1;
  let last = -1;
  msgs.forEach((m, i) => isPrompt(m) && (last = i));
  const step = msgs.slice(last + 1).filter((m) => m.role === 'assistant').length;
  const steps = scenario.turns?.[turn] ?? [];
  // a step with "afterSupplement" plays once a supplement has arrived in this turn: the demo
  // shows the model reacting to it, however many steps it took to get there
  const supplemented = msgs.slice(last + 1).some((m) => m.role === 'user' && textOf(m).startsWith('（补充）'));
  const plain = steps.filter((st) => !st.afterSupplement);
  const after = steps.filter((st) => st.afterSupplement);
  if (supplemented && after.length) {
    const firstAfter = msgs.slice(last + 1).findIndex((m) => m.role === 'user' && textOf(m).startsWith('（补充）'));
    const done = msgs.slice(last + 1 + firstAfter).filter((m) => m.role === 'assistant').length;
    return after[done] ?? after[after.length - 1];
  }
  return plain[step] ?? { text: `(scenario has no step ${turn}.${step})` };
}

/** Split text into small chunks so the TUI shows it streaming. */
function chunks(s, n = 3) {
  const out = [];
  const cs = [...s];
  for (let i = 0; i < cs.length; i += n) out.push(cs.slice(i, i + n).join(''));
  return out;
}

createServer(async (req, res) => {
  let raw = '';
  for await (const c of req) raw += c;
  if (!req.url.endsWith('/chat/completions')) {
    res.writeHead(404).end();
    return;
  }
  const body = JSON.parse(raw || '{}');
  const step = pick(body);
  appendFileSync(LOG, JSON.stringify({ at: new Date().toISOString(), n: body.messages?.length, step }) + '\n');
  res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
  const send = (delta, extra = {}) =>
    res.write(`data: ${JSON.stringify({ id: 'mock', object: 'chat.completion.chunk', choices: [{ index: 0, delta, ...extra }] })}\n\n`);
  const pace = (s) => sleep((s.length / CPS) * 1000);

  await sleep(Number(step.delay ?? 400));
  for (const c of chunks(step.thinking ?? '', 4)) {
    send({ reasoning_content: c });
    await pace(c);
  }
  for (const c of chunks(step.text ?? '')) {
    send({ content: c });
    await pace(c);
  }
  (step.tools ?? []).forEach((t, i) =>
    send({ tool_calls: [{ index: i, id: `call_${Date.now()}_${i}`, type: 'function', function: { name: t.name, arguments: JSON.stringify(t.args ?? {}) } }] })
  );
  send({}, { finish_reason: step.tools?.length ? 'tool_calls' : 'stop' });
  const out = Math.ceil((step.text ?? '').length / 2) + 40;
  res.write(`data: ${JSON.stringify({ choices: [], usage: { prompt_tokens: 3200 + (body.messages?.length ?? 0) * 180, completion_tokens: out } })}\n\n`);
  res.end('data: [DONE]\n\n');
}).listen(Number(port), '127.0.0.1', () => console.log(`mock model on http://127.0.0.1:${port}/v1  scenario=${file}`));
