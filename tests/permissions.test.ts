import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { decide, installPermissionGate, parseRule, suggestRules } from '../src/core/permissions.js';
import { EventBus } from '../src/core/events.js';
import { findTool, grepTool, lsTool } from '../src/core/tools/search.js';
import type { ToolCallBlock } from '../src/core/types.js';

const call = (name: string, input: object): ToolCallBlock => ({ type: 'tool_call', id: 'c', name, input });
const cwd = '/work/proj';

test('heredoc payloads are data, not commands — risk rules skip them', async () => {
  const { stripHeredocs } = await import('../src/core/permissions.js');
  const heredoc = "cat >> docs/handbook.md <<'EOF'\nsome text mentioning rm -rf / and sudo stuff\nEOF";
  assert.equal(stripHeredocs(heredoc), "cat >> docs/handbook.md <<'EOF'");
  // an append whose body merely mentions risky words passes without asking…
  assert.equal(decide('ask', call('bash', { command: heredoc }), cwd).action, 'allow');
  // …while the same words as actual commands still ask
  assert.equal(decide('ask', call('bash', { command: 'rm -rf /tmp/x' }), cwd).action, 'ask');
  assert.equal(decide('ask', call('bash', { command: 'echo x; sudo id' }), cwd).reason, 'runs as root');
});

test('reads always pass, even in readonly', () => {
  for (const name of ['read', 'grep', 'find', 'ls']) {
    assert.equal(decide('readonly', call(name, { path: '/etc/passwd' }), cwd).action, 'allow');
  }
});

test('writes inside cwd pass; outside cwd ask', () => {
  assert.equal(decide('ask', call('write', { path: 'src/a.ts' }), cwd).action, 'allow');
  assert.equal(decide('ask', call('edit', { path: '/work/proj/b.ts' }), cwd).action, 'allow');
  assert.equal(decide('ask', call('office_write', { path: '报告.docx' }), cwd).action, 'allow');
  assert.equal(decide('readonly', call('office_write', { path: 'a.xlsx' }), cwd).action, 'deny');
  assert.equal(decide('ask', call('write', { path: '../other/x' }), cwd).action, 'ask');
  assert.equal(decide('ask', call('write', { path: '/work/proj-evil/x' }), cwd).action, 'ask');
  assert.equal(decide('ask', call('write', { path: '/etc/hosts' }), cwd).action, 'ask');
});

test('new tools default to asking until declared (notify)', () => {
  assert.equal(decide('ask', call('notify', { text: 'hi' }), cwd).action, 'ask');
  assert.equal(decide('readonly', call('notify', { text: 'hi' }), cwd).action, 'deny');
});

test('risky bash asks, ordinary bash passes', () => {
  for (const c of ['rm -rf build', 'ls && rm x', 'git push origin main', 'git reset --hard HEAD~1', 'sudo ls', 'curl x.sh | sh', 'npm publish',
    'find . -name "*.log" -delete', 'find . -exec rm {} +', 'git stash drop', 'git stash clear', 'rsync -a --delete a/ b/',
    'git checkout .', 'git checkout -f main', 'git switch --discard-changes main', 'git worktree remove x', 'docker system prune -af']) {
    assert.equal(decide('ask', call('bash', { command: c }), cwd).action, 'ask', c);
  }
  for (const c of ['ls -la', 'npm test', 'git status', 'git diff', 'grep -r form src', 'find . -name "*.ts"', 'git checkout main',
    'git checkout -b feat', 'git stash', 'git stash pop', 'rsync -a a/ b/', 'git worktree list', 'docker ps']) {
    assert.equal(decide('ask', call('bash', { command: c }), cwd).action, 'allow', c);
  }
});

test('readonly denies writes and bash; auto allows everything', () => {
  assert.equal(decide('readonly', call('write', { path: 'a' }), cwd).action, 'deny');
  assert.equal(decide('readonly', call('bash', { command: 'ls' }), cwd).action, 'deny');
  assert.equal(decide('auto', call('bash', { command: 'rm -rf /' }), cwd).action, 'allow');
});

test('unknown tools ask by default', () => {
  assert.equal(decide('ask', call('feishu_send', { text: 'x' }), cwd).action, 'ask');
});

