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
| Add a newline | Alt+Enter while idle, or switch Enter's meaning with `/multiline` |
| Include a file | `@src/app.ts`, or a line range: `@src/app.ts#20-45` |
| Attach an image | Paste it; terminals paste a temp file path, which becomes a `[图片 #N]` chip |
| Run a command | Type `/` and Tab-complete, or open the palette with Ctrl+P |
| Search earlier prompts | Ctrl+R |
| Park a half-written draft | Alt+S (press again to restore) |
| Edit a draft in Neovim | Ctrl+E, or `/nvim` |
| See a full tool output | Ctrl+O, or click the tool line |
| Check whether it's still working | The rule above the input: a breathing wave (`▁▃▅▃▁`) when idle, or a dancing braille figure with the elapsed time and received characters — plus a `■ ⬝ ⬝` bouncing loader at the status bar's left end |
| Find the logo | An empty session shows the centered TANGO wordmark (block letters, cyan gradient + twinkle) with the dancer and a hint; the first message replaces it |
| Get back to the prompt you're reading about | Scrolled up, the dim top row shows that prompt; click it to jump there |
| Hop between your prompts | `alt+↑` / `alt+↓` walk to the previous / next prompt, landing it at the top of the view; the last `alt+↓` returns to the bottom |
| Search the whole transcript | Ctrl+T, then `/` |
| Ask a side question | `/btw <question>`. The answer doesn't enter the session |
| Answer the model's ❓ panel | ↑↓ + Enter, click, or type the option number + Enter. Esc hides it; `/todo` brings it back |

A tool call in flight shows as `⚙ bash …` with the gear color cycling; when it finishes
the same line becomes `⚙ bash <command> · <ms>` (click it or press ctrl+o for the output).

Reasoning the model streamed shows as `✻ …` lines with a `┃` bar on their left. While the
turn runs, that bar's color scrolls down the newest thinking block (needs a truecolor
terminal; otherwise, and once idle, it is a flat dim bar).

While a turn runs, Enter queues your next prompt instead of interrupting. Queued prompts
are sent in order when the turn ends. `/queue` shows them, and its `[并入当前]` button merges
one into the running turn. Alt+Enter does the same for what you're typing: the text joins the
running turn, and the model sees it on its next request. Ctrl+C (or Esc) stops the current
reply and keeps the queue.

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

### Worktrees

A managed worktree is a separate checkout of your repo at `~/.tango/worktrees/<name>` on
branch `tango/<name>`. Use one whenever two things might write files at the same time. Tango
assumes a single writer in the main workspace and warns if a second tango session opens in
the same directory.

| Command | Effect |
| --- | --- |
| `/worktree` | List managed worktrees with their uncommitted-change counts |
| `/worktree new <name>` | Create one (names: letters, digits, `.`, `_`, `-`, up to 40 chars) |
| `/worktree merge <name>` | `git merge` its branch into your current branch |
| `/worktree rm <name>` | Remove it. Refused while it has uncommitted changes |

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

Switch at any time with `/mode ask|auto|readonly`. Answering `a` in a permission dialog
switches to `auto` for the rest of the session.

`/plan` is a stricter, temporary mode for research: all writes are refused and the model
writes a plan instead. Run `/plan` again to leave it and carry the plan out.

To stop being asked about something specific without going fully `auto`, add an allow rule:

```
/permissions add tool:web_fetch       always allow one tool
/permissions add path:src             allow writes under src/ (not src-old/)
/permissions add bash:git status      allow commands starting with "git status"
/permissions                          list rules · /permissions rm <rule> removes one
```

Rules are saved in `~/.tango/config.json`. They only turn "ask" into "allow" and never
loosen `readonly` or `/plan`. A bash rule covers only the part of a command line it
matches, so `git status; rm -rf build` still asks. `/import claude-code|grok|codex` copies
another agent's allow list into these rules and lists any entries it couldn't convert.
It reads `.claude/settings.json` in the project, `~/.grok/settings.json`, or
`~/.codex/config.toml` (pass a different path after `codex`).

`/add_dir <dir>` makes a directory outside the project writable without asking.

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
| `/copy [N]` | Copy the last (or Nth-last) assistant reply to the clipboard |
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
| `/queue` | Manage queued prompts: `rm <n>`, `all` (merge all into the running turn), `clear` |
| `/interrupt` | Stop the current turn (same as Ctrl+C) |
| `/mcp` | MCP servers and the tools they provide |
| `/plugins` | Installed local plugins |
| `/permissions [add\|rm <rule>]` | List, add or remove allow rules |
| `/import claude-code\|grok\|codex [path]` | Copy another agent's allow list into tango's rules |
| `/config` | View the current config |
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
| `/theme [dark\|light\|auto]` | Switch theme (saved). Bare: picker |
| `/mouse [on\|off]` | Mouse capture. Off gives native terminal selection |
| `/clear` | Clear the screen. The model's context is kept |
| `/help` | Short command list |
