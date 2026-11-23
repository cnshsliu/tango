import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { existsSync } from 'node:fs';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import type { AddressInfo } from 'node:net';
import { makeAgent, type PermState } from '../src/host.js';
import { SessionStore } from '../src/core/session.js';

/**
 * Hook end-to-end: the real agent assembly (makeAgent) against a fake model,
 * with real shell scripts from tests/fixtures/hooks as the hooks.
 */
const FX = join(import.meta.dirname, 'fixtures', 'hooks');
const hook = (name: string, ...args: string[]) => [join(FX, name), ...args].map((a) => `'${a}'`).join(' ');

const sse = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;
const toolCall = (id: string, name: string, args: object) =>
  sse({ choices: [{ delta: { tool_calls: [{ index: 0, id, function: { name, arguments: JSON.stringify(args) } }] }, finish_reason: 'tool_calls' }] }) + 'data: [DONE]\n\n';
const say = (text: string) =>
  sse({ choices: [{ delta: { content: text } }] }) +
  sse({ choices: [{ delta: {}, finish_reason: 'stop' }], usage: { prompt_tokens: 1, completion_tokens: 1 } }) + 'data: [DONE]\n\n';

/** One scripted reply per request, by index; past the end the model just says "done". */
async function fakeModel(replies: string[]) {
  const requests: any[] = [];
  const server = createServer((req, res) => {
    let raw = '';
    req.on('data', (d) => (raw += d));
    req.on('end', () => {
      requests.push(JSON.parse(raw));
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.end(replies[requests.length - 1] ?? say('done'));
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  return { server, requests, port: (server.address() as AddressInfo).port };
}

/** What the model was sent, as one searchable string. */
const seen = (requests: any[]) => requests.map((r) => JSON.stringify(r.messages)).join('\n');

/** Lines a record.sh hook appended, parsed. */
async function logged(file: string): Promise<any[]> {
  if (!existsSync(file)) return [];
  return (await readFile(file, 'utf8')).split('\n').filter((l) => l.trim()).map((l) => JSON.parse(l));
}

/** A fresh workdir + isolated TANGO_HOME, an agent wired with `hooks`, and the fake model. */
async function setup(hooks: Record<string, unknown>, replies: string[], plugins: Record<string, unknown> = {}) {
  const work = await mkdtemp(join(tmpdir(), 'tango-hook-work-'));
  const home = await mkdtemp(join(tmpdir(), 'tango-hook-home-'));
  for (const [name, meta] of Object.entries(plugins)) {
    await mkdir(join(home, 'plugins', name), { recursive: true });
    await writeFile(join(home, 'plugins', name, 'plugin.json'), JSON.stringify(meta));
  }
  const prevHome = process.env.TANGO_HOME;
  process.env.TANGO_HOME = home; // plugins are read from here at assembly
  const model = await fakeModel(replies);
  const store = await SessionStore.create(join(home, 'sessions'), work);
  const perm: PermState = { mode: 'auto' };
  const agent = makeAgent(
    { id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: `http://127.0.0.1:${model.port}/v1`, apiKey: 'test' },
    store, work, perm, { checkpoint: 'off', hooks }
  );
  process.env.TANGO_HOME = prevHome;
  const done = async () => {
    model.server.close();
    await rm(work, { recursive: true, force: true });
    await rm(home, { recursive: true, force: true });
  };
  return { agent, store, work, model, log: join(work, 'hook-log.jsonl'), done };
}

// ---------- PreToolUse ----------

test('PreToolUse: exit 2 refuses the call, the command never runs, the model sees the reason', async () => {
  const t = await setup(
    { PreToolUse: [{ matcher: 'bash', command: hook('guard-rm.sh') }] },
    [toolCall('c1', 'bash', { command: 'mkdir keep && rm -rf keep && touch ran.txt' })]
  );
  try {
    await t.agent.prompt('clean up');
    assert.ok(!existsSync(join(t.work, 'ran.txt')), 'the blocked command did not run');
    assert.ok(!existsSync(join(t.work, 'keep')), 'not even its first step');
    assert.match(seen(t.model.requests), /blocked: PreToolUse hook: guard: rm -rf is not allowed/);
  } finally {
    await t.done();
  }
});

test('PreToolUse: exit 0 lets the call run, and the hook gets the documented payload in the cwd', async () => {
  const t = await setup(
    { PreToolUse: [{ command: hook('record.sh', 'hook-log.jsonl') }] }, // relative: hooks run in the cwd
    [toolCall('c1', 'bash', { command: 'echo hi > ok.txt' })]
  );
  try {
    await t.agent.prompt('write it');
    assert.ok(existsSync(join(t.work, 'ok.txt')), 'the allowed command ran');
    const [ev] = await logged(t.log);
    assert.equal(ev.hook_event_name, 'PreToolUse');
    assert.equal(ev.tool_name, 'bash');
    assert.deepEqual(ev.tool_input, { command: 'echo hi > ok.txt' });
    assert.equal(ev.session_id, t.store.id);
    assert.equal(ev.cwd, t.work);
  } finally {
    await t.done();
  }
});

test('PreToolUse: the matcher limits which tools the hook sees', async () => {
  const t = await setup(
    { PreToolUse: [{ matcher: 'write|edit', command: hook('always-no.sh', 'pre') }] },
    [toolCall('c1', 'bash', { command: 'touch b.txt' }), toolCall('c2', 'write', { path: 'w.txt', content: 'x' })]
  );
  try {
    await t.agent.prompt('go');
    assert.ok(existsSync(join(t.work, 'b.txt')), 'bash is not matched, so it ran');
    assert.ok(!existsSync(join(t.work, 'w.txt')), 'write is matched, so it was refused');
    assert.match(seen(t.model.requests), /always-no pre/);
  } finally {
    await t.done();
  }
});

// ---------- PostToolUse ----------

test('PostToolUse: exit 2 cannot undo the tool, but its stderr rides along with the result', async () => {
  const t = await setup(
    { PostToolUse: [{ matcher: 'write', command: hook('lint-note.sh') }] },
    [toolCall('c1', 'write', { path: 'hello.txt', content: 'hello' })]
  );
  try {
    await t.agent.prompt('write hello');
    assert.equal(await readFile(join(t.work, 'hello.txt'), 'utf8'), 'hello', 'the write happened');
    const toolMsg = t.model.requests[1].messages.find((m: any) => m.role === 'tool');
    assert.match(toolMsg.content, /\[PostToolUse hook\] lint: 2 warnings in hello\.txt/);
  } finally {
    await t.done();
  }
});

test('PostToolUse: payload carries the tool output and its error flag', async () => {
  const t = await setup(
    { PostToolUse: [{ command: hook('record.sh', 'hook-log.jsonl') }] },
    [toolCall('c1', 'bash', { command: 'echo post-ok' }), toolCall('c2', 'bash', { command: 'exit 7' })]
  );
  try {
    await t.agent.prompt('run two');
    const [ok, bad] = await logged(t.log);
    assert.equal(ok.hook_event_name, 'PostToolUse');
    assert.equal(ok.tool_name, 'bash');
    assert.match(ok.tool_response.text, /post-ok/);
    assert.equal(ok.tool_response.is_error, false);
    assert.equal(bad.tool_response.is_error, true, 'a failing command is flagged');
  } finally {
    await t.done();
  }
});

test('PostToolUse: a call refused by PreToolUse still reports, flagged as an error', async () => {
  const t = await setup(
    {
      PreToolUse: [{ command: hook('guard-rm.sh') }],
      PostToolUse: [{ command: hook('record.sh', 'hook-log.jsonl') }]
    },
    [toolCall('c1', 'bash', { command: 'rm -rf x' })]
  );
  try {
    await t.agent.prompt('go');
    const [ev] = await logged(t.log);
    assert.equal(ev.tool_response.is_error, true);
    assert.match(ev.tool_response.text, /blocked: PreToolUse hook/);
  } finally {
    await t.done();
  }
});

// ---------- UserPromptSubmit ----------

test('UserPromptSubmit: exit 2 rejects the prompt before it is saved or sent', async () => {
  const t = await setup({ UserPromptSubmit: [{ command: hook('no-secret.sh') }] }, [say('hi')]);
  try {
    await assert.rejects(t.agent.prompt('my key is sk-abc123'), /prompt blocked by hook: prompt contains an API key/);
    assert.equal(t.model.requests.length, 0, 'nothing reached the model');
    assert.equal(t.store.messages().length, 0, 'nothing was saved to the session');
    // a clean prompt still goes through afterwards
    const msg = await t.agent.prompt('hello');
    assert.equal(msg.stopReason, 'stop');
    assert.equal(t.model.requests.length, 1);
  } finally {
    await t.done();
  }
});

test('UserPromptSubmit: payload carries the prompt text', async () => {
  const t = await setup({ UserPromptSubmit: [{ command: hook('record.sh', 'hook-log.jsonl') }] }, [say('hi')]);
  try {
    await t.agent.prompt('第一个问题');
    const [ev] = await logged(t.log);
    assert.equal(ev.hook_event_name, 'UserPromptSubmit');
    assert.equal(ev.prompt, '第一个问题');
  } finally {
    await t.done();
  }
});

// ---------- Stop ----------

test('Stop: exit 2 sends the model back once, then stop_hook_active lets it finish', async () => {
  const t = await setup(
    { Stop: [{ command: hook('tests-green.sh') }, { command: hook('record.sh', 'hook-log.jsonl') }] },
    [say('first answer'), say('tests ran, all green')]
  );
  try {
    const msg = await t.agent.prompt('fix it');
    assert.equal(t.model.requests.length, 2, 'one extra round');
    assert.match(seen([t.model.requests[1]]), /\[Stop hook\] run the tests before finishing/);
    const text = msg.content.map((b: any) => b.text ?? '').join('');
    assert.equal(text, 'tests ran, all green', 'the final answer is the second one');
    // record.sh runs after tests-green.sh, so it only sees the stop that was let through
    const events = await logged(t.log);
    assert.equal(events.length, 1);
    assert.equal(events[0].stop_hook_active, true);
    assert.equal(events[0].last_assistant_message, 'tests ran, all green');
  } finally {
    await t.done();
  }
});

test('Stop: a hook that always says no is honored at most 3 times', async () => {
  const t = await setup({ Stop: [{ command: hook('always-no.sh', 'stop') }] }, []);
  try {
    const msg = await t.agent.prompt('hi');
    assert.equal(msg.stopReason, 'stop');
    assert.equal(t.model.requests.length, 4, 'one answer + 3 continuations, then it ends');
  } finally {
    await t.done();
  }
});

test('Stop: not called while the model is still using tools', async () => {
  const t = await setup(
    { Stop: [{ command: hook('record.sh', 'hook-log.jsonl') }] },
    [toolCall('c1', 'bash', { command: 'true' }), toolCall('c2', 'bash', { command: 'true' }), say('done')]
  );
  try {
    await t.agent.prompt('go');
    assert.equal((await logged(t.log)).length, 1, 'only the final stop');
  } finally {
    await t.done();
  }
});

// ---------- failure modes, chains, formats, plugins ----------

test('a crashing hook (exit 1) and a missing script only warn; the tool still runs', async () => {
  const t = await setup(
    { PreToolUse: [{ command: hook('crash.sh') }, { command: '/no/such/hook.sh' }] },
    [toolCall('c1', 'bash', { command: 'touch ran.txt' })]
  );
  try {
    await t.agent.prompt('go');
    assert.ok(existsSync(join(t.work, 'ran.txt')));
    assert.doesNotMatch(seen(t.model.requests), /blocked/);
  } finally {
    await t.done();
  }
});

test('a hung hook is killed at its timeout and does not block', async () => {
  const t = await setup(
    { PreToolUse: [{ command: hook('slow.sh'), timeout: 0.3 }] },
    [toolCall('c1', 'bash', { command: 'touch ran.txt' })]
  );
  try {
    const t0 = Date.now();
    await t.agent.prompt('go');
    assert.ok(Date.now() - t0 < 4000, 'did not wait for the 5s sleep');
    assert.ok(existsSync(join(t.work, 'ran.txt')));
  } finally {
    await t.done();
  }
});

test('chain: hooks run in order and the first exit 2 stops the rest', async () => {
  const t = await setup(
    {
      PreToolUse: [
        { command: hook('record.sh', 'hook-log.jsonl') },
        { command: hook('always-no.sh', 'second') },
        { command: 'touch third-ran.txt' }
      ]
    },
    [toolCall('c1', 'bash', { command: 'touch ran.txt' })]
  );
  try {
    await t.agent.prompt('go');
    assert.equal((await logged(t.log)).length, 1, 'the first hook ran');
    assert.ok(!existsSync(join(t.work, 'third-ran.txt')), 'the third never ran');
    assert.ok(!existsSync(join(t.work, 'ran.txt')));
  } finally {
    await t.done();
  }
});

test('Claude Code nested format works unchanged', async () => {
  const t = await setup(
    { PreToolUse: [{ matcher: 'bash', hooks: [{ type: 'command', command: hook('guard-rm.sh'), timeout: 5 }] }] },
    [toolCall('c1', 'bash', { command: 'rm -rf x' })]
  );
  try {
    await t.agent.prompt('go');
    assert.match(seen(t.model.requests), /guard: rm -rf is not allowed/);
  } finally {
    await t.done();
  }
});

test('plugin hooks run after config hooks; disabled plugins never run', async () => {
  const t = await setup(
    { PreToolUse: [{ command: hook('record.sh', 'hook-log.jsonl') }] },
    [toolCall('c1', 'bash', { command: 'touch ran.txt' })],
    {
      guard: { name: 'guard', hooks: { PreToolUse: [{ command: hook('always-no.sh', 'plugin') }] } },
      off: { name: 'off', enabled: false, hooks: { PreToolUse: [{ command: 'touch off-ran.txt' }] } }
    }
  );
  try {
    await t.agent.prompt('go');
    assert.equal((await logged(t.log)).length, 1, 'the config hook ran first');
    assert.match(seen(t.model.requests), /always-no plugin/, 'then the plugin blocked');
    assert.ok(!existsSync(join(t.work, 'off-ran.txt')), 'the disabled plugin stayed out');
  } finally {
    await t.done();
  }
});

test('legacy beforeTool/afterTool keys still notify and never block', async () => {
  const t = await setup(
    // legacy hooks run in tango's process cwd (not the session's), so they get absolute paths
    { beforeTool: `cat > "$TANGO_LEGACY_DIR/legacy-before.json"; exit 2`, afterTool: `cat > "$TANGO_LEGACY_DIR/legacy-after.json"` },
    [toolCall('c1', 'bash', { command: 'touch ran.txt' })]
  );
  process.env.TANGO_LEGACY_DIR = t.work;
  try {
    await t.agent.prompt('go');
    assert.ok(existsSync(join(t.work, 'ran.txt')), 'exit 2 from a legacy hook does not block');
    // legacy hooks are fire-and-forget, and `cat >` creates the file before it writes the payload:
    // wait until each file holds complete JSON, not just until it exists
    const landed = async (name: string): Promise<{ phase?: string } | undefined> => {
      for (let i = 0; i < 100; i++) {
        try {
          return JSON.parse(await readFile(join(t.work, name), 'utf8'));
        } catch {
          await new Promise((r) => setTimeout(r, 20));
        }
      }
      return undefined;
    };
    assert.equal((await landed('legacy-before.json'))?.phase, 'before');
    assert.equal((await landed('legacy-after.json'))?.phase, 'after');
  } finally {
    delete process.env.TANGO_LEGACY_DIR;
    await t.done();
  }
});

// ---------- stdout JSON ----------

const json = (o: object) => hook('json-out.sh', JSON.stringify(o));

test('PreToolUse: stdout {"decision":"block"} refuses the call like exit 2, with its reason', async () => {
  const t = await setup(
    { PreToolUse: [{ matcher: 'bash', command: json({ decision: 'block', reason: 'json says no' }) }] },
    [toolCall('c1', 'bash', { command: 'touch ran.txt' })]
  );
  try {
    await t.agent.prompt('go');
    assert.ok(!existsSync(join(t.work, 'ran.txt')));
    assert.match(seen(t.model.requests), /blocked: PreToolUse hook: json says no/);
  } finally {
    await t.done();
  }
});

test('PreToolUse: additionalContext reaches the model on its next request; approve lets the call run', async () => {
  const t = await setup(
    { PreToolUse: [{ command: json({ decision: 'approve', additionalContext: 'remember: tabs not spaces' }) }] },
    [toolCall('c1', 'bash', { command: 'touch ran.txt' })]
  );
  try {
    await t.agent.prompt('go');
    assert.ok(existsSync(join(t.work, 'ran.txt')));
    assert.doesNotMatch(JSON.stringify(t.model.requests[0].messages), /tabs not spaces/);
    assert.match(JSON.stringify(t.model.requests[1].messages), /\[hook context\] remember: tabs not spaces/);
  } finally {
    await t.done();
  }
});

test('PostToolUse: additionalContext rides along with the tool result', async () => {
  const t = await setup(
    { PostToolUse: [{ command: json({ additionalContext: 'formatted by prettier' }) }] },
    [toolCall('c1', 'bash', { command: 'echo hi' })]
  );
  try {
    await t.agent.prompt('go');
    const toolMsg = t.model.requests[1].messages.find((m: any) => m.role === 'tool');
    assert.match(toolMsg.content, /\[PostToolUse hook context\] formatted by prettier/);
  } finally {
    await t.done();
  }
});

test('PostToolUse: {"continue":false} ends the turn after the tool, stopReason is the last word', async () => {
  const t = await setup(
    { PostToolUse: [{ command: json({ continue: false, stopReason: 'budget reached' }) }] },
    [toolCall('c1', 'bash', { command: 'touch ran.txt' }), toolCall('c2', 'bash', { command: 'touch second.txt' })]
  );
  try {
    const msg = await t.agent.prompt('go');
    assert.ok(existsSync(join(t.work, 'ran.txt')), 'the tool itself ran');
    assert.equal(t.model.requests.length, 1, 'no further model request');
    assert.ok(!existsSync(join(t.work, 'second.txt')));
    assert.match(JSON.stringify(msg.content), /\[hook\] budget reached/);
  } finally {
    await t.done();
  }
});

test('UserPromptSubmit: additionalContext is added before the first request; decision block rejects', async () => {
  const t = await setup({ UserPromptSubmit: [{ command: json({ additionalContext: 'today is Monday' }) }] }, [say('ok')]);
  try {
    await t.agent.prompt('what day?');
    assert.match(JSON.stringify(t.model.requests[0].messages), /\[hook context\] today is Monday/);
  } finally {
    await t.done();
  }
  const b = await setup({ UserPromptSubmit: [{ command: json({ decision: 'block', reason: 'no prompts today' }) }] }, []);
  try {
    await assert.rejects(b.agent.prompt('hi'), /prompt blocked by hook: no prompts today/);
    assert.equal(b.model.requests.length, 0);
  } finally {
    await b.done();
  }
});

test('Stop: {"decision":"block"} sends the model back like exit 2', async () => {
  const t = await setup({ Stop: [{ command: json({ decision: 'block', reason: 'run the tests first' }) }] }, [say('done'), say('tests ran')]);
  try {
    await t.agent.prompt('go');
    assert.match(seen(t.model.requests), /\[Stop hook\] run the tests first/);
  } finally {
    await t.done();
  }
});

// ---------- SessionStart ----------

test('SessionStart: payload says startup, runs once per session before the first prompt', async () => {
  const t = await setup({ SessionStart: [{ command: hook('record.sh', 'hook-log.jsonl') }] }, [say('a'), say('b')]);
  try {
    await t.agent.prompt('one');
    await t.agent.prompt('two');
    const evs = await logged(t.log);
    assert.equal(evs.length, 1, 'once, not per prompt');
    assert.equal(evs[0].hook_event_name, 'SessionStart');
    assert.equal(evs[0].source, 'startup');
    assert.equal(evs[0].session_id, t.store.id);
  } finally {
    await t.done();
  }
});

test('SessionStart: exit 2 cannot block a session (only warns); additionalContext reaches the first request', async () => {
  const t = await setup(
    { SessionStart: [{ command: hook('always-no.sh', 'start') }, { command: json({ additionalContext: 'project uses pnpm' }) }] },
    [say('ok')]
  );
  try {
    await t.agent.prompt('hi');
    assert.equal(t.model.requests.length, 1, 'the prompt still went out');
    assert.doesNotMatch(seen(t.model.requests), /always-no start/);
  } finally {
    await t.done();
  }
  const c = await setup({ SessionStart: [{ command: json({ additionalContext: 'project uses pnpm' }) }] }, [say('ok')]);
  try {
    await c.agent.prompt('hi');
    assert.match(JSON.stringify(c.model.requests[0].messages), /\[hook context\] project uses pnpm/);
  } finally {
    await c.done();
  }
});

// ---------- PreCompact / PostCompact ----------

/** Two short turns (4 messages), so there is something to compact; reply 3 is the summary. */
async function twoTurns(t: Awaited<ReturnType<typeof setup>>) {
  await t.agent.prompt('first');
  await t.agent.prompt('second');
}

test('PreCompact: exit 2 skips the compaction, nothing is summarized', async () => {
  const t = await setup({ PreCompact: [{ command: hook('always-no.sh', 'compact') }] }, [say('a'), say('b'), say('SUMMARY')]);
  try {
    await twoTurns(t);
    assert.equal(await t.agent.compact(), false);
    assert.equal(t.model.requests.length, 2, 'no summary request');
  } finally {
    await t.done();
  }
});

test('PreCompact: payload carries the trigger; the matcher selects manual or auto', async () => {
  const t = await setup(
    { PreCompact: [{ matcher: 'manual', command: hook('record.sh', 'hook-log.jsonl') }, { matcher: 'auto', command: hook('always-no.sh', 'auto') }] },
    [say('a'), say('b'), say('SUMMARY')]
  );
  try {
    await twoTurns(t);
    assert.equal(await t.agent.compact(), true, 'the auto-only blocker did not run');
    const [ev] = await logged(t.log);
    assert.equal(ev.hook_event_name, 'PreCompact');
    assert.equal(ev.trigger, 'manual');
    assert.equal(ev.session_id, t.store.id);
  } finally {
    await t.done();
  }
});

test('PostCompact: payload carries the trigger and the new summary', async () => {
  const t = await setup({ PostCompact: [{ command: hook('record.sh', 'hook-log.jsonl') }] }, [say('a'), say('b'), say('SUMMARY TEXT')]);
  try {
    await twoTurns(t);
    assert.equal(await t.agent.compact(), true);
    const [ev] = await logged(t.log);
    assert.equal(ev.hook_event_name, 'PostCompact');
    assert.equal(ev.trigger, 'manual');
    assert.equal(ev.summary, 'SUMMARY TEXT');
  } finally {
    await t.done();
  }
});

test('PostCompact: exit 2 cannot undo the compaction (only warns); additionalContext goes to the next request', async () => {
  const t = await setup(
    // context collected before the chain stops still counts
    { PostCompact: [{ command: json({ additionalContext: 're-read AGENTS.md' }) }, { command: hook('always-no.sh', 'post') }] },
    [say('a'), say('b'), say('SUMMARY'), say('c')]
  );
  try {
    await twoTurns(t);
    assert.equal(await t.agent.compact(), true, 'already compacted, exit 2 does not change that');
    await t.agent.prompt('third');
    assert.match(JSON.stringify(t.model.requests[3].messages), /\[hook context\] re-read AGENTS\.md/);
  } finally {
    await t.done();
  }
});