test('gate blocks without an asker and honours the answer with one', async () => {
  const risky = { call: call('bash', { command: 'rm -rf x' }), tool: {} as any };

  const silent = new EventBus();
  installPermissionGate(silent, { mode: () => 'ask', cwd });
  const v1 = await silent.emit('before_tool', risky);
  assert.ok(v1 && v1.block && /--yes/.test(v1.reason));

  const no = new EventBus();
  installPermissionGate(no, { mode: () => 'ask', cwd, ask: async () => false });
  const v2 = await no.emit('before_tool', risky);
  assert.ok(v2 && v2.block && /declined/.test(v2.reason));

  const yes = new EventBus();
  installPermissionGate(yes, { mode: () => 'ask', cwd, ask: async () => true });
  assert.equal(await yes.emit('before_tool', risky), undefined);
});

test('the gate asks its refresh before deciding, so rules can follow the files', async () => {
  const rules = { allow: [] as string[] };
  let checks = 0;
  const bus = new EventBus();
  installPermissionGate(bus, {
    mode: () => 'ask',
    cwd,
    rules: () => rules.allow.map((r) => parseRule(r)).filter((x) => x.ok).map((x) => (x as { ok: true; rule: never }).rule),
    // the second check on: a rule appears "on disk"
    refresh: () => void (checks++ === 1 && rules.allow.push('bash:git push'))
  });
  const push = { call: call('bash', { command: 'git push origin main' }), tool: {} as any };
  assert.ok(await bus.emit('before_tool', push), 'no rule yet: the call is stopped');
  assert.equal(await bus.emit('before_tool', push), undefined, 'the rule that appeared is applied, no restart');
});

test('`2>&1` is a redirect, not a command separator — and a risky tail after it still asks', () => {
  const both = suggestRules(call('bash', { command: 'git push origin main 2>&1 | tail -6' }));
  assert.deepEqual(both, ['bash:git push', 'bash:tail'], 'no stray `bash:1`');
  const echo = [parseRule('bash:echo')].filter((x) => x.ok).map((x) => (x as { ok: true; rule: never }).rule);
  const d = decide('ask', call('bash', { command: 'echo hi 2>&1 && rm -rf build' }), cwd, echo);
  assert.equal(d.action, 'ask', 'the rm after the redirect is still seen');
  assert.match(String(d.reason), /rm -rf build|deletes files/);
});

test('grep / find / ls work on a temp tree', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'tango-search-'));
  try {
    await mkdir(join(dir, 'src'));
    await writeFile(join(dir, 'src', 'a.ts'), 'export const needle = 1;\n');
    await writeFile(join(dir, 'b.md'), 'no match here\n');
    const ctx = { cwd: dir, signal: new AbortController().signal };

    const g = await grepTool.execute({ pattern: 'needle' }, ctx);
    assert.match((g.content[0] as any).text, /a\.ts:1:export const needle/);
    const none = await grepTool.execute({ pattern: 'zzz_absent' }, ctx);
    assert.equal((none.content[0] as any).text, 'no matches');

    const f = await findTool.execute({ pattern: '*.ts' }, ctx);
    assert.match((f.content[0] as any).text, /a\.ts/);
    assert.doesNotMatch((f.content[0] as any).text, /b\.md/);

    const l = await lsTool.execute({}, ctx);
    assert.match((l.content[0] as any).text, /^b\.md {2}\d+B\nsrc\/$/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});

