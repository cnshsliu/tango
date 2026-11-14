import { createInterface } from 'node:readline';
import * as http from 'node:http';
import { join } from 'node:path';
import { Agent } from './core/agent.js';
import {
  hasKey,
  keyProblem,
  loadConfig,
  loadModels,
  pickModel,
  saveConfig,
  sessionsDir,
  tangoDir
} from './core/config.js';
import { listDrafts, promoteDraft, saveDraft } from './core/skills.js';
import { EventBus } from './core/events.js';
import { keyEnvName } from './core/models/util.js';
import { SessionStore, type SessionSummary } from './core/session.js';
import { ToolRegistry } from './core/tools/registry.js';
import { bashTool } from './core/tools/bash.js';
import { editTool, readTool, writeTool } from './core/tools/fs.js';
import { diffTool } from './core/tools/diff.js';
import { installDiagnostics } from './core/diagnostics.js';
import { findTool, grepTool, lsTool } from './core/tools/search.js';
import { webSearchTool } from './core/tools/web-search.js';
import { webFetchTool } from './core/tools/web-fetch.js';
import { officeReadTool, officeWriteTool } from './core/tools/office/index.js';
import { installPermissionGate, type AskFn, type PermissionMode } from './core/permissions.js';
import { addTask, daemonLoop, feishuWebhook, lastRunAt, loadTasks, parseInterval, removeTask, runDueOnce, sendFeishu, summarizeOutput } from './core/scheduler.js';
import { notifyTool } from './core/tools/notify.js';
import type { Message, ModelSpec, ToolCallBlock } from './core/types.js';
import type { Task } from './core/scheduler.js';

const VERSION = '0.0.1';

// Node's fetch ignores http(s)_proxy unless told to; many users in China need it.
const proxyEnv = ['HTTPS_PROXY', 'https_proxy', 'HTTP_PROXY', 'http_proxy'].some((k) => process.env[k]);
if (proxyEnv && !process.env.NODE_USE_ENV_PROXY) (http as any).setGlobalProxyFromEnv?.();

const HELP = `tango ${VERSION} — a light agent harness

usage:
  tango [prompt...]      run one prompt and exit
  tango                  interactive session (REPL)
  tango -c               continue the most recent session
  tango -r <id>          resume a session by id or prefix
  tango -m <model>       pick a model (see --models)
  tango --models         list models and which have keys
  tango --sessions       list sessions, newest first
  tango task             list scheduled tasks
  tango task add <name> <every:30m|2h> <prompt...> [--mode auto]
  tango task rm <name>   remove a task
  tango task run [--loop]  run due tasks once, or keep running as a daemon
options:
  --cwd <dir>            work in <dir> (default: current directory)
  -y / --yes             run risky commands without asking
  --readonly             block all writes and shell commands
  -h / --help, -V / --version

permissions (default "ask"): reads and edits inside the working directory run freely;
deletes, git push/reset, sudo, writes outside cwd and similar ask first. One-shot runs
cannot ask, so those calls are refused unless --yes is given.`;

const CMD_HELP = `commands:
  /model <id>  switch model (saved as default)
  /models      list models
  /mode [ask|auto|readonly]  show or change permission mode
  /sessions    list sessions, newest first
  /compact     summarize and drop older context
  /save-as-skill [描述]  save this session's flow as a skill draft (then /save-as-skill finish <file>)
  /cost        session cost
  /new         start a fresh session
  /help        this
  /exit        quit (ctrl+d also works, ctrl+c interrupts a running turn)`;

const dim = (s: string) => `\x1b[2m${s}\x1b[0m`;
const red = (s: string) => `\x1b[31m${s}\x1b[0m`;
const cyan = (s: string) => `\x1b[36m${s}\x1b[0m`;

interface Args {
  help: boolean;
  version: boolean;
  listModels: boolean;
  listSessions: boolean;
  continue: boolean;
  resume?: string;
  model?: string;
  cwd?: string;
  input?: string;
  yes: boolean;
  readonly: boolean;
  task?: string[];
}

