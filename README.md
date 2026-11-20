# tango

> it takes two — you and the agent.

A light agent harness for work and code. China-first: DeepSeek / Kimi / Qwen / GLM work
out of the box with a single API key, no proxy.

## Status

Core loop is working: agent loop, JSONL sessions with compaction, Anthropic + OpenAI-compatible
adapters, built-in tools (read / write / edit / bash / grep / find / ls / web_search / web_fetch /
office_read / office_write / notify / diff), a permission gate, AGENTS.md + skills context assembly,
typed event bus, cost tracking with long-prompt price tiers, `/save-as-skill`, scheduled tasks
(`tango task`) with Feishu webhook delivery, auto type diagnostics after edits, a JSONL
serve protocol (`tango serve`), and a Rust TUI client (`tui/`) on top of it.

Through the tknet.ai gateway, tool calling works end to end with `tknet/qwen3.8-flash-next` (the
default), `tknet/deepseek-v4.1-flash` and `tknet/deepseek-v4-pro` (checked 2026-11-14: one-shot,
REPL with permission prompts, `-c`, `-r`). Costs are tknet list prices, which is what tknet bills (it has no per-account discounts; custom
prices come from pricing groups, which tango doesn't see). If
`http(s)_proxy` is set, tango routes its requests through it.

## Model presets

`tango --models` lists presets in three groups: the tknet gateway, then by where the endpoint is served:

| Group | Presets | Key variable |
|---|---|---|
| tknet (own gateway) | `tknet/qwen3.8-flash-next`, `tknet/deepseek-v4.1-flash`, `tknet/deepseek-v4-pro` (tknet.ai, Tencent Cloud Silicon Valley) | `TKNET_API_KEY` |
| China (mainland endpoints, no proxy) | `deepseek/deepseek-flash`, `deepseek/deepseek-v4-pro` | `DEEPSEEK_API_KEY` |
| | `kimi/kimi-k3.1-pro`, `kimi/kimi-k3.1-pro-highspeed` (moonshot.cn) | `KIMI_API_KEY` |
| | `kimi-code/kimi-for-coding` (api.kimi.com/coding, Kimi Code plan) | `KIMI_CODE_API_KEY` |
| | `qwen/qwen3.8-max`, `qwen/qwen3.8-flash` (dashscope.aliyuncs.com) | `DASHSCOPE_API_KEY` |
| | `zhipu/glm-5.3`, `zhipu/glm-5.3-flash` (bigmodel.cn) | `Z_API_KEY` |
| International | `kimi-intl/kimi-k3.1-pro`, `kimi-intl/kimi-k3.1-pro-highspeed` (moonshot.ai) | `KIMI_API_KEY` |
| | `qwen-us/qwen3.8-max`, `qwen-us/qwen3.8-flash` (dashscope-us) | `DASHSCOPE_US_API_KEY` |
| | `anthropic/claude-opus-5-5`, `anthropic/claude-haiku-5-5` | `ANTHROPIC_API_KEY` |
| | `openai/gpt-6-astra`, `openai/gpt-6.1-terra` | `OPENAI_API_KEY` |
| | `google/gemini-4-carbon`, `google/gemini-3.8-flash` | `GEMINI_API_KEY` |
| | `xai/grok-4.7`, `xai/grok-build-0.1` | `XAI_API_KEY` |

China and international accounts of the same vendor use different keys. `kimi/kimi-k3.1-pro` and
`kimi-intl/kimi-k3.1-pro` both read `KIMI_API_KEY`, so only the one matching your account works;
point the other at a different variable in `models.json` if you have both. The default model is the first preset with a key set, which is `tknet/qwen3.8-flash-next`.

Costs follow each vendor's pricing page, including long-prompt tiers: when a request's prompt
(input plus cache reads and writes) passes the threshold, the whole request is billed at the
higher tier. Claude Haiku 5.5 steps up above 100K tokens, GPT-6 Astra and GPT-6.1 Terra above
272K, and both xAI models at 200K. In `models.json`, add tiers as
`"cost": { "input": 1, "output": 2, "tiers": [{ "above": 200000, "input": 2, "output": 4 }] }`.

## Quick start

```sh
npm install
npm run dev -- --help
export DEEPSEEK_API_KEY=sk-...
npm run dev -- "list the files here and summarize the project"   # one-shot
npm run build && (cd tui && cargo build --release) && tango          # TUI
```

Install globally after `npm run build`; `tango` / `tang` are the bin names.

## CLI

```
tango                  fullscreen TUI (the interactive interface)
tango -c / -r <id>     TUI on the most recent / a given session
tango -m <model> -y    TUI options: model, auto mode (or --readonly)
tango [prompt...]      run one prompt and exit (also when stdin is a pipe)
tango --models         list models   tango --sessions   list sessions   --cwd <dir>
tango repl             line-mode REPL — dev fallback, not the product
```

TUI commands: `/model`, `/agents`, `/resume`, `/new`, `/rename`, `/mode`, `/plan`, `/goal`,
`/loop`, `/effort`, `/review`, `/security-review`, `/bg`, `/tasks`, `/stop`, `/subtask`,
`/worktree`, `/mcp`, `/plugins`, `/vim`, `/nvim`, `/editor`, `/multiline`, `/btw`, `/diff`,
`/focus`, `/density`, `/mouse`, `/theme`, `/compact`, `/autocompact`, `/skill`, `/skills`,
`/memory`, `/remember`, `/cost`, `/usage`, `/stats`, `/insights`, `/context`, `/queue`,
`/timestamps`, `/statusline`, `/preset`, `/permissions`, `/import`, `/cd`, `/pwd`, `/add_dir`,
`/config`, `/keymap`, `/init`, `/doctor`, `/debug-zip`, `/status`, `/clear`, `/rewind`, `/undo`,
`/redo`, `/fork`, `/handoff`, `/takeover`, `/branch`, `/tree`, `/find`, `/export`, `/transcript`, `/copy`, `/recap`,
`/help`, `/exit`. `/model <id>` also saves the choice as the default for new sessions.

## Worktrees, background, subagents, MCP, plugins

- **/worktree** — managed git worktrees (`~/.tango/worktrees/<name>`, branch `tango/<name>`):
  the isolation unit for anything that writes in parallel. Dirty worktrees refuse removal;
  `/worktree merge <name>` merges back into the main workspace (which must be clean).
- **/bg <prompt>** — a background prompt: fresh worktree + independent serve child; the task
  runs to completion (detached), its session lands in the shared sessions dir (`/tasks` shows
  status + session, `/stop` kills the process group). `/bg --long` uses the longContext role.
- **/subtask <task>** — a subagent: a background task on the subagent role model whose final
  summary is written back into THIS session (`⑂ subtask …`); main turns are never disturbed.
- **/agents [role model]** — role-based routing: `driver` (live), `subagent` (/subtask, /bg),
  `longContext` (/bg --long). Defaults to the current model.
- **/mcp** — MCP servers from config `mcpServers: {name: {command, args}}` (stdio JSON-RPC);
  tools bridge as `mcp_<server>_<tool>` (collision-suffixed), treated as write-effect
  (permission rules apply). Dead servers lazily reconnect on the next call (capped at 3
  attempts); spawn failures and EPIPE never take serve down.
- **/plugins** — local plugins: `~/.tango/plugins/<name>/plugin.json` with
  `{name, description, enabled?, hooks?: {beforeTool?, afterTool?}}`; hooks receive the call
  JSON on stdin like config hooks; `enabled: false` skips.

Not doing (by design): concurrent writers in the main workspace (worktrees are the only
isolation), MCP server→client capabilities (sampling/roots), plugin marketplaces.

## Workflow modes

- `/plan` — research-only mode: every write-effect tool call is vetoed (even allow rules
  can't widen it) and the model is instructed to produce an execution plan; `/plan` again to
  execute. PLAN badge in the status bar.
- `/goal <text>` — keep working toward a goal: after each turn tango asks (tool-free) whether
  the goal is done; NO → one auto-continuation, YES → cleared with a notice. Hard cap
  (`/goal max N`, default 5), interrupt stops it, ●goal badge while active.
- `/loop 10m <prompt>` — interval-repeated prompt (floor 10s), `/loop off` or ctrl+c stops.
- `/effort low|medium|high|off` — reasoning effort (openai `reasoning_effort`, anthropic
  thinking budget), session-scoped.
- `/review [paths]` / `/security-review` — the working diff inlined into a instructed
  correctness/security audit prompt.
- `/remember <text>` appends one line to `~/.tango/AGENTS.md` (the global memory every
  session's context assembles); `/memory` views/edits it.
- `hooks` in config.json: `beforeTool` / `afterTool` shell commands receive the call JSON on
  **stdin**; hook failures warn on stderr and never block the tool.

## Lean by design (token plans)

On metered/token plans the harness's own overhead is quota: the system prompt always rides
the cacheable message prefix on every route (OpenAI-native endpoints use the `developer`
role newer models expect — GPT-6.2 bills out-of-prefix instructions uncached, so tango
never sends one; the anthropic route adds an explicit `cache_control` breakpoint). Tango
keeps the per-request prompt assembly small — system prompt ≈ 240 tokens, tool schemas ≈ 1.7k, so a bare project
starts under 2k tokens before any conversation (audit with `/context`). Vendor specifics are
first-class: per-model `reasoningField` (qwen/dashscope `reasoning_content` by default),
`prompt_tokens_details.cached_tokens` cache accounting in `/status` (缓存命中 %) and
`/usage` token totals, Anthropic system-prompt cache breakpoints. qwen4-tokenplan presets
(qwen4-max 1M window, deepseek-v4-pro, glm-5.3, flash) work with just `QWEN4_API_KEY`.

## Permissions rules

Three rule kinds, persisted in config.json (`permissions.allow`) and live via `/permissions`:
`tool:<name>` (always allow that tool), `path:<prefix>` (allow writes under a path — relative to
cwd or absolute, segment-boundary matched so `src` never matches `src-evil/`), `bash:<prefix>`
(first command tokens, so `bash:git status` matches `git status -s` but not `git_status`).
Rules never override readonly mode; they only turn "ask" into "allow".
`/import claude-code|grok|codex` maps another agent's allowed-tools into these rules and
reports every entry it could not represent (codex reads trusted `command = "…"` entries from
`~/.codex/config.toml`, loose TOML scan, `path:` override for testing). Bash rules cover only
the command segments they match: a risky command later in the same line (`git status; rm …`)
still asks. `/export` and `/insights` write into `~/.tango/exports/` (never dirty the repo). `/cd <dir>` moves the session working directory (agent,
tools and checkpoints follow live); `/add_dir <dir>` registers an extra writable root.

## Observability

- `/context` — what the next request carries: system prompt / skills / tool schemas / history as
  estimated tokens, with a share bar and 60/80% warnings.
- `/queue` — the running prompt and the ones queued behind it (ctrl+c drains the queue).
- `/usage` / `/stats` — cost and token aggregates across ALL sessions, by model and by session;
  `/insights` writes the same data as an offline single-file HTML report.
- `/timestamps` toggles `[HH:MM]` prefixes; `/density 0|1|2` sets tool-detail level (compact /
  default-errors-only / full) and applies to restored history too.
- **Mouse basics** (Claude Code / OpenCode parity): drag in the chat to select — whole
  logical lines highlight and release copies them to the system clipboard; a click on a
  complete local file path (absolute, `./…`, `~/…`, `:line` suffix tolerated) opens the file
  in the read-only viewer; the permission modal takes `c` to copy the pending command before
  answering y/n/a. `/mouse off` still hands selection back to the terminal natively.
- **IME auto-switch (macOS)**: single-key dialogs (y/n/a) are unreachable through a CJK
  input method. When `macism` is on PATH, a permission dialog switches to ABC automatically
  and restores your previous input method when you answer; already-roman keyboards and a
  missing macism are left untouched.

- **Prompt queue** (while a turn runs): Enter enqueues locally (sent in order when the turn
  ends); `/queue` opens a clickable dialog (per-item [并入当前]/[删], footer [全部并入]/[清空])
  and also accepts `rm <n>`/`all`/`clear`; **Ctrl+C interrupts only the current reply and keeps the queue**; **Alt+Enter appends the typed
  text to the RUNNING turn** ("forgot to mention…") — it rides the same turn's next model
  request as a `（补充）…` user message. The status bar pins the active ask (▶) and each
  finished turn leaves a `✓ 本轮：…` recap line.
- `/autocompact [N%|0.N|off]` shows or sets the auto-compaction threshold (default 75% of the
  window); persisted in config.
- `/statusline '<tpl>'` customizes the bottom bar with `{model} {mode} {cost} {ctx} {pct} {session}
  {tokens} {work}`; unknown placeholders pass through; `off` restores the default.
- `/preset save <name>` stores the current draft as a reusable template (`~/.tango/presets.json`);
  bare `/preset` opens a filtered picker and fills the input without sending.

## Time travel

Every prompt turn is checkpointed automatically: before a turn's first write-effect tool call
(`write`/`edit`/`bash`/`office_write`) a git tree snapshot is taken through a **temporary index**
(`refs`-only commits — your branch, index and `git status` are never touched), and the turn's end
state is snapshotted too. Snapshots live in the session log; the JSONL log is the single truth.

- `/rewind` opens a timeline picker (one node per turn); choosing one rolls **code + conversation
  + model context** back to just before that turn. Smart delete removes agent leftovers (files that
  exist in a later snapshot but not the target), keeps files no snapshot knows about (your own
  hand-made files are reported, never deleted) and never touches `.gitignore`d files.
- `/undo` / `/redo` step one turn back/forward. The abandoned future is never lost: it stays in the
  log (marked `⟲`), `/redo` restores it, `/find <text>` searches all sessions including abandoned
  futures, `/export [md|html]` writes the full transcript (rewind boundaries marked) to a local file.
- `/fork [name]` copies the session up to the current (or picked) turn into a new session —
  try another direction; the original is untouched. `/branch` is the in-place variant: keep the
  named turn, drop the rest, original future stays recoverable. `/tree` shows the fork tree.
- `/handoff [session] [note]` sends the current session's work to an existing session: a
  tool-free call writes a brief (goal, done, decisions, files touched, current state, next steps,
  open questions; `note` steers it) and queues it under `~/.tango/handoffs/<target>/`. You stay
  where you are. The target merges the brief into its context when it is next opened, or before
  its next prompt if it is already open in another window; its own history is kept. A brief
  still unmerged after 7 days is discarded with a notice instead. `/handoff list` shows every
  queued brief with its queue id; `/handoff cancel <queue id>` withdraws one. If the target is
  open in another window, `/handoff` says so (and `/takeover` warns that a live source keeps
  moving after the snapshot). Each open session holds `~/.tango/locks/sessions/<id>.json`.
- `/takeover [session] [note]` is the reverse: it briefs an existing session and merges that brief
  into the current one, leaving the source untouched. Both take an id or prefix; bare commands open
  a session picker. Briefs show in the transcript where they were merged.
- `/transcript` opens the raw transcript (including `⟲` abandoned futures); `/copy [N]` copies the
  last (or Nth-last) assistant reply; `/recap` streams a one-line summary without touching the session.
- Sessions from before checkpoints degrade gracefully: rewind restores the conversation only and
  the timeline marks those turns 无代码快照. Non-git directories fall back to tar snapshots under
  `~/.tango/snapshots/`. Set `"checkpoint": "off"` in `~/.tango/config.json` to disable.

## Session warming

`"warming"` in config keeps provider prompt caches warm between prompts (opencode parity):
while the session idles, tango sends a tool-free keep-alive that rides the full
conversation context and is discarded — nothing enters history. Default off.

```json
{ "warming": { "interval": "auto", "duration": 1800 } }
```

`interval` seconds between keep-alives (`"auto"` = 240s — tango has no per-model cache-TTL
data yet); `duration` seconds of inactivity after the last real request during which warming
runs (default 1800). Any real turn restarts the window; warming never extends it and never
runs during a live request. A cache **miss** (reads < ½ of input) halts warming until the
next real request, so an expired cache never burns tokens. Keep-alives are real billable
calls — enable when cache savings outweigh the usage.

## Scheduled tasks

`tango task add 新闻摘要 2h 搜集今天的 AI 新闻，写成摘要发飞书 [--mode auto]` stores a task in
`~/.tango/tasks.json`. `tango task run --loop` is the daemon: every minute it runs tasks whose
interval has passed, each in its own session (`task-<name>-<time>.jsonl`, visible in `--sessions`).
Results go to the `feishuWebhook` in config as a card. Headless tasks can't answer prompts, so
they default to `readonly` (reads, web, writes files); `--mode auto` also allows bash.
The model can also push mid-task with the `notify` tool. cron works too — `tango task run` runs
due tasks once and exits. On macOS, `scripts/com.tango.task.plist` installs the daemon as a
launchd service (starts at login, respawns on crash): copy it to `~/Library/LaunchAgents/`,
`launchctl bootstrap gui/$(id -u) <path>`. launchd gets no shell environment, so the wrapper
sources API keys from `~/.tango/env` (KEY=VALUE, chmod 600); logs go to `~/Library/Logs/tango-task.log`.

## Skills

Global skills live in `~/.tango/skills/<name>/SKILL.md` (project: `.tango/skills/`); only name +
description enter the system prompt. `/save-as-skill 名称` distils the current session (user turns
+ tool calls) into a draft under `~/.tango/skill-drafts/`. Edit the draft — above all write a real
`description:` — then `/save-as-skill finish <file>` installs it as a project skill.

## Coding feedback loop

After every `write`/`edit` of a source file tango runs the project's *own* checker and appends any
errors to the tool result, so the model sees a broken edit immediately: `tsc --noEmit` for
TypeScript, `pyright` (then `ruff`) from `.venv` or `node_modules` for Python, `go vet`, `cargo check`.
Silent when the project has no checker installed. The `diff` tool (read-only) shows what changed:
a summary, or full hunks per file.

## Serve protocol (the UI seam)

`tango serve` speaks JSONL over stdin/stdout (the only transport today): one JSON command per
stdin line, one JSON frame per stdout line. Human logs go to stderr; stdout is protocol-only.
Commands: `prompt`, `btw` (ephemeral, tool-free side question), `interrupt`, `permission`
(answers a `permission_request` frame by request id), `models` (list presets with key availability),
`set_model` (also saved as the default), `set_mode`, `set_nvim_viewer` (persist `ask`/`readonly`),
`set_theme`, `set_mouse`, `diff` (read-only working-tree/staged patch), `timeline` (turn list with
snapshot flags), `rewind`/`undo`/`redo` (time travel; refused while a turn runs), `fork`/`branch`/
`tree` (session forking and ancestry), `handoff` / `takeover` (`session`, `note?` → `brief`; handoff also → `queueId`), `handoff_list` (→ `handoffs`), `handoff_cancel` (`queueId`), `find` (search all sessions incl. abandoned futures),
`export` (md/html transcript file), `transcript` (raw text incl. orphans), `rename`, `recap`
(tool-free one-line summary via `btw_stream`), `sessions`, `history`
(flattened transcript with paired tool calls/results, latest 300 items by default), `save_skill`,
`compact`, `status`. Prompts run one at a time; `interrupt`, `permission`, `set_mode` and `status`
skip that queue so they land mid-turn. A client switches sessions by respawning the child with
`TANGO_RESUME=<id or prefix>` (and optionally `TANGO_MODE`, `TANGO_MODEL`) in the environment —
serve takes them from there, no flags. Frames: `hello` (session/model/tools/context) → `stream`
(token deltas) → `tool_result` / `permission_request` → `message` → `turn_end` (usage, context
window/tokens + running cost) → `result` (per command). Side questions use request-id-tagged
`btw_stream` frames and never enter session history. stdin EOF lets queued work finish, so pipes
work too. This is the seam the TUI —
and later web/IDE shells — plug into; the core never knows which face is attached.

## TUI

`tui/` holds the Rust client (ratatui + crossterm): `cargo build --release` in `tui/`, then run
`tango` (`tango -c` / `tango -r <id>` open a previous session). It spawns `tango serve` and speaks the protocol above — fullscreen
alternate screen, mouse wheel scrolling, streaming answers, thinking lines, tool status lines with
durations, a centered permission modal (y/n/a), a bottom status bar below the input (model · mode ·
cost · session) and a multi-line input with Tab-completed `/` commands and `@file` inlining (contents ride along in the
prompt, 6KB cap), fences render as indented gray blocks, and long pasted prompts collapse to a
short summary (over 600 chars or 8 lines). Ctrl+O opens the latest full prompt/tool output in an
independent scrollable viewer; clicking a specific summary/tool line opens that item. Closing the
viewer restores the original chat position. All overlays use the shared `tui/src/dialog.rs` frame:
rounded complete borders, a themed surface and title background, bottom key hints, and content
confined to the inner rectangle. Focus is viewer → permission → picker → completion → chat; modal
scrolling never leaks into the chat, pickers support wheel/click, and Esc closes the focused layer.
Opening full content checks `nvim` and `vim` with `--version`; only an actual `NVIM` is used,
never standard Vim. On first detection, choose one-time read-only, persistent read-only
(`nvimViewer: "readonly"`), edit, or the internal viewer. In the internal viewer, Ctrl+E edits the
current item in Neovim. After saving, choose a new request, full reprocessing, or a correction
containing a unified diff; the original session history and tool results are never rewritten.
Unchanged saves submit nothing. Leaving Neovim returns to the prompt (or to the edit-resolution
picker when submitted text changed), not to the internal viewer. In the input, Ctrl+E opens the
current unsubmitted draft in external Neovim; bare `/nvim` starts with an empty draft. Saving and
quitting returns the edited text to the input without submitting it or opening the diff-resolution
picker; Enter or Ctrl+S sends it explicitly. If Neovim is unavailable, the draft stays in place.
Bracketed paste never submits by itself: multi-line and long pasted text is
shown as `[Pasted text #N +M lines]` followed by a fresh input line, while the full original text is
kept for submission. Further typing stays visible, and the input scrolls to keep the cursor in view.

Pasting an **image** works like the CLI agents: terminals drop a clipboard image to a temp file and
paste its path (e.g. `/var/folders/…/orca-paste-….png`); tango recognizes a single-line paste naming an
existing `.png/.jpg/.jpeg/.gif/.webp` file and turns it into a compact `[图片 #N]` chip instead of the
long path. The chip is atomic (Backspace/Delete removes it whole), survives Alt+S draft stashing, and
is **mouse-clickable** — clicking it (in the input box or in the chat transcript) opens the image in
the system viewer (macOS `open` / Linux `xdg-open`). On submit the image rides the next prompt as a
base64 upload (`images` field → `ImageBlock` → vendor `image_url`/`image` content), with the chip text
`[图片 #N]` left in the message so the model knows where it sits; queued and Alt+Enter-supplemented
prompts carry their images too. Resumed history replays images as clickable `[图片 <path>]` chips.
Models flagged `vision: true` (Claude / Gemini / GPT consumer flagships / Grok-4.7 / qwen4-max presets) accept
images silently; an unflagged model still receives the image but prints a heads-up notice — override
per-model with `"vision": true` in `~/.tango/models.json`. The `read` tool likewise returns image files
as images. Limits: 10MB per file; external-editor (`/editor`) round-trips flatten chips back to path
text.
Keys follow
one rule: nothing but `/exit` (or `/quit`) can leave the session. Ctrl+C interrupts a running turn
and clears the input when idle — press it as often as you like; Esc closes popups, else does the
same (OpenCode-style). `TANGO_BIN` overrides the server binary (e.g. `TANGO_BIN="node bin/tango.js"`
for dev builds). Bare `/model` and `/resume` open centered pickers; picking a session kills the
serve child and respawns it with `TANGO_RESUME`, so both the model context and the visible
transcript come back. `/new` starts fresh while keeping the current model and permission mode.
Ctrl+P opens a filterable command palette; model/session pickers use the same filtering and mouse
routing. Ctrl+R searches original prompt history, and Alt+S stashes/restores the complete draft
(including collapsed paste metadata) without submitting it. File references accept inclusive line
ranges: `@path#20-45`, `@path#L20-L45`, or `@path#20`. Scrolling up pauses auto-follow and keeps the
reading position stable; new output shows an unread badge, and clicking it or Ctrl+End resumes
following. Ctrl+T opens a transcript with `/` search, `n/N` matches, `{`/`}` prompt navigation,
and `c` to copy. Tool failures show key output inline; `/focus` hides successful tools and thinking
but never errors. The status bar includes context tokens/window/percentage with 60%/80% warning
thresholds. `/diff` opens a read-only working-tree patch (`--staged` and optional paths supported).
`/btw <question>` opens a separate streamed answer without tools or session-history writes.
Set `theme` in `~/.tango/config.json` to `dark`, `light`, or `auto`; `auto` recognizes light
terminal backgrounds 7, 15 and 255 in `COLORFGBG`. Missing, malformed or unknown values use dark.
`/theme` applies and saves changes immediately. `/mouse [on|off]` controls mouse capture; off keeps
keyboard scrolling and native terminal selection. Both settings persist in config.
Set `editor` to `vim` for modal editing (the default is `default`), or toggle it for the current
TUI process with `/vim` (also `/vim on` / `/vim off`). This is the TUI's built-in lightweight
Vim-style key handling, not an embedded Neovim process. Entry prints a short key hint, and the
status bar continuously shows `VIM INSERT`, `VIM NORMAL`, or `VIM COMMAND`. Vim starts in INSERT;
Esc enters NORMAL, where `h`/`l`, `0`/`$`, `x`, `i`/`a`/`I`/`A` edit and `j`/`k` move the input
cursor between logical lines (arrow keys still scroll chat). `/` and `@` return to INSERT. In Vim INSERT, Enter adds a newline instead of submitting; NORMAL Enter does
nothing. Ctrl+S submits the complete draft in either mode (and in non-Vim mode); NORMAL `:wq` and
`:wqa` also submit. Entering `:` expands the input by one row: the draft stays visible above and
the command has its own bottom line. Esc cancels that line without clearing the draft. The optional `keys`
object remaps `expandTools`, `newline`, `interrupt`, `clearInput`,
`scrollUp`, `scrollDown`, `pageUp`, and `pageDown`, using values such as `ctrl+u` or `alt+enter`.
Unknown actions, keys, and editor values fall back to defaults. Ctrl+C always remains a safe
interrupt/clear binding even when `interrupt` is remapped. Example:

```json
{ "editor": "vim", "keys": { "expandTools": "ctrl+u" } }
```

## Permissions

Three modes, set with flags or `/mode`:

| mode | flag | behaviour |
| --- | --- | --- |
| `ask` | (default) | reads and writes inside cwd run; deletes, `git push`/`reset --hard`, `sudo`, `curl … \| sh`, writes outside cwd and unknown tools ask first |
| `auto` | `-y` / `--yes` | everything runs |
| `readonly` | `--readonly` | reads only; writes and shell are refused |

In the TUI's permission dialog, answer `y`, `n` or `a` (always: switches to `auto` for the rest of
the session, taking effect mid-turn). One-shot runs
can't ask, so risky calls are refused there unless `--yes` is given. The gate is a `before_tool`
handler in `src/core/permissions.ts`.

## Web tools

`web_search` uses the first backend whose key is set: `TAVILY_API_KEY`, `BOCHA_API_KEY` (博查),
`BRAVE_API_KEY`. With no key it scrapes Bing's public results page, which works from mainland
China but can break if Bing changes its markup. Force one with `TANGO_SEARCH=bing|tavily|bocha|brave`.

`web_fetch` reads an http(s) URL and turns HTML into markdown-like text (headings, links, lists),
decoding GBK pages correctly. It refuses binary content, stops at 5 MB and times out after 30 s.
Both tools count as reads for permissions.

## Office files

`office_read` extracts text from PDF, Word, Excel and PowerPoint with no npm dependencies.

| type | how | options |
| --- | --- | --- |
| .docx | built-in ZIP + XML reader; headings become `#`, lists `-`, tables markdown | |
| .xlsx | tab-separated rows prefixed with the row number; dates converted from serials | `sheet` (name or index), `rows` (default first 500) |
| .pptx | slides in presentation order, plus speaker notes | `pages` (slide range) |
| .pdf | `pdftotext -layout` (poppler), else macOS PDFKit; says so when a PDF has no text layer (scanned) | `pages` |
| .doc .rtf .odt .wps | macOS `textutil` | |

Old binary .xls/.ppt are refused with a hint to re-save as .xlsx/.pptx. `brew install poppler`
gives much better PDF output than the PDFKit fallback. Counts as a read for permissions.

`office_write` creates .docx (markdown-ish text: `#` headings, lists, `| tables |`, **bold**)
and .xlsx (tab-separated rows, or `[{name, rows}]` for several sheets — the same shape
`office_read` prints, so read → edit → write round-trips). No dependencies; the generated files
open in Word, WPS, Numbers and macOS textutil. Counts as a write for permissions.

## Config

All user state lives in `~/.tango/` (override with `TANGO_HOME`):

| file | purpose |
| --- | --- |
| `config.json` | default model (`/model <id>` writes it); `feishuWebhook` for notify/tasks; `theme`: `dark` (default), `light`, or `auto` from the terminal's `COLORFGBG` |
| `models.json` | extra/overriding model specs, same shape as built-in presets |
| `tasks.json` | scheduled tasks (`tango task add`) |
| `AGENTS.md` | global instructions appended to the system prompt |
| `skills/<name>/SKILL.md` | global skills (project-level: `<cwd>/.tango/skills/`) |
| `sessions/*.jsonl` | append-only sessions, one line per entry |

`apiKey` is either an environment variable name or the key itself. A value made only of uppercase
letters, digits and underscores (`"DEEPSEEK_API_KEY"`) is read from that variable; anything else is
used as the literal key. Nothing is inferred. `tango --models` shows which variable each preset reads.

## Layout

```
src/core/
  types.ts      shared message/model/tool/session types (imports nothing)
  events.ts     typed bus; before_tool can veto, before_request can mutate
  agent.ts      the loop: model → tools → model, persist + emit each step
  session.ts    append-only JSONL store, compaction = summary entry
  context.ts    system prompt = base + AGENTS.md (upward + global) + skills + env
  skills.ts     /save-as-skill drafts → SKILL.md
  diagnostics.ts after write/edit → project checker (tsc/pyright/ruff/go vet/cargo) → tool result
  scheduler.ts  tango task: due-check, per-run sessions, Feishu webhook, daemon loop
  config.ts     ~/.tango loading, model picking, key availability
  permissions.ts  ask / auto / readonly gate on before_tool
  models/       anthropic-messages + openai-completions adapters, presets, cost
  tools/        registry + read/write/edit/bash + grep/find/ls + web_search/web_fetch + notify + diff
                office/ = office_read (zip + xml readers) + office_write (zip writer, docx/xlsx)
src/host.ts     shared agent assembly for every host (REPL, serve, tasks)
src/serve.ts    tango serve: JSONL protocol for external UIs (the TUI seam)
src/cli.ts      REPL / one-shot entry
```

Design rules (keep them):
1. `core` stays UI-free; shells (REPL, TUI, message bots) sit on top of `Agent`.
2. New capability = a tool or an event handler, not a change to the loop.
3. `types.ts` imports nothing; adapters translate, core never speaks vendor formats.

## Development

```sh
npm run typecheck
npm test
```