test('allow rules: tool/path/bash matching with boundary safety, readonly wins', async () => {
  const { parseRule, decide } = await import('../src/core/permissions.js');
  const cwd = '/w';
  const call = (name: string, input: unknown) => ({ type: 'tool_call' as const, id: 'x', name, input });
  const rules = ['tool:diff', 'path:src', 'bash:git status'].map(parseRule).map((r: any) => r.rule);

  // tool rule: exact name
  assert.equal(decide('ask', call('diff', {}), cwd, rules).action, 'allow');
  assert.notEqual(decide('ask', call('write', { path: '/elsewhere/x' }), cwd, rules).action, 'allow');

  // path rule: segment boundary, and rules CAN allow outside-cwd subtrees
  const rules2 = ['tool:diff', 'path:src', 'path:../sib'].map(parseRule).map((r: any) => r.rule);
  assert.equal(decide('ask', call('write', { path: 'src/a.ts' }), cwd, rules2).action, 'allow');
  assert.equal(decide('ask', call('write', { path: '../sib/x' }), cwd, rules2).action, 'allow', 'outside-cwd rule target allowed');
  assert.equal(decide('ask', call('write', { path: '../sib-evil/x' }), cwd, rules2).action, 'ask', 'segment boundary: sib does not match sib-evil');
  assert.equal(decide('ask', call('write', { path: '../outside/x' }), cwd, rules2).action, 'ask');

  // bash rule: token boundary against commands that would otherwise ASK (risky ones)
  const rules3 = [...rules, { kind: 'bash', value: 'rm' }];
  assert.equal(decide('ask', call('bash', { command: 'rm -rf build/' }), cwd, rules3).action, 'allow', 'rule beats risky-ask');
  assert.equal(decide('ask', call('bash', { command: 'rm -rf /' }), cwd, rules).action, 'ask', 'risky still asks without a rule');
  assert.equal(decide('ask', call('bash', { command: 'chmod +x x' }), cwd, rules3).action, 'ask', 'other risky commands still ask');

  // readonly always wins over rules
  assert.equal(decide('readonly', call('write', { path: 'src/a.ts' }), cwd, rules).action, 'deny', 'readonly wins over path rules');
  assert.equal(decide('readonly', call('bash', { command: 'rm -rf build/' }), cwd, rules3).action, 'deny', 'readonly wins over bash rules');

  // invalid rules are rejected with a reason
  assert.ok(parseRule('nonsense').error);
  assert.ok(parseRule('bash:').error);
  assert.ok(parseRule('path:/absolute').ok === true || parseRule('path:/absolute').error); // absolute allowed but flagged
});

test('bash rule tail re-check: a matching prefix cannot smuggle risky tails', async () => {
  const { parseRule, decide } = await import('../src/core/permissions.js');
  const rules = ['bash:git status'].map(parseRule).map((r: any) => r.rule);
  const call = (command: string) => ({ type: 'tool_call' as const, id: 'x', name: 'bash', input: { command } });
  assert.equal(decide('ask', call('git status -s'), '/w', rules).action, 'allow', 'plain match allowed');
  // the tail contains a risky command — must ask despite the rule
  assert.equal(decide('ask', call('git status; rm -rf /'), '/w', rules).action, 'ask', 'risky tail re-checked');
  assert.equal(decide('ask', call('git status && git push'), '/w', rules).action, 'ask', 'push tail re-checked');
  // a fully covered compound is fine
  const rules2 = ['bash:git status', 'bash:git push'].map(parseRule).map((r: any) => r.rule);
  assert.equal(decide('ask', call('git status && git push'), '/w', rules2).action, 'allow', 'both segments covered');
});

test('redirecting to /dev/null is not a system-path write (false positive fix)', async () => {
  const { decide } = await import('../src/core/permissions.js');
  const call = (command: string) => ({ type: 'tool_call' as const, id: 'x', name: 'bash', input: { command } });
  // the classic benign idiom must pass without asking
  assert.notEqual(decide('ask', call('npm test > /dev/null 2>&1'), '/w').action, 'ask');
  assert.notEqual(decide('ask', call('grep foo bar.txt 2>/dev/null'), '/w').action, 'ask');
  // real system paths and raw devices still ask
  assert.equal(decide('ask', call('echo x > /etc/passwd'), '/w').action, 'ask');
  assert.equal(decide('ask', call('cat disk.img > /dev/sda'), '/w').action, 'ask');
  assert.equal(decide('ask', call('dd if=a of=/dev/disk2 bs=1m'), '/w').action, 'ask');
});

test('suggestRule derives the rule "always allow" remembers for a call', async () => {
  const { suggestRule } = await import('../src/core/permissions.js');
  const bash = (command: string) => ({ type: 'tool_call' as const, id: 'x', name: 'bash', input: { command } });
  assert.equal(suggestRule(bash('git branch -D backup-pre-split')), 'bash:git branch');
  assert.equal(suggestRule(bash('npm publish --access public')), 'bash:npm publish');
  assert.equal(suggestRule(bash('rm -rf /tmp/x')), 'bash:rm');
  // the LEADING segment only (suggestRules has the rest) — the rule never covers a piped tail
  assert.equal(suggestRule(bash('cd /tmp && git branch -D x')), 'bash:cd');
  // a flag right after the tool is not a subcommand
  assert.equal(suggestRule(bash('git -C /x branch -D y')), 'bash:git');
  assert.equal(suggestRule(bash('')), null);
  const write = (path: string) => ({ type: 'tool_call' as const, id: 'x', name: 'write', input: { path } });
  assert.equal(suggestRule(write('/etc/hosts')), 'path:/etc');
  assert.equal(suggestRule(write('notes.txt')), 'path:.');
  const tool = { type: 'tool_call' as const, id: 'x', name: 'notify', input: {} };
  assert.equal(suggestRule(tool as any), 'tool:notify');
});

