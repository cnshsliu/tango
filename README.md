# tango

> it takes two — you and the agent.

A light agent harness for work and code. China-first: DeepSeek / Kimi / Qwen / GLM work
out of the box with a single API key, no proxy.

How to use it day to day: [docs/handbook.md](docs/handbook.md).

## Status

Core loop is working: agent loop, JSONL sessions with compaction, Anthropic + OpenAI-compatible
adapters, built-in tools (read / write / edit / bash / grep / find / ls / web_search / web_fetch /
office_read / office_write / notify / diff / session_search), a permission gate, AGENTS.md + skills context assembly,
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

| Group                                | Presets                                                                                                                   | Key variable                |
| ------------------------------------ | ------------------------------------------------------------------------------------------------------------------------- | --------------------------- |
| tknet (own gateway)                  | `tknet/qwen3.8-flash-next`, `tknet/deepseek-v4.1-flash`, `tknet/deepseek-v4-pro` (tknet.ai, Tencent Cloud Silicon Valley) | `TKNET_API_KEY`             |
| China (mainland endpoints, no proxy) | `deepseek/deepseek-flash`, `deepseek/deepseek-v4-pro`                                                                     | `DEEPSEEK_API_KEY`          |
|                                      | `kimi/kimi-k3.1-pro`, `kimi/kimi-k3.1-pro-highspeed` (moonshot.cn)                                                        | `KIMI_API_KEY`              |
|                                      | `kimi-code/kimi-for-coding` (api.kimi.com/coding, Kimi Code plan)                                                         | `KIMI_CODE_API_KEY`         |
|                                      | `qwen/qwen3.8-max`, `qwen/qwen3.8-flash` (dashscope.aliyuncs.com)                                                         | `DASHSCOPE_API_KEY`         |
|                                      | `zhipu/glm-5.3`, `zhipu/glm-5.3-flash` (bigmodel.cn)                                                                      | `Z_API_KEY`                 |
|                                      | `zhipu-plan/glm-5.3`, `-flash` (GLM Coding Plan, Anthropic 协议)                                                          | `ZHIPU_CODING_PLAN_API_KEY` |
| International                        | `kimi-intl/kimi-k3.1-pro`, `kimi-intl/kimi-k3.1-pro-highspeed` (moonshot.ai)                                              | `KIMI_API_KEY`              |
|                                      | `qwen-us/qwen3.8-max`, `qwen-us/qwen3.8-flash` (dashscope-us)                                                             | `DASHSCOPE_US_API_KEY`      |
|                                      | `anthropic/claude-opus-5-5`, `anthropic/claude-haiku-5-5`                                                                 | `ANTHROPIC_API_KEY`         |
|                                      | `openai/gpt-6-astra`, `openai/gpt-6.1-terra`                                                                              | `OPENAI_API_KEY`            |
|                                      | `google/gemini-4-carbon`, `google/gemini-3.8-flash`                                                                       | `GEMINI_API_KEY`            |
|                                      | `xai/grok-4.7`, `xai/grok-build-0.1`                                                                                      | `XAI_API_KEY`               |

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

Install globally (`npm link`) after `npm run build` and `cargo build --release` in `tui/`;
`tango` / `tang` are the bin names. Both point at the Rust binary `tui/target/release/tango-tui`:
an interactive launch (`tango`, `-c`, `-r`, `-m`, `-y`, `--readonly`, `--cwd`, `tango tui`)
opens the TUI in that process, and everything else (prompts, `task`, `serve`, `repl`,
`--help`, pipes) is exec'd into `node bin/tango.js` with the same arguments, so no idle
launcher process stays around.

## CLI

```
tango                  fullscreen TUI (the interactive interface)
tango -c / -r <id>     TUI on the most recent / a given session
tango -m <model> -y    TUI options: model, auto mode (or --readonly)
tango [prompt...]      run one prompt and exit (also when stdin is a pipe)
tango --models         list models   tango --sessions   list sessions   --cwd <dir>
tango repl             line-mode REPL — dev fallback, not the product
tango tui              start the Rust TUI from node (dev; TANGO_TUI=<path> picks the binary)
tango serve            JSONL protocol on stdin/stdout for UIs (see "Serve protocol")
tango task list        scheduled tasks (also `tango task` alone)
tango task add <name> <interval> <prompt...> [--mode auto|readonly]
tango task rm <name>   delete a task
tango task run [--loop]  run due tasks once, or keep running as a daemon
```

