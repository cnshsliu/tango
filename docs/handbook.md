# tango handbook

How to use tango day to day. For installation, model presets, status and architecture, see
the [README](../README.md).

- [Get started](#get-started)
- [Everyday workflows](#everyday-workflows)
  - [Sessions](#sessions)
  - [Sub-agents the model starts itself](#sub-agents-the-model-starts-itself)
  - [Background work: /bg vs /subagent](#background-work-bg-vs-subagent)
  - [Worktrees](#worktrees)
  - [Time travel](#time-travel)
  - [Permissions](#permissions)
- [Slash-command reference](#slash-command-reference)

## Get started

1. Set one API key, for example `export TKNET_API_KEY=...` or `export DEEPSEEK_API_KEY=...`.
   `tango --models` shows every preset and which variable it reads.
2. Open a project directory and run `tango`. The fullscreen TUI starts a new session there.
3. Type a request and press Enter. Answers stream in, and tool calls appear as status lines
   with their durations.
4. When tango wants to do something risky (delete files, `git push`, write outside the
   project), a dialog asks first. Press `y` to allow, `n` to refuse, or `a` to allow
   everything for the rest of the session.
5. Leave with `/exit`. Nothing else closes the session. Ctrl+C clears a non-empty
   input first; only when the input is empty does it interrupt the current reply.

Run a single prompt without the TUI:

```sh
tango "summarize the open TODOs in this repo"
git diff | tango "review this patch"
```

### Input basics

| You want to | Do this |
| --- | --- |
| Send | Enter (Ctrl+S always sends, also in Vim mode) |
| Add a newline | Alt+Enter while idle; Shift+Enter in terminals with the Kitty keyboard protocol (kitty, WezTerm, Ghostty, iTerm2 with CSI u); or switch Enter's meaning with `/multiline` |
| Include a file | `@src/app.ts`, or a line range: `@src/app.ts#20-45`. Typing a bare name (`@handler`) also lists matching files from anywhere in the git repo, fuzzily (`@srvts` finds `src/serve.ts`); git-ignored files are left out |
| Ask about a file by its path | Paste or drag the full path (`/Users/me/notes.md`, `~/x.md`, quoted or with `\ ` escapes), alone or followed by your question. It goes to the model as `文件：<path>` plus your text; a bare path asks the model to read and understand the file. It is never taken for a slash command; a path that doesn't exist says so |
| Attach an image | Paste it; terminals paste a temp file path, which becomes a `[图片 #N]` chip |
| Run a command | Type `/` and Tab-complete (works while a turn runs too), or open the palette with Ctrl+P |
| Fill in an argument | After `/mode `, `/effort `, `/model `, `/send ` or `/resume `, press Tab: the first match goes in, each further Tab swaps in the next, Shift+Tab goes back (zsh menu-complete). Typed letters filter first; `/model glm54` finds `zhipu-plan/glm-5.4`. Enter runs what is in the input |
| Answer a `?choice` panel | ↑↓/click + Enter, or type the option number + Enter. Tab answers in your own words: the input fills with the ask as a lead-in (`关于…，模型在等你选择: `) in the conversation's language — only what you write after it is sent; the panel's title uses the same language. Esc collapses the panel |
| Interrupt the running turn | Esc or Ctrl+C. If a dialog is open (picker, completion list, `/todo`, `/queue`, a `?choice` panel), the first Esc only closes it and the turn keeps running; press Esc again to interrupt. Ctrl+C interrupts right away |
| Use a dialog with the mouse | Click a key in its bottom hint (`y 允许`, `n 拒绝`, `esc 关闭`, `enter 确认` …): it does what pressing that key does. Every dialog works this way: permission, pickers, viewers, `/queue`, `/todo`, `?choice` |
| Search earlier prompts | Ctrl+R |
| Park a half-written draft | Alt+S (press again to restore) |
| Edit a draft in Neovim | Ctrl+E, or `/nvim` |
| See a full tool output | Ctrl+O, or double-click the tool line: a built-in dialog, `y` copies, Esc closes |
| See what a turn produced at a glance | Each finished turn ends with a muted `✓ 本轮：…` line: the answer's whole first line, wrapped when long, never cut. `/recap` instead summarizes the whole conversation |
| Read the model's full reasoning | Double-click a thinking block to expand / collapse it |
| Copy over SSH or inside tmux | Works by default: over SSH tango copies through the terminal (OSC 52). In tmux set `set -g set-clipboard on`; if it still fails, try `/clipboard osc52` |
| Emoji show as boxes or misalign | `/ascii on` draws them (and the thinking robot) as plain ASCII |
| Screen tears or flickers while streaming | Use a terminal with synchronized output (iTerm2, Kitty, WezTerm, Ghostty, Alacritty, Windows Terminal, recent tmux); tango sends each frame as one update |
| Check whether it's still working | The rule above the input: `idle`, or a `●` with the elapsed time and received characters; a band of light flows along the rule while output arrives, and stops and dims after 10s of silence |
| Find the logo | An empty session shows the centered TANGO wordmark (block letters, cyan gradient + twinkle) with the dancer and a hint. Your first message ends the dancer and hint but keeps the wordmark at the top of the chat, with your first message at the bottom: screenshot-friendly. It scrolls away as the chat grows, is not saved with the session, and is skipped on terminals under 20 rows or 30 columns |
| Get back to the prompt you're reading about | Scrolled up, the dim top row shows that prompt; click it to jump there |
| Hop between your prompts | `alt+↑` / `alt+↓` walk to the previous / next prompt, landing it at the top of the view; the last `alt+↓` returns to the bottom |
| Search the whole transcript | Ctrl+T, then `/` |
| Ask a side question | `/btw <question>`. The answer doesn't enter the session |
| Answer the model's ❓ panel | ↑↓ + Enter, click, or type the option number + Enter. Esc hides it; `/todo` brings it back |

A tool call in flight shows as `⚙ bash …` with the gear color cycling; when it finishes
the same line becomes `⚙ bash <command> · <ms>` (double-click it or press ctrl+o for the output).
A tool line always takes one row: long or multi-line commands (heredocs, loops) show their first
line, cut to the terminal width, with `…（共 N 行）`; the viewer shows the full command above the output. Interrupting a running command
(Esc / Ctrl+C) marks its line `· 已中断` in the warning color instead of a red error. A command
that fails stays one line too, in the warning color, ending `· 出错 · 双击查看`: double-click it
(or Ctrl+O) to read the error output.

Reasoning the model streamed shows as a block headed by an upright `🤖` (the text after it is dim
italic), with a `┃` bar on its left. The whole
text is kept: short reasoning shows in full, a long block collapses to its first 3 lines, the last one ending in
`… 双击展开思考（还有 N 字 · 共 M 行）` (no line of its own). Only the first collapsed block names
the double-click; the rest just end in `…（还有 N 字 · 共 M 行）`. Double-click any block to expand or collapse it; Ctrl+T's transcript always shows thinking in full. While the
turn runs, that bar's color scrolls down the newest thinking block (in 256 colors on
terminals without 24-bit color; once idle, or with `/animations off`, it is a flat dim bar).

While a turn runs, Enter queues your next prompt instead of interrupting. Queued prompts
are sent in order when the turn ends. `/queue` shows them, and its `[并入当前]` button merges
one into the running turn. The dialog works without a mouse too: ↑↓ (or `j`/`k`, `1`–`9`)
picks a row, Enter merges it, `d` deletes it, `a` merges all, `c` clears, Esc closes. Once the
queue is empty the dialog closes on its own. Alt+Enter does the same for what you're typing: the text joins the
running turn, and the model sees it on its next request. Ctrl+C (or Esc) stops the current
reply and keeps the queue.

If you'd rather have every Enter during a turn join it, run `/queue auto append`. From then on
Enter while busy works like Alt+Enter, and nothing queues. `/queue auto off` brings queueing
back, and `/queue auto` shows which mode is on. The setting lasts until you quit Tango; it is
not saved to config.

## Everyday workflows

### Sessions

Every conversation is a session, stored as an append-only JSONL file in
`~/.tango/sessions/`. Sessions are tied to the directory you started them in.

| Task | Command |
| --- | --- |
| Continue the latest session | `tango -c` |
| Open a specific session | `tango -r <id or prefix>` (the id's last `-` part also works, e.g. `tango -r 0ad2040a8182`), or `/resume` inside the TUI for a picker. If the session's directory no longer exists, it runs in the directory you start tango from, for that run, and says so |
| Start fresh, same model and mode | `/new` |
| Go back to the previous session (e.g. from a subagent's session to its parent) | `/back`; `/forward` undoes it |
| Name the session | `/rename <name>` |
| Search all past conversations | `/find <text>` |
| Ask the model about an earlier chat | Just ask ("what did we decide last time about X?"). It searches past sessions itself |
| Export a transcript | `/export md` or `/export html` (written to `~/.tango/exports/`) |

Long sessions fill the context window. The status bar shows usage and turns yellow at 60%
and red at 80%. Tango compacts older history automatically at 75% (change it with
`/autocompact`), or you can run `/compact` yourself. `/context` shows what takes up the space.
Compaction keeps your most recent turns word for word (about 20 000 tokens' worth) and turns only
the older part into a structured summary: goal, constraints, progress, key decisions, next steps
and critical details such as paths and error messages. Compacting again updates that summary, so
early decisions are not lost after several compactions.

To move work between sessions, use `/handoff <session>` to send a brief of this session to
another one, or `/takeover <session>` to pull another session's work into this one. Neither
command rewrites the other session's history.

For a quick word instead of a full brief, `/send <session> <消息>` messages the other session's
user: it appears in that session's chat while it is idle (within ~2s; a running turn finishes
first) and is part of the model's context from its next prompt; several messages arrive in the
order they were sent. Mail is delivery-only — receiving
never starts a turn, so two sessions cannot reply-loop on their own — and messages expire after
7 days. Remember that a received message does reach the other session's model, i.e. it can
influence its behavior; permission checks still apply to whatever it then does.

### Sub-agents the model starts itself

For investigation the model can delegate on its own with the `task` tool. You don't type a
command; you see a tool line such as `⚙ task explore · find where tokens are refreshed`. While
the sub-agent works, that line shows its latest step (`⚙ task explore · 第3步 grep 英智`). When
it ends, the sub-agent's report goes back to the model, which carries on from there. If you
interrupt the turn first, the model still gets what the sub-agent had found so far.

- `explore` is read-only. The model sends it to search the codebase, explain how something
  works, review a change or research the web, so that dozens of file reads never fill your
  session's context. It can run in the background: the model keeps working, and the report
  arrives later. A `⑂ 后台 explore … 完成` notice tells you when.
- `general` can edit files and run commands in your current directory. It goes through the
  same permission prompts as the main session. In plan mode and `readonly` mode the model
  can only use read-only sub-agents.

To get a sub-agent, just ask ("use explore to find every caller of X", "review this diff with a
sub-agent"). Each run is saved as its own session. They are hidden from the `/resume` list; the
tool result shows the session id, so `/resume <id>` opens the full run. `/agents subagent
<model>` puts sub-agents on a cheaper model.

Define your own in `config.json` with `subAgents`, for example a read-only reviewer with
fixed rules. `/agents` lists them all. See README "Sub-agents: the task tool" for the fields.

Pick `task` sub-agents for questions and reviews in the current checkout. Pick `/bg` or
`/subagent` when the work should happen in a separate worktree while you keep going. `/tasks`
lists only `/bg` and `/subagent` runs.

### Background work: /bg vs /subagent

Both commands start a separate tango process in its own new git worktree (`bg-<id>`), so the
work never touches your main checkout and your current session stays free. The background
process runs in `auto` permission mode: nobody is there to answer prompts, and the worktree
can be thrown away.

| | `/bg <prompt>` | `/subagent <task>` |
| --- | --- | --- |
| Use it for | A larger job you will check on later | A small, self-contained job whose result you want here |
| Model | `subagent` role (`/bg --long` uses `longContext`) | `subagent` role |
| Result | Its own session. Open it with `/resume`, come back with `/back` | A `⑂ subagent … 完成：…` summary added to **this** session, in violet so it stands apart from the main agent's replies |
| Main session | Not touched | Shows the summary within about 2 seconds of the task ending, also while a turn runs (marked `本回合结束后写入会话`). The summary enters the conversation, so the model sees it, when the turn ends. A session that was restarted or resumed meanwhile still gets it, exactly once |

`/subagent` was called `/subtask` before; the old name still works but is no longer offered.

Both kinds show up in `/tasks` with their status (`running`, `done`, `failed`, `stopped`)
and session id. `/stop <id or name>` kills a running task.

The task's file changes stay in its worktree. To bring them into your checkout, run
`/worktree merge bg-<id>` (see below). Merging only picks up committed changes, so if the
task left uncommitted edits, commit them in the worktree first. Set role models with
`/agents subagent <model>` and `/agents longContext <model>`. Without them, the current
model is used.

### Code navigation

Ask "who calls `searchOutcome`?" or "what type does `run` return?" and the model uses the `lsp`
tool: definitions, references, hover, file outlines, project-wide symbol search, callers and
callees, answered by the project's language server rather than by text search. TypeScript and
JavaScript work in any project with `typescript-language-server` in `node_modules` or on `PATH`;
Python needs `pyright-langserver`, Rust `rust-analyzer`, Go `gopls`, C/C++ `clangd`. tango
doesn't install them on its own. When one is missing, the model tells you and offers the install
command for your machine (`rustup component add rust-analyzer`, `npm install -g pyright` …); say
yes and it runs it through a permission dialog that shows the exact command, then carries on with
the server. Say no and it falls back to grep. `/doctor` shows which servers you have and how to
get the rest. The first question in a project takes a second or two while the server loads it.

### Long-running commands (dev servers, watchers)

A normal `bash` call waits for the command and is killed after 120 seconds. For `npm run dev`,
`cargo watch` or anything else that keeps running, ask the model to start it in the background
("start the dev server in the background"). It calls `bash` with `background: true` and gets an
id like `p1` straight away, so the turn goes on. Later it reads the output with `process_logs`,
stops it with `process_stop` and lists everything with `process_list`. Up to 8 can run at once,
and all of them are killed when tango exits, so nothing keeps running after you quit.

### When the model endpoint fails

A request that fails before the model sends anything (connection reset, DNS failure, timeout, rate
limit 429, gateway 502/503/504) is retried automatically. The chat shows
`⟳ 请求失败（connection reset [ECONNRESET]），1 秒后第 1/10 次重试…` before each try, waiting 1s,
2s, 4s, 8s, then 15s each. Press Esc to stop waiting. After the last retry the error is shown as
before, with "gave up after 10 retries". Set the count with `"retries": 5` in `~/.tango/config.json` (0-10,
default 10; `0` turns it off), or per model with `retries` in `models.json`. A wrong key (401/403)
fails at once, since retrying cannot fix it. A reply that breaks off midway is not retried,
because tango would have to throw away the text it already showed.

Some gateways change what you send. Measured on 2026-11-25: `slb.apikey.fan` (the
`apikeyfan/claude-opus-5-5` model) turns every `<` and `>` in the request into `[` and `]`
before the model sees it, whether sent raw or JSON-escaped; the direct `zhipu-plan/glm-5.4`
endpoint keeps them. On such a gateway the model can't read HTML, generics or shell redirects
correctly in files and tool output. Switch models (`/model`) for that kind of work. To test a
gateway, ask it for the code points of `A<B>C`: the answer should be `65 60 66 62 67`.

### Scripts and CI (one-shot runs)

`tango "<prompt>"` runs one prompt without the TUI and prints the answer. For a script that needs
to read the result, add `--format json`: every stdout line is then a JSON event (start, each model
step, each tool call with its output and duration, token usage and cost, errors) and the last line
is always a `done` summary. `--max-tool-rounds N` caps how many model→tool rounds the run may take,
so a stuck task fails fast instead of using the default 64. One-shot runs cannot answer permission
asks, so risky calls are refused unless you add `-y`. The README's "Headless JSON" section lists
every event field.

### Recurring tasks

Ask in plain words: "每两小时检查一下 CI，有失败就发飞书". The model creates the task with
`schedule_create` (you approve it in `ask` mode); `schedule_list` and `schedule_remove` show and
delete them. These are the same tasks as `tango task list/add/rm`, kept in `~/.tango/tasks.json`.
They only run while `tango task run --loop` (or the launchd agent from the README) is running, and
they run without a TUI: `readonly` by default, `auto` only if you asked for it.

### Worktrees

A managed worktree is a separate checkout of your repo at `~/.tango/worktrees/<name>` on
branch `tango/<name>`. Use one whenever two things might write files at the same time. Tango
assumes a single writer in the main workspace and warns if a second tango session opens in
the same directory. A lock left behind by a session that crashed doesn't count: tango checks
that the pid in it is still the same process. Writing into a managed worktree never asks for permission, even from a
session whose cwd is the main checkout. A checkout anywhere else outside the cwd still asks,
unless you `/add_dir` it.

| Command | Effect |
| --- | --- |
| `/worktree` | List managed worktrees with their uncommitted-change counts |
| `/worktree new <name>` | Create one (names: letters, digits, `.`, `_`, `-`, up to 40 chars) |
| `/worktree merge <name>` | `git merge` its branch into your current branch |
| `/worktree rm <name>` | Remove it. Refused while it has uncommitted changes, or while a live session is working in it — the error names that session; `/cd` it to another directory (or close it) first |

Never delete the directory a session is working in, by hand either. A session that is open
there is stuck until it `/cd`s to a directory that exists. A session you resume afterwards runs
in the directory you started tango from, with a chat line saying so (`/cd` to pick another). A
terminal still sitting in the deleted directory starts tango without a working directory — it
then runs in the home directory for that launch and says so.

A subcommand without a name (`/worktree new`) prints its usage line instead of falling back to
the list. Registrations can go stale if a worktree is deleted outside tango — `rm` cleans the
entry (and `/worktree` then shows none).

Merging requires a clean main workspace, so commit or stash first. If the merge conflicts,
tango runs `git merge --abort` and your workspace is left as it was.

To work in a worktree yourself, `/cd ~/.tango/worktrees/<name>` moves the session there.

### Time travel

Tango snapshots your files before the first write of each turn and again at its end. The
snapshots use git objects outside your branch and index, so `git status` and your history
are never affected. In a directory without git, tar snapshots under `~/.tango/snapshots/`
are used instead.

| Command | Effect |
| --- | --- |
| `/undo` | Go back one turn: code, conversation and model context together |
| `/redo` | Go forward again |
| `/rewind` | Pick any turn from a timeline and roll back to just before it |
| `/fork [name]` | Copy the session up to here into a new session. The original stays as it is |
| `/branch` | Keep history up to a turn and drop the rest, in place |
| `/tree` | Show how sessions were forked from each other |
| `/transcript` | Raw transcript, including abandoned futures (marked `⟲`) |

Nothing is lost when you go back. Abandoned turns stay in the session log, `/redo` brings
them back, and `/find` searches them too.

Rolling back deletes files that tango's turns created after the target point. Files that no
snapshot knows about (ones you made by hand) are reported and kept, and `.gitignore`d files
are never touched. Time travel is refused while a turn is running. Turn it off with
`"checkpoint": "off"` in `~/.tango/config.json`.

### Permissions

Every tool call passes a permission gate. There are four modes; the TUI shows each with its
Chinese name (`plan`, below, is 只出计划):

| Mode | Start with | Behaviour |
| --- | --- | --- |
| `confirm` 每步确认 | `/mode confirm` | Ask before every change: file writes and every shell command ask first |
| `ask` 风险确认 | default | Reads and writes inside the project run. Deletes, `git push`, `git reset --hard`, `sudo`, `curl … \| sh`, writes outside the project and unknown tools ask first |
| `auto` 完全放行 | `tango -y` | Everything runs |
| `readonly` 只读 | `tango --readonly` | Reads only. Writes and shell are refused |

Switch at any time with `/mode <mode>` — bare `/mode` opens a picker listing all modes with a one-line description each (plan included). Switching to `auto` (完全放行) first shows an acknowledgement: what it allows (any file path, any shell command, network), a warning that mistakes may be irreversible, and a `☐ 我已了解风险，并对自己的数据安全负责` checkbox — press Space to check it, then Enter to enable; Esc cancels.
With the mouse, click the checkbox row and then `启用`; `启用` does nothing while the box is unchecked. Answering `a` (始终允许) in a permission
dialog saves persistent allow rules for that call and allows it — `bash:git branch` for
`git branch -D x`, the written file's directory for an outside write, `tool:<name>` otherwise —
so the same thing never asks again, in this session or the next. A command line with several
parts (`osascript … && rm -f x`) saves one rule per command, so the line is covered as a whole.
Only real commands count: text inside quotes, loop and `if` keywords, and the body of a
`cat > f <<'EOF'` heredoc are skipped;
the hint under the dialog says what will be remembered (`a 始终允许（记住 osascript · rm）`).
That also allows each of those commands anywhere, so for a one-off answer `y`. `/permissions`
lists and removes the rules it created.

`/plan` is a stricter, temporary mode for research: all writes are refused and the model
writes a plan instead. Run `/plan` again to leave it and carry the plan out. In the TUI,
Shift+Tab switches modes on the spot, one step per press: ask → plan → confirm → readonly →
ask. From ask, one press turns plan on; the next one leaves plan for confirm. Clicking the
footer's `shift+tab mode` hint opens the mode picker instead, so you can pick any mode. The input block's left bar and the `mode · model` row under the
draft turn orange while plan is on (ask is blue, auto red, readonly gray). Shift+Tab never switches
to `auto` (from auto the next press is plan); use `/mode auto` for that.

To stop being asked about something specific without going fully `auto`, add an allow rule:

```
/permissions add tool:web_fetch       always allow one tool
/permissions add path:src             allow writes under src/ (not src-old/)
/permissions add bash:git status      allow commands starting with "git status"
/permissions                          list rules · /permissions rm <rule> removes one
```

Rules are saved in the project's personal `.tango/config.local.json` (outside a project: in
`~/.tango/config.json`); `/permissions` labels each rule with its file. They apply at once, everywhere they are
in force: an `a` pressed in one window counts in every open session from its next decision on,
and after `/cd` the rules of the directory you are in now apply (the ones you left stop). They
only turn "ask" into "allow" and never
loosen `readonly` or `/plan`. A bash rule covers only the part of a command line it
matches, so `git status; rm -rf build` still asks. `/import claude-code|grok|codex` copies
another agent's allow list into these rules and lists any entries it couldn't convert.
It reads `.claude/settings.json` in the project, `~/.grok/settings.json`, or
`~/.codex/config.toml` (pass a different path after `codex`).

`/add_dir <dir>` makes a directory outside the project writable without asking.

### Secrets in tool output

If a command prints one of your API keys or the Feishu webhook (say, `cat ~/.tango/models.json`),
tango replaces it with `[REDACTED <name>]` before the output reaches the screen, the session file
or the model. It knows the values from `~/.tango/env`, the config and `models.json`, and from
environment variables with names like `*_API_KEY` or `*_TOKEN`. To use a credential, the model has
the shell read it instead of seeing it. `/doctor` shows whether it is on and how many values it
watches; `"redactSecrets": false` turns it off. It matches only whole values of 16 characters or
more, and does not rewrite session files that already exist.

### Hooks

Hooks run your own scripts at fixed points and can say no, for checks that a permission rule
can't express. Add them under `hooks` in `~/.tango/config.json`:

```json
{ "hooks": {
  "PreToolUse": [{ "matcher": "bash", "command": "./guard.sh" }],
  "Stop":       [{ "command": "npm test --silent >/dev/null 2>&1 || { echo 'tests fail' >&2; exit 2; }" }]
} }
```

The script reads the event as JSON on stdin. Exiting with 2 refuses a tool call
(`PreToolUse`), tells the model something after a tool ran (`PostToolUse`), rejects your
prompt (`UserPromptSubmit`), or sends the model back to work instead of finishing (`Stop`).
Whatever the script prints to stderr is the reason the model sees. A `Stop` hook gets
`"stop_hook_active": true` once it has already sent the model back, so it can let it finish. tango stops honoring it after 3 sends per prompt.

More events cover the rest of a session: `SessionStart` / `SessionEnd`, `PreCompact` (exit 2
skips the compaction) / `PostCompact`, `PostToolUseFailure` (only failed calls), `StopFailure`
(the turn died on an error), `InstructionsLoaded` (AGENTS.md read), `CwdChanged` (`/cd`),
`Notification` (a permission prompt is waiting: good for a desktop ping), and `SubagentStart` /
`SubagentStop` / `TaskCreated` / `TaskCompleted` around `/subagent` and `/bg` (exit 2 on the start
events refuses the launch). README "Hooks" has the full table. Instead of an exit code, a
script can print JSON on stdout: `{"additionalContext": "…"}` hands the model extra text,
`{"decision": "block", "reason": "…"}` works like exit 2, and `{"continue": false, "stopReason": "…"}`
ends the turn. For example, a `SessionStart` hook that prints
`{"additionalContext": "this repo uses pnpm"}` saves you saying it every session.
Details and payload fields: README, "Hooks".

### Project config

A repo can carry its own `.tango/config.json` (committed), and you can keep a personal
`.tango/config.local.json` next to it that git ignores. Both sit on top of
`~/.tango/config.json`: single values come from the nearest file, rule and hook lists add up. The
global file is created readable by you only (mode 600), because it can hold the Feishu webhook.
`/config` shows every file and the merged result.

Instructions work the same way. tango reads `AGENTS.md` from the current directory up to the
filesystem root, plus `~/.tango/AGENTS.md`, and gives the model all of them (global first, nearest
last). To change a shared `AGENTS.md` just for yourself, put an `AGENTS.override.md` next to it:
in that directory tango reads the override instead, and the other directories are unaffected.

Project hooks, permission rules, MCP servers and extra dirs can run commands or loosen access.
The first time you open such a repo, tango lists them and asks whether to trust the project.
Until you say yes they stay off; everything else in the file applies right away. The answer is
remembered per repo (worktrees included). `/trust` shows the state and asks again; `/trust yes`
or `/trust no` changes it and restarts the session's backend.

## Slash-command reference

Arguments in `<angle brackets>` are required, `[square brackets]` optional. Commands work
while a turn is running unless noted. `/help` prints a short list inside the TUI.

### Sessions and navigation

| Command | What it does |
| --- | --- |
| `/new` | Start a new session with the same model and permission mode |
| `/resume` (`/sessions`) | Open the session picker and switch to the one you pick; `/resume <id or prefix>` switches directly (Tab completes the id; the id's last `-` part, e.g. `0ad2040a8182`, works too). An id that matches no session, or a prefix that matches several, prints an error and keeps the current session |
| `/back` | Return to the session you were in before the last switch (`/resume`, a picker pick, `/new`, `/fork`, or a step of `/forward`), like a browser's back button. After `/resume`-ing into a subagent's session, `/back` takes you to the parent. The welcome line names the way back (`← /back 返回会话 …`) |
| `/forward` | Undo a `/back`. A new switch clears the forward history, as in a browser. The history lasts until the TUI quits or `/restart`s |
| `/rename [name]` (`/name`) | Show or set the session name. In the picker, Ctrl+R renames any session |
| `/editlast` (Alt+E) | Interrupt the running turn, roll the conversation **and code** back to before your last prompt, and load that prompt verbatim into the input box for editing. Two presses: the first arms (rollback touches code), the second executes; Esc cancels |
| `/find [text]` | Search all sessions, including abandoned turns, and jump to a hit |
| `/send [session] [消息]` | Send a short message to another session's user: it shows in that session's chat (within ~2s when idle, or on its next open) and joins the model's context from its next prompt. Bare: pick the session, then type the message. Reply with `/send <from>` (the sender id is in the notice). No auto-reply — a message never starts a turn by itself | 
| `/handoff [session] [note]` | Send a brief of this session's work to another session. Bare: pick one |
| `/handoff list` | Show queued briefs with their queue ids. Briefs not merged within 7 days expire |
| `/handoff cancel <queue id>` | Withdraw a queued brief |
| `/takeover [session] [note]` | Pull a brief of another session's work into this one |
| `/export [md\|html]` | Write the full transcript to `~/.tango/exports/` |
| `/transcript` | View the raw transcript, including abandoned futures |
| `/copy [N]` | Copy the last (or Nth-last) assistant reply to the clipboard. A copy is confirmed by a toast in the chat's top-right corner that goes away after 2 seconds; it never adds a chat line |
| `/recap` | One-line summary of the conversation. Not saved to the session |
| `/restart` | Restart the TUI and serve (picks up a new build) and reopen this session. Not during a turn |
| `/exit` (`/quit`) | Leave tango |

### Time travel

| Command | What it does |
| --- | --- |
| `/undo` / `/redo` | Step one turn back or forward (code + conversation). Not during a turn |
| `/rewind` | Timeline picker, newest turn on top. Roll back to just before the chosen turn |
| `/fork [name]` | Copy the session up to now (or the turn picked in `/rewind`) into a new session |
| `/branch` | Keep history up to a turn and drop the rest in place. `/redo` can recover the rest |
| `/tree` | Show the fork tree of sessions |

### Models and modes

| Command | What it does |
| --- | --- |
| `/model [id]` | Switch model and save it as the default. Bare: open the model picker (its filter is fuzzy too); clicking the model name in the input box's info row opens the same picker, the current model selected (click it again to close; click a row to pick). The id matches fuzzily, case-insensitive: `glm54` → `zhipu-plan/glm-5.4`; with several words each must match (`zp glm5.4`); several matches list the candidates instead of guessing. `-m` on the command line uses the same matching |
| `/agents [role model]` | Show or set role models: `driver`, `subagent` (`/bg`, `/subagent`, `task` sub-agents), `longContext` (`/bg --long`), `plan` (plan mode thinks with it, then switches back); also lists the `task` sub-agents |
| `/mode [confirm\|ask\|auto\|readonly]` | Show or set the permission mode. Bare: a picker with all modes (clicking the mode label in the input box's info row opens it too, the current mode selected; click it again to close, or click the model name to swap to the model picker); switching to auto asks for the risk acknowledgement first |
| `/auto` | Short for `/mode auto` (完全放行): opens the same risk acknowledgement. Its checkbox row and `启用` button are clickable |
| `/plan` | Toggle plan mode: writes refused, the model produces a plan |
| `/effort [档位\|off]` | Show or set how hard the model thinks, for this session. The levels are the model's own: `low high max` on GLM-5.4, `low medium high xhigh max` on Claude Opus 5.5, `none low medium high xhigh max` on GPT-6.1 Terra, none on a switch-only model. Tab lists what the current one takes; bare: the level in use and the choices. `off` sends nothing |
| `/goal <text>` | Keep working toward a goal. After each turn tango checks whether it's done and continues if not |
| `/goal off\|pause\|resume` · `/goal max N` | Clear, pause or resume the goal · set the round cap (1–50, default 5). Bare `/goal` shows status |
| `/loop <interval> <prompt>` | Repeat a prompt on an interval, e.g. `/loop 10m check the build` (minimum 10s) |
| `/loop off` | Stop the loop (Ctrl+C also stops it) |

### Background work and isolation

| Command | What it does |
| --- | --- |
| `/bg <prompt>` | Run a prompt in a new worktree as a detached background session |
| `/bg --long <prompt>` | Same, on the `longContext` role model |
| `/subagent <task>` (`/subtask`) | Run a small task in a new worktree. Its summary is written back into this session |
| `/tasks` | List background tasks with status and session id |
| `/stop <id or name>` | Kill a running background task |
| `/worktree` | List managed worktrees |
| `/worktree new <name>` · `rm <name>` · `merge <name>` | Create, remove (refused if dirty) or merge one into the current branch |
| `/cd <dir>` | Move the session's working directory. Tools and snapshots follow. Bare `/cd` lists directories you have used and lets you type a `/path` or `~/path`. A turn does not start when that directory is gone; the same picker opens |
| `/pwd` | Show the working directory |
| `/add_dir <dir>` | Add a writable directory outside the project (saved) |

### Review and side questions

| Command | What it does |
| --- | --- |
| `/review [paths]` | Code review of the working-tree diff |
| `/security-review` | Security review of the working-tree diff |
| `/commit-push` | Ask the model to commit this piece of work and push the current branch. The commit message follows the repository's existing commits (language and style). `git push` still goes through the permission dialog in ask mode |
| `/commit-pr` | The same commit, then a pull request opened with `gh pr create`; the URL comes back in the reply. On the default branch it creates a feature branch first |
| `/diff [--staged] [paths]` | Read-only view of the working-tree (or staged) patch |
| `/btw <question>` | Ask about the current context without tools. The answer isn't saved |
| `/todo [内容]` | `/todo 买菜` adds a note and opens the list on it. The list holds your notes and the model's pending choices (❓) and suggestions, and you act on them one by one: click a row's buttons, or ↑↓/`j` `k`/1-9 to pick a row, Enter to act on it (完成 ticks a note or unticks it, 展开 a choice, 执行 a suggestion, 答复 a question), `d` to drop it, Esc/`q` to close. Notes come first, open ones before ticked ones, and are saved per directory in `~/.tango/todos.json` |

### Context, memory and skills

| Command | What it does |
| --- | --- |
| `/context` | Token breakdown of the next request: system prompt, skills, tool schemas, history |
| `/compact` | Summarize older history to free up context |
| `/autocompact [N%\|0.N\|off]` | Show or set the auto-compaction threshold (default 75%, saved) |
| `/remember <text>` | Add a line to the global memory `~/.tango/AGENTS.md`, used by every session |
| `/memory` | View the global memory. Ctrl+E edits it |
| `/init [--force]` | Create a project `AGENTS.md` scaffold (`--force` appends to an existing one) |
| `/skills` | List global and project skills (`~/.tango/skills`, `.tango/skills`, `.agents/skills`) |
| `/skill [title]` (`/save-as-skill`) | Turn this session into a skill draft in `~/.tango/skill-drafts/` |
| `/skill finish <file>` | Install an edited draft as a project skill |
| `/preset save <name>` | Save the current input as a reusable prompt template |
| `/preset [name]` | Fill the input with a saved template (bare: open the picker). Doesn't send |

### Usage and diagnostics

| Command | What it does |
| --- | --- |
| `/status` | Session info: id, model, mode, cache hit rate |
| `/cost` | This session's cost and message count |
| `/usage` (`/stats`) | Requests and cost across all sessions, by model and by session |
| `/insights` | Write the same data as an offline HTML report to `~/.tango/exports/` |
| `/queue` | Manage queued prompts: `rm <n>`, `all` (merge all into the running turn), `clear`. `auto append` makes Enter during a turn join the running turn like Alt+Enter; `auto off` goes back to queueing; bare `auto` shows which is on. Lasts until you quit |
| `/interrupt` | Stop the current turn (same as Ctrl+C) |
| `/mcp [add <名字>]` | MCP servers and the tools they provide, each with its address or the reason it is down. `/mcp add context7` adds a well-known remote server (Context7, DeepWiki, GitHub, Sentry) to `~/.tango/config.json`; GitHub and Sentry also need a token in `GITHUB_PAT` / `SENTRY_ACCESS_TOKEN`. `/restart` connects it |
| `/plugins` | Installed local plugins |
| `/permissions [add\|rm <rule>]` | List, add or remove allow rules |
| `/import claude-code\|grok\|codex [path]` | Copy another agent's allow list into tango's rules |
| `/config` | View the merged config and the files it came from |
| `/trust [yes\|no]` | Whether this project's hooks, rules and MCP servers apply. Bare: show state and ask |
| `/doctor` | Check the environment: git, API keys, directories, versions |
| `/debug-zip` (`/feedback`) | Write a diagnostics zip (webhook redacted) to the working directory |

### Input and display

| Command | What it does |
| --- | --- |
| `/multiline` | Toggle whether Enter sends or adds a newline (Ctrl+S always sends) |
| `/vim [on\|off]` | Vim-style editing in the input box |
| `/nvim` | Write a draft in external Neovim. Ctrl+E opens the current draft |
| `/editor` | Edit the draft in `$VISUAL` / `$EDITOR` |
| `/keymap` | Show key bindings (change them under `keys` in `config.json`) |
| `/focus` | Hide successful tool calls and thinking. Errors stay visible |
| `/density [0\|1\|2]` | Tool detail level: compact, default (one line per call, failures too), full (every call's output previewed inline) |
| `/timestamps` | Toggle `[HH:MM]` timestamps |
| `/bubble [on\|off]` | User messages as right-aligned bubbles (on) or full-width (off) |
| `/statusline [tpl\|off]` | Custom status bar, e.g. `/statusline '{model} {pct} {cost}'` |
| `/theme [dark\|light\|auto]` | Switch theme (saved). Bare: picker. `auto` follows the terminal background (OSC 11 at startup, then `COLORFGBG`) |
| `/mouse [on\|off]` | Mouse capture. Off gives native terminal selection |
| `/clipboard [auto\|osc52\|native]` | How copies reach a clipboard (saved). `auto`: OSC 52 over SSH so text lands on your local machine, the system command locally; `osc52` forces the terminal escape (SSH, tmux), `native` the system command. Bare `/clipboard` shows the current mode |
| `/ascii [on\|off]` | ASCII mode (saved): emoji (including the 🤖 thinking icon) drawn as `?` `+` `#` `*`, for terminals without an emoji font |
| `/animations [on\|off]` | Motion on the busy rule, thinking bar and splash (saved). Off keeps the text, static colors |
| `/clear` | Clear the screen. The model's context is kept |
| `/help` | Short command list |