function parseArgs(argv: string[]): Args {
  const a: Args = {
    help: false,
    version: false,
    listModels: false,
    listSessions: false,
    continue: false,
    yes: false,
    readonly: false
  };
  const rest: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    switch (argv[i]) {
      case '-y':
      case '--yes':
        a.yes = true;
        break;
      case '--readonly':
        a.readonly = true;
        break;
      case '-h':
      case '--help':
        a.help = true;
        break;
      case '-V':
      case '--version':
        a.version = true;
        break;
      case '--models':
        a.listModels = true;
        break;
      case '--sessions':
        a.listSessions = true;
        break;
      case '-c':
      case '--continue':
        a.continue = true;
        break;
      case '-r':
      case '--resume':
        a.resume = argv[++i];
        break;
      case '-m':
      case '--model':
        a.model = argv[++i];
        break;
      case '--cwd':
        a.cwd = argv[++i];
        break;
      default:
        rest.push(argv[i]);
    }
  }
  if (rest[0] === 'task') {
    a.task = rest.slice(1);
    return a;
  }
  if (rest.length) a.input = rest.join(' ');
  return a;
}

function buildTools(): ToolRegistry {
  const tools = new ToolRegistry();
  for (const t of [readTool, writeTool, editTool, bashTool, grepTool, findTool, lsTool, webSearchTool, webFetchTool, officeReadTool, officeWriteTool, notifyTool, diffTool])
    tools.register(t);
  return tools;
}

/** Process-wide permission state; the REPL installs `ask`, one-shot runs leave it unset. */
const perm: { mode: PermissionMode; ask?: AskFn } = { mode: 'ask' };

function toolSummary(call: ToolCallBlock): string {
  const input = (call.input ?? {}) as Record<string, unknown>;
  const key = ['command', 'path', 'url', 'query', 'pattern'].find((k) => typeof input[k] === 'string');
  const detail = String(key ? input[key] : JSON.stringify(input));
  return (detail.length > 90 ? detail.slice(0, 90) + '…' : detail).replace(/\n/g, ' ');
}

function textOf(m: Message): string {
  return m.content
    .filter((b) => b.type === 'text')
    .map((b) => (b as { text: string }).text)
    .join('\n');
}

function makeAgent(model: ModelSpec, store: SessionStore, cwd: string): Agent {
  const events = new EventBus();
  // gate first: a vetoed call is never announced as running
  installPermissionGate(events, { mode: () => perm.mode, cwd, ask: (c, r) => (perm.ask ? perm.ask(c, r) : Promise.resolve(false)) });
  events.on('before_tool', ({ call }) => {
    if (process.stdout.isTTY) process.stdout.write(dim(`⚙ ${call.name} ${toolSummary(call)}\n`));
  });
  installDiagnostics(events, cwd);
  return new Agent({ store, tools: buildTools(), model, cwd, events });
}

function printModels(models: ModelSpec[], current?: string): void {
  const groups: [string, ModelSpec[]][] = [
    ['tknet (own gateway)', models.filter((m) => m.region === 'gateway')],
    ['China (mainland endpoints)', models.filter((m) => m.region === 'cn')],
    ['International (outside mainland China)', models.filter((m) => m.region === 'intl')],
    ['Other (from models.json, no region)', models.filter((m) => !m.region)]
  ];
  for (const [title, list] of groups) {
    if (!list.length) continue;
    console.log(cyan(title));
    for (const m of list) {
      const mark = hasKey(m) ? '✓' : '·';
      const source = keyEnvName(m) ?? (m.apiKey ? 'literal key' : 'no key');
      const note = hasKey(m) ? `  ${source}` : `  (${keyProblem(m)})`;
      console.log(`  ${mark} ${m.id}${m.id === current ? cyan('  (current)') : ''}${dim(note)}`);
    }
  }
  console.log(dim(`\nconfig: ${tangoDir()}/config.json · model overrides: ${tangoDir()}/models.json`));
}

function printSessions(sessions: SessionSummary[]): void {
  if (!sessions.length) return console.log(dim('no sessions yet'));
  for (const s of sessions) {
    console.log(`${s.id.slice(0, 8)}  ${s.createdAt.slice(0, 16)}  ${String(s.messages).padStart(4)} msgs  ${s.cwd}  ${s.name ?? ''}`);
  }
}