| flag | short | meaning |
| --- | --- | --- |
| `--continue` | `-c` | continue the most recent session |
| `--resume <id>` | `-r` | continue a given session (id or prefix) |
| `--model <id>` | `-m` | model for this run |
| `--yes` | `-y` | auto mode: tools run without asking |
| `--readonly` | | no writes, no bash |
| `--cwd <dir>` | | working directory for the session |
| `--models` / `--sessions` | | list models with key status / list sessions |
| `--help` / `--version` | `-h` / `-V` | usage / version |

TUI commands: `/model`, `/agents`, `/resume`, `/new`, `/restart`, `/rename`, `/mode`, `/plan`, `/goal`,
`/loop`, `/effort`, `/review`, `/security-review`, `/bg`, `/tasks`, `/stop`, `/subtask`,
`/worktree`, `/mcp`, `/plugins`, `/vim`, `/nvim`, `/editor`, `/multiline`, `/btw`, `/diff`,
`/focus`, `/density`, `/mouse`, `/animations`, `/theme`, `/compact`, `/autocompact`, `/skill`, `/skills`,
`/memory`, `/remember`, `/cost`, `/usage`, `/stats`, `/insights`, `/context`, `/queue`,
`/timestamps`, `/statusline`, `/preset`, `/permissions`, `/import`, `/cd`, `/pwd`, `/add_dir`,
`/config`, `/trust`, `/keymap`, `/init`, `/doctor`, `/debug-zip`, `/status`, `/clear`, `/rewind`, `/undo`,
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
  `{name, description, enabled?, hooks?}`; `hooks` takes the same keys as config hooks and
  runs after them; `enabled: false` skips.

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
- `hooks` in config.json — see [Hooks](#hooks).

## Hooks

Shell commands at four points of a turn, configured under `hooks` in `~/.tango/config.json`
or a trusted project's `.tango/config.json` (see "Project config and trust"; Claude Code's format;
global hooks run first, then project, then personal, then plugin hooks):

```json
{ "hooks": {
  "PreToolUse":       [{ "matcher": "bash", "command": "./guard.sh", "timeout": 10 }],
  "PostToolUse":      [{ "matcher": "edit|write", "command": "npm run -s lint" }],
  "UserPromptSubmit": [{ "command": "./no-secrets.sh" }],
  "Stop":             [{ "command": "./tests-green.sh" }]
} }
```

Each command runs as `sh -c` in the session's cwd with the event JSON on stdin (`hook_event_name`,
`session_id`, `cwd`, plus `tool_name` / `tool_input` / `tool_response`, `prompt`, or
`stop_hook_active` / `last_assistant_message`). `matcher` is an anchored regex over the tool name;
missing or `*` matches every tool. The nested `{matcher, hooks: [{type: "command", command}]}`
form works too. `timeout` is in seconds (default 10) and a timed-out hook is killed.

Exit 0 means go on. Exit 2 acts on the event, with stderr as the reason:

| Event | Exit 2 does |
| --- | --- |
| `PreToolUse` | the call is refused; the model sees `blocked: PreToolUse hook: <reason>` |
| `PostToolUse` | the tool already ran; the reason is appended to its result for the model |
| `UserPromptSubmit` | the prompt is rejected before it is saved or sent |
| `Stop` | the model keeps going with the reason as a note; `stop_hook_active` is true on the next stop, so a hook can let it finish; at most 3 times per prompt |

Any other exit code warns on stderr and never blocks. The older `beforeTool` / `afterTool` string
keys still work as before (notify only, never block).

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

Three rule kinds, persisted in `permissions.allow` and live via `/permissions` (which adds to the
project's personal `.tango/config.local.json`, or the global file outside a project):
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
- **Thinking bar**: committed reasoning (`✻ …` lines) carries a `┃` gutter on its left;
  while the turn runs, the newest thinking block's bar color scrolls down (a dim→bright
  cyan wave, ~8fps, in 24-bit color with `$COLORTERM=truecolor` and xterm 256 colors otherwise,
  and only the block that belongs to the running turn; older blocks and idle chats show a flat dim bar).
- **Running tools**: while a bash/other tool call is in flight its `⚙ …` line keeps the
  gear cycling through the same cyan wave (~2.5 steps/s); when the result lands, that same
  line freezes into `⚙ <tool> <summary> · <ms>` in place — one line per call, never a
  duplicate. Without truecolor the same wave runs in xterm 256 colors.