test('confirm mode asks before every change: file writes and every bash command', async () => {
  const { decide } = await import('../src/core/permissions.js');
  const call = (name: string, input: object) => ({ type: 'tool_call' as const, id: 'x', name, input });
  // a write inside cwd: ask mode allows it, confirm asks
  const w = call('write', { path: '/w/src/a.ts' });
  assert.deepEqual(decide('ask', w, '/w'), { action: 'allow' });
  assert.equal(decide('confirm', w, '/w').action, 'ask');
  // an innocent command: ask allows, confirm asks
  const b = call('bash', { command: 'ls -la' });
  assert.deepEqual(decide('ask', b, '/w'), { action: 'allow' });
  assert.equal(decide('confirm', b, '/w').action, 'ask');
  // reads still pass, and an explicit allow rule still beats confirm
  assert.deepEqual(decide('confirm', call('read', { path: '/w/a.ts' }), '/w'), { action: 'allow' });
  assert.deepEqual(
    decide('confirm', call('bash', { command: 'git branch' }), '/w', [{ kind: 'bash', value: 'git branch' }]),
    { action: 'allow' }
  );
  // plan mode still vetoes writes whatever the mode says
  // (checked one level up, not in decide itself)
});

test('"always allow" saves a rule per segment, so a command with a risky tail is not asked again', async () => {
  const { suggestRules, suggestRule, decide, parseRule } = await import('../src/core/permissions.js');
  const bash = (command: string) => ({ type: 'tool_call' as const, id: 'x', name: 'bash', input: { command } });
  // the shape from the field: harmless prefix, then cd / an assignment / python3 / rm
  // (the field command had an unbalanced quote and `EOF && rm …` on the terminator line, which
  // bash itself would not end the heredoc on; the shape here is the well-formed equivalent)
  const cmd = `osascript -e 'tell application "Microsoft Word" to close document "协议.docx" saving no'; cd /tmp/ctr && F="/Volumes/WD/协议.docx" && python3 - <<'EOF'\np='/tmp/ctr/t/word/document.xml'\nEOF\nrm -f ../out8.docx && echo done`;
  const call = bash(cmd);
  assert.deepEqual(suggestRules(call), ['bash:osascript', 'bash:cd', 'bash:python3', 'bash:rm', 'bash:echo']);
  const rules = suggestRules(call).map((r) => parseRule(r)).filter((r): r is Extract<typeof r, { ok: true }> => r.ok).map((r) => r.rule);
  // the old behaviour (first rule only) asked again every time: the reason from the field
  const firstOnly = rules.slice(0, 1);
  assert.deepEqual(decide('ask', call, '/tmp', firstOnly).action, 'ask');
  assert.match((decide('ask', call, '/tmp', firstOnly) as any).reason, /rule covers the prefix, but a later command deletes files/);
  // all of them: the same command is allowed next time …
  assert.deepEqual(decide('ask', call, '/tmp', rules), { action: 'allow' });
  // … while a command that was never allowed still asks
  assert.equal(decide('ask', bash('ls; rm -rf ~/Documents'), '/tmp', rules).action, 'ask');
  assert.equal(suggestRule(call), 'bash:osascript');
  // no segment, nothing to remember
  assert.deepEqual(suggestRules(bash(';; &&')), []);
});