async function resolveSessionId(prefix: string): Promise<string> {
  const sessions = await SessionStore.list(sessionsDir());
  const hits = sessions.filter((s) => s.id.startsWith(prefix));
  if (hits.length === 1) return hits[0].id;
  if (!hits.length) throw new Error(`no session matches "${prefix}" (tango --sessions)`);
  throw new Error(`ambiguous "${prefix}": ${hits.map((s) => s.id.slice(0, 8)).join(', ')}`);
}

async function openAgent(args: Args): Promise<{ agent: Agent; store: SessionStore }> {
  const cfg = await loadConfig();
  const models = await loadModels();
  const model = pickModel(models, cfg, args.model);

  let store: SessionStore;
  if (args.resume) {
    store = await SessionStore.open(sessionsDir(), await resolveSessionId(args.resume));
  } else if (args.continue) {
    const [latest] = await SessionStore.list(sessionsDir());
    if (!latest) throw new Error('no sessions to continue yet');
    store = await SessionStore.open(sessionsDir(), latest.id);
  } else {
    store = await SessionStore.create(sessionsDir(), args.cwd ?? process.cwd());
  }

  const cwd = args.cwd ?? store.meta()?.cwd ?? process.cwd();
  // honour a model switch made earlier in a resumed session unless the user asked explicitly
  const stored = !args.model && !cfg.model ? store.lastModel() : undefined;
  const effective = stored ? models.find((m) => m.id === stored) ?? model : model;
  if (!hasKey(effective)) console.error(red(`⚠ ${keyProblem(effective)}`));
  return { agent: makeAgent(effective, store, cwd), store };
}

async function runOnce(agent: Agent, input: string): Promise<void> {
  let streamed = false;
  const msg = await agent.prompt(input, (ev) => {
    if (ev.type === 'text') {
      streamed = true;
      process.stdout.write(ev.delta);
    }
  });
  process.stdout.write('\n');
  if (!streamed && msg.stopReason !== 'aborted') console.error(dim('[model returned no text]'));
  if (msg.stopReason === 'aborted') {
    console.error(dim('[interrupted]'));
    process.exitCode = 130;
  }
}