- **Input history**: Up/Down in the input box walk through sent prompts (empty input
  recalls the newest). The unsent draft is stashed aside and comes back when you press
  Down past the newest entry, or Esc — and after you edit a recalled entry and send it,
  the draft returns to the box. Inside a multi-line entry Up/Down move the cursor between
  lines first. With no history at all, Up/Down still scroll the chat.
- **Mouse basics** (Claude Code / OpenCode parity): drag in the chat to select — whole
  logical lines highlight and release copies them to the system clipboard; a click on a
  complete local file path (absolute, `./…`, `~/…`, `:line` suffix tolerated) opens the file
  in the read-only viewer; in the input box, a click moves the cursor there, drag selects text and release copies it, and a
  double-click selects the whole prompt and copies it (paste chips expand to the full pasted
  text), the highlight clearing on the next key; the permission modal takes `c` to copy the pending command before
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

- `/rewind` opens a timeline picker (one node per turn, newest on top with the cursor on it); choosing one rolls \*\*code + conversation
  - model context\*\* back to just before that turn. Smart delete removes agent leftovers (files that
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
  queued brief with its queue id and deletes expired ones (so briefs for sessions never reopened
  don't pile up); `/handoff cancel <queue id>` withdraws one. If the target is
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

- tool calls) into a draft under `~/.tango/skill-drafts/`. Edit the draft — above all write a real
  `description:` — then `/save-as-skill finish <file>` installs it as a project skill.

## Coding feedback loop

After every `write`/`edit` of a source file tango runs the project's _own_ checker and appends any
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

Other frames: `notice` (a chat-line message such as a skipped config file or goal status),
`error` (a failed command without a request id; `fatal: true` when serve cannot start), and
`trust_request` (`root`, `keys`, `files`: the project's config has trust-gated keys and no
decision yet; answer with `set_trust`).

The remaining commands, grouped. Fields in parentheses are the command's inputs; `→` names the
reply data. Most of these also skip the prompt queue.

| group | commands |
| --- | --- |
| settings | `set_effort` (`level`), `set_plan` (`on`), `set_autocompact` (`value`, 0 = off), `set_statusline` (`template`, null clears), `set_bubble` (`bubble`), `set_animations` (`animations`) |
| config and trust | `config_show` (→ merged `config`, `sources`, `line`), `trust_status` (→ `root`, `trusted`, `gated`, `line`), `set_trust` (`trusted` → `reload`: restart serve to apply) |
| permissions | `permissions_list` (→ `allow` with each rule's layer), `permissions_add` / `permissions_remove` (`rule` → `allow`, `path` of the file written), `import` (`source`: claude-code, grok or codex; `path` → `imported`, `skipped`) |
| directories | `cd` (`dir` → `cwd`), `pwd` (→ `cwd`), `add_dir` (`dir` → `extraDirs`, `path`) |
| background work | `bg` (`prompt`, `name`, `long`), `subtask` (`prompt`, `name`; runs in its own worktree), `tasks` (→ `tasks`), `stop_task` (task `id`) |
| worktrees | `worktree_create` (`name`, `base`), `worktree_list`, `worktree_merge` (`name`), `worktree_remove` (`name`, `force`) |
| goals | `goal` (`op`, `text`; → `goal`, `paused`, `maxRounds`), `loop` (`prompt`, `everySec`, or `off`) |
| review | `review` (reviews the working-tree diff), `security_review` (`paths`) |
| during a turn | `supplement` (`text`, `images`: adds to the running turn; the model sees it at its next step), `editlast` (`entryIndex` optional: interrupt, rewind to before that prompt, → its original text for re-editing) |
| sessions | `session_preview` (`session` → `preview`), `session_rename` (`session`, `name`), `queue` (→ `running`, `queued`) |
| memory and skills | `memory` (→ `text`, `path`), `remember` (`text`), `skills` (→ `skills`), `preset_save` / `preset_get` (`name`, `text`), `preset_list` (→ `presets`), `init` (`force`; writes AGENTS.md) |
| models and agents | `agents_show` (→ `roles`, `driver`), `agents_set` (`role`, `model`) |
| usage and diagnostics | `usage` / `stats` (requests and cost by model and session), `insights` (→ `path` of an HTML report), `doctor` (→ `checks`), `debug_zip` (→ `path`), `mcp_list` (→ `servers`), `plugins_list` (→ `plugins`) |

## TUI

`tui/` holds the Rust client (ratatui + crossterm): `cargo build --release` in `tui/`, then run
`tango` (`tango -c` / `tango -r <id>` open a previous session). It spawns `tango serve` and speaks the protocol above — fullscreen
alternate screen, mouse wheel scrolling, streaming answers, thinking lines, tool status lines with
durations, a centered permission modal (y/n/a), a bottom status bar below the input (model · mode ·
cost · session, fitted to the terminal width — on a narrow screen ctx and sess go first, then the
pinned ▶, and the model name shortens, while cost and the ❓ badge stay). The rule above the input
carries the work state on its left end — `idle`, or the elapsed time and
received characters behind a steady `●` — and
a rotating tip on its right end during long turns; on a narrow terminal the tip goes first, then the
counter, then the timer. While a turn runs, a cyan band of light flows along the rule's dashes: fast
while text streams, a slow drift while waiting on tools; after 10s with no output it stops and dims.
Terminals without 24-bit color get the same band (and the thinking bar's wave) in xterm 256 colors. `/animations off` (saved as
`"animations": false` in config) keeps the text and drops the motion: steady dot, no band, static
thinking bar and splash.
A custom `/statusline` template can still place `{work}` in the status bar. A prompt that starts with an existing file or directory path is sent as `文件：<path>` (or `目录：`) plus the rest of the text, or a request to read it when nothing follows; known slash commands still win. The input is multi-line with Tab-completed `/` commands and `@file` inlining (contents ride along in the
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
one rule: only `/exit` (or `/quit`) or a deliberate double Ctrl+C leaves the session. Ctrl+C is
staged: with text in the input it clears the input first; with an empty input it interrupts the
running turn (as often as you like, it never quits mid-turn); when idle, the first press shows
"再按一次 Ctrl+C 退出" on the input rule and a second one within 2s quits (any other key cancels);
Esc closes popups, else does the same (OpenCode-style). `TANGO_BIN` overrides the server binary (e.g. `TANGO_BIN="node bin/tango.js"`
for dev builds). Bare `/model` and `/resume` open centered pickers; picking a session kills the
serve child and respawns it with `TANGO_RESUME`, so both the model context and the visible
transcript come back. `/new` starts fresh while keeping the current model and permission mode.
`/restart` quits and re-execs the TUI (and with it a fresh serve), so a rebuilt `tango-tui` or
`dist/` takes effect; it resumes the current session with the same permission mode. It refuses
while a turn is running.
Ctrl+P opens a filterable command palette; model/session pickers use the same filtering and mouse
routing. Ctrl+R searches original prompt history, and Alt+S stashes/restores the complete draft
(including collapsed paste metadata) without submitting it. File references accept inclusive line
ranges: `@path#20-45`, `@path#L20-L45`, or `@path#20`. Scrolling up pauses auto-follow and keeps the
reading position stable; a centered ↓ pill on its own row above the input jumps back to the bottom
(and counts new lines), shrinking its label on narrow terminals; clicking it or Ctrl+End resumes
following. While scrolled up, a dim top row names the latest prompt above the view (it changes as
you scroll); click it to jump to that prompt. Ctrl+T opens a transcript with `/` search, `n/N` matches, `{`/`}` prompt navigation,
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
object remaps `expandTools`, `editLast` (Alt+E: interrupt and edit the last prompt), `newline`, `interrupt`, `clearInput`,
`scrollUp`, `scrollDown`, `pageUp`, `pageDown`, `promptPrev`, and `promptNext`, using values such as `ctrl+u` or `alt+enter`.
Unknown actions, keys, and editor values fall back to defaults. Ctrl+C always remains a safe
interrupt/clear binding even when `interrupt` is remapped. Example:

```json
{ "editor": "vim", "keys": { "expandTools": "ctrl+u" } }
```

## Permissions

Three modes, set with flags or `/mode`:

| mode       | flag           | behaviour                                                                                                                                   |
| ---------- | -------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `ask`      | (default)      | reads and writes inside cwd run; deletes, `git push`/`reset --hard`, `sudo`, `curl … \| sh`, writes outside cwd and unknown tools ask first |
| `auto`     | `-y` / `--yes` | everything runs                                                                                                                             |
| `readonly` | `--readonly`   | reads only; writes and shell are refused                                                                                                    |

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

## Past conversations

`session_search` greps the user's earlier sessions in `~/.tango/sessions` (user and assistant
text, compaction summaries and handoff briefs; not thinking or raw tool output) and returns
matching sessions newest first, with id, date, cwd, topic and up to three snippets. It skips the
calling session and counts as a read. The system prompt tells the model to call it whenever the
user refers to an earlier conversation ("did we talk about…", "之前/上次/我们聊过"), before saying
it can't remember.

## Office files

`office_read` extracts text from PDF, Word, Excel and PowerPoint with no npm dependencies.

| type                | how                                                                                              | options                                             |
| ------------------- | ------------------------------------------------------------------------------------------------ | --------------------------------------------------- |
| .docx               | built-in ZIP + XML reader; headings become `#`, lists `-`, tables markdown                       |                                                     |
| .xlsx               | tab-separated rows prefixed with the row number; dates converted from serials                    | `sheet` (name or index), `rows` (default first 500) |
| .pptx               | slides in presentation order, plus speaker notes                                                 | `pages` (slide range)                               |
| .pdf                | `pdftotext -layout` (poppler), else macOS PDFKit; says so when a PDF has no text layer (scanned) | `pages`                                             |
| .doc .rtf .odt .wps | macOS `textutil`                                                                                 |                                                     |

Old binary .xls/.ppt are refused with a hint to re-save as .xlsx/.pptx. `brew install poppler`
gives much better PDF output than the PDFKit fallback. Counts as a read for permissions.

`office_write` creates .docx (markdown-ish text: `#` headings, lists, `| tables |`, **bold**)
and .xlsx (tab-separated rows, or `[{name, rows}]` for several sheets — the same shape
`office_read` prints, so read → edit → write round-trips). No dependencies; the generated files
open in Word, WPS, Numbers and macOS textutil. Counts as a write for permissions.

## Config

All user state lives in `~/.tango/` (override with `TANGO_HOME`):

| file                     | purpose                                                                                                                                                  |
| ------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `config.json`            | default model (`/model <id>` writes it); `feishuWebhook` for notify/tasks; `theme`: `dark` (default), `light`, or `auto` from the terminal's `COLORFGBG` |
| `models.json`            | extra/overriding model specs, same shape as built-in presets                                                                                             |
| `tasks.json`             | scheduled tasks (`tango task add`)                                                                                                                       |
| `AGENTS.md`              | global instructions appended to the system prompt                                                                                                        |
| `skills/<name>/SKILL.md` | global skills (project-level: `<cwd>/.tango/skills/`)                                                                                                    |
| `sessions/*.jsonl`       | append-only sessions, one line per entry                                                                                                                 |
| `trusted.json`           | your trust decision per project (keyed by the main checkout), see below                                                                                  |

`apiKey` is either an environment variable name or the key itself. A value made only of uppercase
letters, digits and underscores (`"DEEPSEEK_API_KEY"`) is read from that variable; anything else is
used as the literal key. Nothing is inferred. `tango --models` shows which variable each preset reads.

### `config.json` keys

| key | what it does | set by |
| --- | --- | --- |
| `model` | default model id | `/model <id>` |
| `feishuWebhook` | Feishu/Lark bot webhook for `notify` and scheduled tasks (global file only) | edit |
| `theme` | `dark`, `light` or `auto` (from `COLORFGBG`) | `/theme` |
| `mouse` | mouse capture; `false` keeps native selection (default `true`) | `/mouse` |
| `animations` | busy-rule, thinking and splash motion (default `true`) | `/animations` |
| `bubble` | user messages as right-aligned bubbles (default `true`) or full width | `/bubble` |
| `editor` | `default` or `vim` input editing | `/vim` |
| `keys` | remap TUI actions, e.g. `{ "expandTools": "ctrl+u" }` (see "TUI") | edit |
| `nvimViewer` | `ask` or `readonly`: how Neovim opens read-only views | Neovim picker |
| `checkpoint` | `off` disables per-turn workspace snapshots (default on) | edit |
| `autocompact` | compact when context passes this share of the window (default `0.75`); `off` disables | `/autocompact` |
| `maxTurns` | model round-trips per prompt before tango stops the loop (1-500, default 64) | edit |
| `warming` | keep prompt caches warm: `true` or `{ "interval": 240, "duration": 1800 }` (see "Session warming") | edit |
| `statusline` | status bar template with `{model} {mode} {cost} {ctx} {pct} {session} {tokens}` | `/statusline` |
| `permissions` | `{ "allow": [rules] }` (see "Permissions rules"; trust-gated in project files) | `/permissions add` |
| `extraDirs` | extra writable directories (trust-gated in project files) | `/add_dir` |
| `hooks` | lifecycle hooks (see "Hooks"; trust-gated in project files) | edit |
| `roles` | models for `driver`, `subagent`, `longContext` | `/agents` |
| `mcpServers` | MCP servers `{ name: { command, args, env } }` (trust-gated in project files) | edit |

### Project config and trust

A session merges up to three config files, lowest first:

| layer | file | notes |
| --- | --- | --- |
| global | `~/.tango/config.json` | `/theme`, `/mouse`, `/animations`, `/model` write here |
| project shared | `<git toplevel>/.tango/config.json` | committed with the repo |
| project personal | `<main checkout>/.tango/config.local.json` | `/permissions add`, `/import`, `/add_dir` write here; tango adds it to `.git/info/exclude` |

CLI flags and environment variables still win over all three. Single values come from the
nearest layer. `permissions.allow`, `extraDirs` and each hook event's list add up across layers;
`mcpServers`, `roles` and `keys` merge by name. `feishuWebhook` and legacy `beforeTool`/`afterTool`
hooks are read only from the global file. A file with broken JSON is skipped with a notice.

`hooks`, `permissions`, `mcpServers` and `extraDirs` from the shared file run commands or widen
access, so they apply only after you trust the project. The TUI asks on the first start in such a
project and remembers the answer in `~/.tango/trusted.json`; `/trust yes|no` changes it later and
restarts serve. Until then the other keys still apply, and `/status` shows what was skipped.
Worktrees share the main checkout's trust and personal file. The personal file needs no trust
unless git tracks it. Project hooks get `$TANGO_PROJECT_DIR` (the git toplevel), so
`"command": "$TANGO_PROJECT_DIR/scripts/check.sh"` works from any subdirectory.

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
npm test            # test:ts (node) + test:tui (cargo test in tui/)
```

`npm test` needs a Rust toolchain because `test:tui` runs `cargo test` in `tui/`. Among those
tests is the slash-command sync check: TUI handlers, `/help`, `docs/handbook.md` and the command
list above must agree. Use `npm run test:ts` for the TypeScript tests alone.

### Docs coverage

Docs are part of every feature: other agents may compare tango by reading these docs alone.
`node scripts/docs-check.mjs` reads the source and lists every tool, CLI flag and subcommand,
config key, serve command and frame, env var, hook event and TUI key action that the docs
don't name (in backticks or a code block, in the section it belongs to). Slash commands are
checked by a cargo test against `SLASH`, the handbook reference and the list above.
`tests/docs-guard.test.ts` keeps coverage at zero gaps.

In this repo `scripts/hooks/docs-guard.mjs` also runs as project hooks (`.tango/config.json`).
`UserPromptSubmit` records where the turn started. At `Stop`, if the turn changed code under
`src/`, `tui/src/`, `scripts/` or `bin/`, it sends the model back while coverage has gaps, and
once more if no doc file (`README.md`, `AGENTS.md`, `docs/*.md` outside `docs/notes/`) changed.
After that one reminder the model may finish, for example by saying nothing user-visible changed.

### Adding a hook event

Every event in `HOOK_EVENTS` (`src/core/hooks.ts`) needs at least one end-to-end test in
`tests/hooks-e2e.test.ts` whose title starts with `<Event>:`. Ideally that means one blocking case
(exit 2) and one payload case using `tests/fixtures/hooks/record.sh`. `npm run test:hooks` checks
the coverage and then runs every hook test (`hooks`, `hooks-e2e`, `hook-guard`).

The repo enforces this on itself: `.tango/config.json` runs `scripts/hooks/hook-guard.mjs` as
project hooks. They apply once you trust the tango checkout (the TUI asks on first start, or
`/trust yes`):

```json
{ "hooks": {
  "PostToolUse": [{ "matcher": "edit|write", "command": "node \"$TANGO_PROJECT_DIR/scripts/hooks/hook-guard.mjs\"", "timeout": 10 }],
  "Stop":        [{ "command": "node \"$TANGO_PROJECT_DIR/scripts/hooks/hook-guard.mjs\"", "timeout": 120 }]
} }
```

After an edit to hook code, `PostToolUse` tells the model which events still lack a test. At the
end of a turn with unpushed hook changes, `Stop` runs the coverage check and all hook tests and
sends the model back while either fails. A passing run is cached by content hash in
`.git/hook-guard-ok`, so unchanged code is not retested.
