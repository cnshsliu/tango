# tango handbook

How to use tango day to day. For installation, model presets, status and architecture, see
the [README](../README.md).

- [Get started](#get-started)
- [Everyday workflows](#everyday-workflows)
  - [Sessions](#sessions)
  - [Background work: /bg vs /subtask](#background-work-bg-vs-subtask)
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
| Include a file | `@src/app.ts`, or a line range: `@src/app.ts#20-45` |
| Ask about a file by its path | Paste or drag the full path (`/Users/me/notes.md`, `~/x.md`, quoted or with `\ ` escapes), alone or followed by your question. It goes to the model as `文件：<path>` plus your text; a bare path asks the model to read and understand the file. It is never taken for a slash command; a path that doesn't exist says so |
| Attach an image | Paste it; terminals paste a temp file path, which becomes a `[图片 #N]` chip |
| Run a command | Type `/` and Tab-complete, or open the palette with Ctrl+P |
| Answer a `?choice` panel | ↑↓/click + Enter, or type the option number + Enter. Tab answers in your own words: the input fills with the ask as a lead-in (`关于…，模型在等你选择: `) in the conversation's language — only what you write after it is sent; the panel's title uses the same language. Esc collapses the panel |
| Search earlier prompts | Ctrl+R |
| Park a half-written draft | Alt+S (press again to restore) |
| Edit a draft in Neovim | Ctrl+E, or `/nvim` |
| See a full tool output | Ctrl+O, or double-click the tool line |
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

Reasoning the model streamed shows as a block headed by an upright `🤖` (the text after it is dim
italic), with a `┃` bar on its left. The whole
text is kept: short reasoning shows in full, a long block collapses to its first 3 lines plus
`双击展开思考（还有 N 字 · 共 M 行）`. Double-click any block to expand or collapse it; Ctrl+T's transcript always shows thinking in full. While the
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
| Open a specific session | `tango -r <id or prefix>`, or `/resume` inside the TUI for a picker |
| Start fresh, same model and mode | `/new` |
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

### Background work: /bg vs /subtask

Both commands start a separate tango process in its own new git worktree (`bg-<id>`), so the
work never touches your main checkout and your current session stays free. The background
process runs in `auto` permission mode: nobody is there to answer prompts, and the worktree
can be thrown away.

| | `/bg <prompt>` | `/subtask <task>` |
| --- | --- | --- |
| Use it for | A larger job you will check on later | A small, self-contained job whose result you want here |
| Model | `subagent` role (`/bg --long` uses `longContext`) | `subagent` role |
| Result | Its own session. Open it with `/resume` | A `⑂ subtask … 完成：…` summary added to **this** session |
| Main session | Not touched | Gets the summary once the task ends, at the next idle moment |

Both kinds show up in `/tasks` with their status (`running`, `done`, `failed`, `stopped`)
and session id. `/stop <id or name>` kills a running task.

The task's file changes stay in its worktree. To bring them into your checkout, run
`/worktree merge bg-<id>` (see below). Merging only picks up committed changes, so if the
task left uncommitted edits, commit them in the worktree first. Set role models with
`/agents subagent <model>` and `/agents longContext <model>`. Without them, the current
model is used.

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
`⟳ 请求失败（connection reset [ECONNRESET]），1 秒后第 1/3 次重试…` before each try, waiting 1s,
2s, then 4s. Press Esc to stop waiting. After the last retry the error is shown as before, with
"gave up after 3 retries". Set the count with `"retries": 5` in `~/.tango/config.json` (0-10,
default 3; `0` turns it off), or per model with `retries` in `models.json`. A wrong key (401/403)
fails at once, since retrying cannot fix it. A reply that breaks off midway is not retried,
because tango would have to throw away the text it already showed.

### Scripts and CI (one-shot runs)

`tango "<prompt>"` runs one prompt without the TUI and prints the answer. For a script that needs
to read the result, add `--format json`: every stdout line is then a JSON event (start, each model
step, each tool call with its output and duration, token usage and cost, errors) and the last line
is always a `done` summary. `--max-tool-rounds N` caps how many model→tool rounds the run may take,
so a stuck task fails fast instead of using the default 64. One-shot runs cannot answer permission
asks, so risky calls are refused unless you add `-y`. The README's "Headless JSON" section lists
every event field.

### Worktrees

A managed worktree is a separate checkout of your repo at `~/.tango/worktrees/<name>` on
branch `tango/<name>`. Use one whenever two things might write files at the same time. Tango
assumes a single writer in the main workspace and warns if a second tango session opens in
the same directory. Writing into a managed worktree never asks for permission, even from a
session whose cwd is the main checkout. A checkout anywhere else outside the cwd still asks,
unless you `/add_dir` it.

| Command | Effect |
| --- | --- |
| `/worktree` | List managed worktrees with their uncommitted-change counts |
| `/worktree new <name>` | Create one (names: letters, digits, `.`, `_`, `-`, up to 40 chars) |
| `/worktree merge <name>` | `git merge` its branch into your current branch |
| `/worktree rm <name>` | Remove it. Refused while it has uncommitted changes |

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

Every tool call passes a permission gate. There are three modes:

| Mode | Start with | Behaviour |
| --- | --- | --- |
| `ask` | default | Reads and writes inside the project run. Deletes, `git push`, `git reset --hard`, `sudo`, `curl … \| sh`, writes outside the project and unknown tools ask first |
| `auto` | `tango -y` | Everything runs |
| `readonly` | `tango --readonly` | Reads only. Writes and shell are refused |

Switch at any time with `/mode ask|auto|readonly`. Answering `a` (始终允许) in a permission
dialog saves persistent allow rules for that call and allows it — `bash:git branch` for
`git branch -D x`, the written file's directory for an outside write, `tool:<name>` otherwise —
so the same thing never asks again, in this session or the next. A command line with several
parts (`osascript … && rm -f x`) saves one rule per command, so the line is covered as a whole;
the hint under the dialog says what will be remembered (`a 始终允许（记住 osascript · rm）`).
That also allows each of those commands anywhere, so for a one-off answer `y`. `/permissions`
lists and removes the rules it created.

`/plan` is a stricter, temporary mode for research: all writes are refused and the model
writes a plan instead. Run `/plan` again to leave it and carry the plan out. In the TUI,
Shift+Tab does the same toggle; the input block's left bar and the `mode · model` row under the
draft turn orange while plan is on (ask is blue, auto red, readonly gray). Shift+Tab never switches
to `auto`; use `/mode auto` for that.

To stop being asked about something specific without going fully `auto`, add an allow rule:

```
/permissions add tool:web_fetch       always allow one tool
/permissions add path:src             allow writes under src/ (not src-old/)
/permissions add bash:git status      allow commands starting with "git status"
/permissions                          list rules · /permissions rm <rule> removes one
```

Rules are saved in the project's personal `.tango/config.local.json` (outside a project: in
`~/.tango/config.json`); `/permissions` labels each rule with its file. They only turn "ask" into "allow" and never
loosen `readonly` or `/plan`. A bash rule covers only the part of a command line it
matches, so `git status; rm -rf build` still asks. `/import claude-code|grok|codex` copies
another agent's allow list into these rules and lists any entries it couldn't convert.
It reads `.claude/settings.json` in the project, `~/.grok/settings.json`, or
`~/.codex/config.toml` (pass a different path after `codex`).

`/add_dir <dir>` makes a directory outside the project writable without asking.

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
Details and payload fields: README, "Hooks".

### Project config

A repo can carry its own `.tango/config.json` (committed), and you can keep a personal
`.tango/config.local.json` next to it that git ignores. Both sit on top of
`~/.tango/config.json`: single values come from the nearest file, rule and hook lists add up.
`/config` shows every file and the merged result.

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
| `/resume` (`/sessions`) | Open the session picker and switch to the one you pick |
| `/rename [name]` (`/name`) | Show or set the session name. In the picker, Ctrl+R renames any session |
| `/editlast` (Alt+E) | Interrupt the running turn, roll the conversation **and code** back to before your last prompt, and load that prompt verbatim into the input box for editing. Two presses: the first arms (rollback touches code), the second executes; Esc cancels |
| `/find [text]` | Search all sessions, including abandoned turns, and jump to a hit |
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
| `/model [id]` | Switch model and save it as the default. Bare: open the model picker |
| `/agents [role model]` | Show or set role models: `driver`, `subagent` (`/bg`, `/subtask`), `longContext` (`/bg --long`) |
| `/mode [ask\|auto\|readonly]` | Show or set the permission mode |
| `/plan` | Toggle plan mode: writes refused, the model produces a plan |
| `/effort [low\|medium\|high\|off]` | Show or set reasoning effort for this session |
| `/goal <text>` | Keep working toward a goal. After each turn tango checks whether it's done and continues if not |
| `/goal off\|pause\|resume` · `/goal max N` | Clear, pause or resume the goal · set the round cap (1–50, default 5). Bare `/goal` shows status |
| `/loop <interval> <prompt>` | Repeat a prompt on an interval, e.g. `/loop 10m check the build` (minimum 10s) |
| `/loop off` | Stop the loop (Ctrl+C also stops it) |

### Background work and isolation

| Command | What it does |
| --- | --- |
| `/bg <prompt>` | Run a prompt in a new worktree as a detached background session |
| `/bg --long <prompt>` | Same, on the `longContext` role model |
| `/subtask <task>` | Run a small task in a new worktree. Its summary is written back into this session |
| `/tasks` | List background tasks with status and session id |
| `/stop <id or name>` | Kill a running background task |
| `/worktree` | List managed worktrees |
| `/worktree new <name>` · `rm <name>` · `merge <name>` | Create, remove (refused if dirty) or merge one into the current branch |
| `/cd <dir>` | Move the session's working directory. Tools and snapshots follow |
| `/pwd` | Show the working directory |
| `/add_dir <dir>` | Add a writable directory outside the project (saved) |

### Review and side questions

| Command | What it does |
| --- | --- |
| `/review [paths]` | Code review of the working-tree diff |
| `/security-review` | Security review of the working-tree diff |
| `/diff [--staged] [paths]` | Read-only view of the working-tree (or staged) patch |
| `/btw <question>` | Ask about the current context without tools. The answer isn't saved |
| `/todo` | Open the list of pending choices (❓) and suggestions from the model, and act on them one by one |

### Context, memory and skills

| Command | What it does |
| --- | --- |
| `/context` | Token breakdown of the next request: system prompt, skills, tool schemas, history |
| `/compact` | Summarize older history to free up context |
| `/autocompact [N%\|0.N\|off]` | Show or set the auto-compaction threshold (default 75%, saved) |
| `/remember <text>` | Add a line to the global memory `~/.tango/AGENTS.md`, used by every session |
| `/memory` | View the global memory. Ctrl+E edits it |
| `/init [--force]` | Create a project `AGENTS.md` scaffold (`--force` appends to an existing one) |
| `/skills` | List global and project skills |
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
| `/mcp` | MCP servers and the tools they provide |
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
| `/density [0\|1\|2]` | Tool detail level: compact, default, full |
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