async function interactive(agent: Agent, store: SessionStore): Promise<void> {
  console.log(dim(`tango ${VERSION} · ${agent.model.id} · ${perm.mode} · session ${store.id.slice(0, 8)} · ${store.meta()?.cwd ?? ''}`));
  console.log(dim('/help for commands\n'));

  const cfg = await loadConfig();
  const models = await loadModels();
  const rl = createInterface({ input: process.stdin, output: process.stdout, prompt: cyan('› ') });

  let running = false;
  let pumping = false;
  let closed = false;
  const queue: string[] = [];
  // a pending y/n answer takes the next input line instead of the prompt queue
  let answer: ((line: string) => void) | undefined;

  perm.ask = (call, reason) =>
    new Promise((resolveAsk) => {
      process.stdout.write(`\n${red('?')} ${call.name}: ${toolSummary(call)}\n  ${dim(reason)} — allow? [y/N/a=always] `);
      answer = (line) => {
        answer = undefined;
        const a = line.trim().toLowerCase();
        if (a === 'a' || a === 'always') {
          perm.mode = 'auto';
          console.log(dim('permission mode → auto for this session'));
        }
        resolveAsk(a === 'y' || a === 'yes' || a === 'a' || a === 'always');
      };
    });

  rl.on('close', () => {
    closed = true;
  });

  rl.on('SIGINT', () => {
    if (answer) {
      answer('n');
      return;
    }
    if (running) {
      agent.abort();
      console.log(dim('\n^C — interrupting…'));
    } else {
      rl.close();
    }
  });

  /** returns false when the REPL should close */
  async function handleCommand(line: string): Promise<boolean> {
    const [cmd, ...restArgs] = line.slice(1).split(/\s+/);
    const arg = restArgs.join(' ').trim();
    switch (cmd) {
      case '':
        return true;
      case 'help':
        console.log(CMD_HELP);
        return true;
      case 'exit':
      case 'quit':
      case 'q':
        return false;
      case 'models':
        printModels(models, agent.model.id);
        return true;
      case 'model': {
        if (!arg) return console.log(`current model: ${agent.model.id}`), true;
        try {
          const next = pickModel(models, {}, arg);
          agent.setModel(next);
          await saveConfig({ ...cfg, model: next.id });
          console.log(dim(`model → ${next.id}${hasKey(next) ? '' : ` (${keyProblem(next)})`}`));
        } catch (e: any) {
          console.error(red(e.message));
        }
        return true;
      }
      case 'mode': {
        if (arg === 'ask' || arg === 'auto' || arg === 'readonly') perm.mode = arg;
        else if (arg) console.log(red('mode must be ask, auto or readonly'));
        console.log(dim(`permission mode: ${perm.mode}`));
        return true;
      }
      case 'sessions':
        printSessions(await SessionStore.list(sessionsDir()));
        return true;
      case 'compact': {
        const ok = await agent.compact();
        console.log(dim(ok ? `compacted → ${store.messageCount()} messages in context` : 'nothing worth compacting'));
        return true;
      }
      case 'save-as-skill':
      case 'skill': {
        try {
          if (arg === 'finish' || arg === '定稿') {
            console.error(red('usage: /save-as-skill finish <draft file>'));
            for (const d of await listDrafts()) console.log(dim('  ' + d));
            return true;
          }
          if (arg.startsWith('finish ') || arg.startsWith('定稿 ')) {
            const file = arg.replace(/^(finish|定稿)\s+/, '');
            const root = process.cwd().includes('.tango') ? process.cwd() : join(process.cwd(), '.tango', 'skills');
            const s = await promoteDraft(file, root);
            console.log(dim(`skill → ${s.file}`));
            console.log(dim(`(project skill: ${root} · global: move it to ${join(tangoDir(), 'skills')})`));
            return true;
          }
          const d = await saveDraft({
            title: arg || textOf(store.allMessages().find((m) => m.role === 'user') ?? { role: 'user', content: [] }) || 'workflow',
            description: arg || undefined,
            messages: store.allMessages(),
            cwd: process.cwd()
          });
          console.log(dim(`draft → ${d.file}`));
          console.log(dim('open it, write a real description + steps, then: /save-as-skill finish <file>'));
        } catch (e: any) {
          console.error(red(e.message));
        }
        return true;
      }
      case 'cost':
        console.log(dim(`$${agent.totalCostUsd.toFixed(4)} this process · ${store.messageCount()} messages in context`));
        return true;
      case 'new': {
        store = await SessionStore.create(sessionsDir(), process.cwd());
        agent = makeAgent(agent.model, store, process.cwd());
        console.log(dim(`new session ${store.id.slice(0, 8)}`));
        return true;
      }
      default:
        console.log(red(`unknown command /${cmd} — ${dim('/help')}`));
        return true;
    }
  }

  async function handle(line: string): Promise<void> {
    if (!line.trim()) return;
    if (line.startsWith('/')) {
      if (!(await handleCommand(line))) {
        queue.length = 0;
        rl.close();
      }
      return;
    }
    running = true;
    let streamed = false;
    try {
      const msg = await agent.prompt(line, (ev) => {
        if (ev.type === 'text') {
          streamed = true;
          process.stdout.write(ev.delta);
        }
      });
      if (!streamed) console.log(textOf(msg));
      else process.stdout.write('\n');
      if (msg.stopReason === 'aborted') console.log(dim('[interrupted]'));
      if (process.stdout.isTTY) console.log(dim(`$${agent.totalCostUsd.toFixed(4)} · ${agent.model.id}`));
    } catch (e: any) {
      console.error(red(`error: ${e?.message ?? e}`));
    }
    running = false;
  }

  async function pump(): Promise<void> {
    if (pumping) return;
    pumping = true;
    while (queue.length) {
      const line = queue.shift();
      if (line !== undefined) await handle(line);
      if (!closed) rl.prompt();
    }
    pumping = false;
  }

  rl.on('line', (raw) => {
    if (answer) return answer(raw);
    queue.push(raw);
    void pump();
  });

  rl.prompt();
  await new Promise<void>((resolve) => rl.once('close', resolve));
  // piped input: readline closes at EOF while queued lines may still be pending
  while (pumping || queue.length) {
    if (!pumping) await pump();
    else await new Promise((r) => setImmediate(r));
  }
  if (running) agent.abort();
  console.log(dim(`bye ($${agent.totalCostUsd.toFixed(4)} this session)`));
}