test('writes under ~/.tango/worktrees never ask; other outside writes still do', async () => {
  const { makeAgent } = await import('../src/host.js');
  const { SessionStore } = await import('../src/core/session.js');
  const { worktreeRoot } = await import('../src/core/worktrees.js');
  const home = await mkdtemp(join(tmpdir(), 'tango-wt-home-'));
  const work = await mkdtemp(join(tmpdir(), 'tango-wt-work-'));
  const prev = process.env.TANGO_HOME;
  process.env.TANGO_HOME = home;
  try {
    const asked: string[] = [];
    const store = await SessionStore.create(join(home, 'sessions'), work);
    const agent = makeAgent(
      { id: 'fake/m', provider: 'fake', model: 'm', api: 'openai-completions', baseUrl: 'http://127.0.0.1:9/v1', apiKey: 't' },
      store, work, { mode: 'ask', ask: async (c) => (asked.push(String((c.input as any).path)), false) }, { checkpoint: 'off' }
    );
    const tool = { name: 'write', writeEffect: true } as any;
    const inWt = join(worktreeRoot(), 'feat', 'a.ts');
    assert.ok(inWt.startsWith(home), 'root follows TANGO_HOME');
    const v1 = await agent.events.emit('before_tool', { call: call('write', { path: inWt }), tool });
    assert.ok(!v1?.block, 'managed worktree write passes without asking');
    const v2 = await agent.events.emit('before_tool', { call: call('write', { path: join(home, 'other.txt') }), tool });
    assert.ok(v2?.block, 'other outside writes still ask (and are declined here)');
    assert.deepEqual(asked, [join(home, 'other.txt')]);
  } finally {
    if (prev === undefined) delete process.env.TANGO_HOME;
    else process.env.TANGO_HOME = prev;
    await rm(home, { recursive: true, force: true });
    await rm(work, { recursive: true, force: true });
  }
});

test('"always allow" never saves code, quote fragments or shell keywords as rules', async () => {
  const { suggestRules, stripHeredocs, decide } = await import('../src/core/permissions.js');
  const bash = (command: string) => ({ type: 'tool_call' as const, id: 'x', name: 'bash', input: { command } });
  // the field case: a JS heredoc with commands AFTER the terminator
  const js = `cat > /tmp/a.mjs <<'EOF'\nconst o = [];\nconsole.log(await page.evaluate(async () => { for (const oid of [1]) { o.push(oid); } return [].filter(r => r).join("|"); }));\nEOF\nego-browser nodejs < /tmp/a.mjs\ncd ~/p && git add -A && git commit -qm "bili: x; record urls" && git push -q`;
  assert.deepEqual(suggestRules(bash(js)), ['bash:cat', 'bash:ego-browser', 'bash:cd', 'bash:git add', 'bash:git commit', 'bash:git push']);
  assert.equal(stripHeredocs(js).includes('console.log'), false, 'body dropped though commands follow it');
  // <<- with tab-indented terminator, two heredocs on one line, here-strings untouched
  assert.equal(stripHeredocs('cat <<-X\n\trm -rf /\n\tX\nls'), 'cat <<-X\nls');
  assert.equal(stripHeredocs("diff <(cat <<A\na\nA\n) <(cat <<B\nb\nB\n)"), 'diff <(cat <<A\n) <(cat <<B\n)');
  assert.equal(stripHeredocs('cat <<< "x"; rm y'), 'cat <<< "x"; rm y');
  // separators inside quotes are text
  assert.deepEqual(suggestRules(bash(`grep -n "a|b; c" f && sed 's/a;b/c/' g | note "x && y"`)), ['bash:grep', 'bash:sed', 'bash:note']);
  // loops and conditionals: the commands inside, not the grammar
  assert.deepEqual(suggestRules(bash('for n in 1 2; do echo $n; done')), ['bash:echo']);
  assert.deepEqual(suggestRules(bash('if test -f x; then cat x; else ls; fi')), ['bash:test', 'bash:cat', 'bash:ls']);
  assert.deepEqual(suggestRules(bash('(cd /x && make) && { ls; }')), ['bash:cd', 'bash:make', 'bash:ls']);
  // a risky command on a later LINE (not just after ;) is still caught past an allowed prefix
  const r = decide('ask', bash('ls\nrm -rf ~/x'), '/w', [{ kind: 'bash', value: 'ls' }]);
  assert.equal(r.action, 'ask');
  // a heredoc body with rm in it is data, not a risky tail
  assert.deepEqual(decide('ask', bash("cat > n.md <<'EOF'\nthen rm -rf it\nEOF"), '/w', [{ kind: 'bash', value: 'cat' }]), { action: 'allow' });
});