async function taskMain(args: Args, rest: string[]): Promise<void> {
  const [sub, ...a] = rest;
  if (!sub || sub === 'list') {
    const tasks = await loadTasks();
    if (!tasks.length) return console.log(dim('no tasks yet: tango task add <name> <every> <prompt...>  (e.g. every: 30m, 2h)'));
    for (const t of tasks) {
      const last = await lastRunAt(t.name);
      console.log(`${t.name}  every ${t.everyMinutes}m  ${t.mode ?? 'readonly'}  last: ${last ? `${last.ts.slice(0, 16)} ${last.ok ? 'ok' : 'FAILED'}` : 'never'}`);
      console.log(dim(`  ${t.prompt.slice(0, 110)}`));
    }
    return console.log(dim(`\ndaemon: tango task run --loop · run once now: tango task run`));
  }
  if (sub === 'add') {
    const [name, every, ...prompt] = a;
    if (!name || !every || !prompt.length) throw new Error('usage: tango task add <name> <every:30m|2h> <prompt...>');
    let mode: PermissionMode = 'readonly';
    const mi = prompt.indexOf('--mode');
    if (mi >= 0) {
      const v = prompt[mi + 1];
      if (v !== 'auto' && v !== 'readonly') throw new Error('--mode must be auto or readonly (headless tasks never ask)');
      mode = v;
      prompt.splice(mi, 2);
    }
    await addTask({ name, prompt: prompt.join(' '), everyMinutes: parseInterval(every), cwd: process.cwd(), mode });
    return console.log(dim(`task "${name}" added; starts on the next "tango task run --loop" tick`));
  }
  if (sub === 'rm') {
    if (!a[0]) throw new Error('usage: tango task rm <name>');
    return console.log(dim(`removed "${(await removeTask(a[0])).name}"`));
  }
  if (sub === 'run') {
    const loop = a.includes('--loop');
    const runner = (t: Task) => runTask(t, args);
    if (!loop) return void (await runDueOnce(runner));
    console.log(dim('tango task daemon: ctrl-c to stop'));
    const ac = new AbortController();
    process.on('SIGINT', () => ac.abort());
    process.on('SIGTERM', () => ac.abort());
    await daemonLoop(runner, 1, ac.signal);
    return;
  }
  throw new Error(`unknown task command "${sub}" (list | add | rm | run [--loop])`);
}

async function runTask(t: Task, args: Args): Promise<{ ok: boolean; output: string }> {
  perm.mode = t.mode === 'auto' ? 'auto' : 'readonly'; // headless: nothing can answer "ask"
  const { agent } = await openAgent({ ...args, input: undefined, resume: undefined, continue: false, cwd: t.cwd ?? args.cwd, yes: t.mode === 'auto' });
  const msg = await agent.prompt(t.prompt, () => {});
  const output = summarizeOutput(textOf(msg) || `[no text; stopReason=${msg.stopReason}]`);
  const hook = await feishuWebhook();
  if (hook) {
    try {
      await sendFeishu(hook, `tango · ${t.name}`, output);
    } catch (e: any) {
      console.error(red(`feishu: ${e.message}`));
    }
  }
  return { ok: msg.stopReason !== 'error' && msg.stopReason !== 'aborted', output };
}

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2));
  if (args.task) return taskMain(args, args.task);
  if (args.version) return console.log(VERSION);
  if (args.help) return console.log(HELP);

  if (args.listModels) return printModels(await loadModels(), (await loadConfig()).model);
  if (args.listSessions) return printSessions(await SessionStore.list(sessionsDir()));

  if (args.yes && args.readonly) throw new Error('--yes and --readonly cannot be combined');
  perm.mode = args.yes ? 'auto' : args.readonly ? 'readonly' : 'ask';
  const { agent, store } = await openAgent(args);
  if (args.input) return runOnce(agent, args.input);
  await interactive(agent, store);
}

main().catch((e) => {
  console.error(red(String(e?.message ?? e)));
  process.exit(1);
});
