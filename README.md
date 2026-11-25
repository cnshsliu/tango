# tango

> it takes two — you and the agent.

A light agent harness for work and code. China-first: DeepSeek / Kimi / Qwen / GLM work
out of the box with a single API key, no proxy.

How to use it day to day: [docs/handbook.md](docs/handbook.md).

## Status

Core loop is working: agent loop, JSONL sessions with compaction, Anthropic + OpenAI-compatible
adapters, built-in tools (read / write / edit / bash / grep / find / ls / web_search / web_fetch /
office_read / office_write / notify / diff / session_search / process_logs / process_stop / process_list), a permission gate, AGENTS.md + skills context assembly,
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
|                                      | `baidu/ernie-5.5-flash-preview` (qianfan.baidubce.com, ERNIE 5.5 Flash Preview, 1M context)                              | `QIANFAN_API_KEY`           |
|                                      | `qwen/qwen3.8-max`, `qwen/qwen3.8-flash` (dashscope.aliyuncs.com)                                                         | `DASHSCOPE_API_KEY`         |
|                                      | `zhipu/glm-5.3`, `zhipu/glm-5.3-flash` (bigmodel.cn)                                                                      | `Z_API_KEY`                 |
|                                      | `zhipu-plan/glm-5.4`, `glm-5.3`, `-flash` (GLM Coding Plan, Anthropic 协议)                                               | `ZHIPU_CODING_PLAN_API_KEY` |
| International                        | `kimi-intl/kimi-k3.1-pro`, `kimi-intl/kimi-k3.1-pro-highspeed` (moonshot.ai)                                              | `KIMI_API_KEY`              |
|                                      | `qwen-us/qwen3.8-max`, `qwen-us/qwen3.8-flash` (dashscope-us)                                                             | `DASHSCOPE_US_API_KEY`      |
|                                      | `anthropic/claude-opus-5-5`, `anthropic/claude-haiku-5-5`                                                                 | `ANTHROPIC_API_KEY`         |
|                                      | `openai/gpt-6-astra`, `openai/gpt-6.1-terra`                                                                              | `OPENAI_API_KEY`            |
|                                      | `google/gemini-4-carbon`, `google/gemini-3.8-flash`                                                                       | `GEMINI_API_KEY`            |
|                                      | `xai/grok-4.7`, `xai/grok-build-0.1`                                                                                      | `XAI_API_KEY`               |
| Aggregator (one key, many vendors)   | `openrouter/…` — 10 free (`…:free`) + 18 paid flagships, e.g. `openrouter/baidu/ernie-5.5-flash-preview:free`             | `OPENROUTER_API_KEY`        |
| Local                                | `local/phai-a1-35b` (NovaMLX at 127.0.0.1:6590, text only, zero cost so it lists under "free")                            | `NOVA_API_KEY`              |

A source with its own platform keeps every model it serves in one group: OpenRouter's `:free`
variants sit next to its paid ones, so the suffix is what marks a free model (the model search
matches it — `ernie free` finds the free ERNIE). A zero-cost model with no platform, such as
`local/phai-a1-35b`, still collects into the virtual `free` group.

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
tango --format json [prompt...]   same, but JSON lines on stdout for scripts and CI
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
| `--resume <id>` | `-r` | continue a given session (id, id prefix, or the id's last `-` part, e.g. `0ad2040a8182`) |
| `--model <id>` | `-m` | model for this run (fuzzy: `glm54` finds `zhipu-plan/glm-5.4`) |
| `--yes` | `-y` | auto mode: tools run without asking |
| `--readonly` | | no writes, no bash |
| `--cwd <dir>` | | working directory for the session |
| `--format json\|text` | | one-shot output: JSON lines (see "Headless JSON") or text (default) |
| `--max-tool-rounds <n>` | | cap model→tool rounds for this run (1-500); overrides `maxTurns` |
| `--models` / `--sessions` | | list models with key status / list sessions |
| `--help` / `--version` | `-h` / `-V` | usage / version |

### Headless JSON

`tango --format json "<prompt>"` prints one JSON object per line on stdout and nothing else
(warnings stay on stderr):

| `type` | fields |
| --- | --- |
| `start` | `session`, `model`, `cwd` |
| `step_start` | `step` (1-based model request number) |
| `text` | `step`, `text` (the assistant's text for that step) |
| `tool_use` | `step`, `id`, `name`, `input`, `output` (cut to 2000 chars), `isError`, `ms` |
| `step_finish` | `step`, `stopReason`, `usage` (`input`, `output`, `cacheRead`, `cacheWrite`), `costUsd` |
| `retry` | `step`, `attempt`, `max`, `reason` (e.g. `HTTP 503`, `connection reset [ECONNRESET]`), `delayMs`: the request failed before any output and is retried (see `retries` under "Config") |
| `error` | `step`, `message` (e.g. the `--max-tool-rounds` cap was hit, or the model call failed) |
| `done` | `session`, `stopReason` (`stop`, `aborted`, `error`), `steps`, `toolCalls`, `usage` totals, `costUsd`, `ms` |

`done` is always the last line. Exit code: 0 on success, 1 on `error`, 130 when interrupted. In
one-shot runs permission asks are refused, as in text mode, unless `--yes` is given. Example:
`tango --format json --max-tool-rounds 10 -y "run the tests" | jq -c 'select(.type=="done")'`.

TUI commands: `/model`, `/agents`, `/resume`, `/back`, `/forward`, `/new`, `/restart`, `/rename`, `/send`, `/mode`, `/plan`, `/goal`,
`/loop`, `/effort`, `/review`, `/security-review`, `/bg`, `/tasks`, `/stop`, `/subagent`,
`/worktree`, `/mcp`, `/plugins`, `/vim`, `/nvim`, `/editor`, `/multiline`, `/btw`, `/diff`,
`/focus`, `/density`, `/mouse`, `/animations`, `/ascii`, `/clipboard`, `/theme`, `/compact`, `/autocompact`, `/skill`, `/skills`,
`/memory`, `/remember`, `/cost`, `/usage`, `/stats`, `/insights`, `/context`, `/queue`,
`/timestamps`, `/statusline`, `/preset`, `/permissions`, `/import`, `/cd`, `/pwd`, `/add_dir`,
`/config`, `/trust`, `/keymap`, `/init`, `/doctor`, `/debug-zip`, `/status`, `/clear`, `/rewind`, `/undo`,
`/redo`, `/fork`, `/handoff`, `/takeover`, `/branch`, `/tree`, `/find`, `/export`, `/transcript`, `/copy`, `/recap`,
`/help`, `/exit`. `/model <id>` also saves the choice as the default for new sessions. Model ids
match fuzzily everywhere (`-m` too): case-insensitive subsequence — `glm54` finds `zhipu-plan/glm-5.4`,
`zp glm5.4` needs every word to match, several matches list the candidates — and the pickers' filters
(including the model picker's) match the same way.

## Worktrees, background, subagents, MCP, plugins

- **/worktree** — managed git worktrees (`~/.tango/worktrees/<name>`, branch `tango/<name>`):
  the isolation unit for anything that writes in parallel. Writes under `~/.tango/worktrees/` never
  trigger the "writes outside cwd" prompt, whatever the session's cwd. Dirty worktrees refuse removal;
  `/worktree merge <name>` merges back into the main workspace (which must be clean). A bare
  subcommand (`/worktree new`) prints its usage; `/worktree` alone lists (or picks) worktrees.
  `/worktree rm <name>` removes one. It is refused while the worktree has uncommitted changes and
  while a live session is working in it — the error names that session; `/cd` it to another
  directory (or close it) first. Never delete the directory a session works in by hand either:
  a session that is open there is stuck until it `/cd`s to a directory that exists. A session
  resumed after its directory is gone runs in the directory tango was started in instead, and
  says so in a chat line (`/cd` picks another; until then the session keeps its recorded
  directory, so the next resume says so again). A terminal still sitting in the deleted directory
  starts tango without a working directory: the CLI moves to the home directory first and passes
  the note to serve as `TANGO_LAUNCH_NOTICE`, shown as a notice frame after `hello`.
- **/bg <prompt>** — a background prompt: fresh worktree + independent serve child; the task
  runs to completion (detached), its session lands in the shared sessions dir (`/tasks` shows
  status + session, `/stop` kills the process group). `/bg --long` uses the longContext role.
- **/subagent <task>** — a subagent: a background task on the subagent role model whose final
  summary is written back into THIS session (`⑂ subagent …`); main turns are never disturbed.
  Its dispatch line and the `⑂ subagent … 完成：…` write-back are drawn in a violet tone (also after
  `/resume`) so they stand apart from the main agent's replies.
  The old name `/subtask` still works (the serve command is still `subtask`). The task registry
  (`~/.tango/tasks-bg.json`) records the parent session (`parent`) and the write-back (`reported`), so
  the summary shows within ~2s of the task ending, survives `/restart`, and is written once. Mid-turn
  the notice (ending `· 本回合结束后写入会话`) and `SubagentStop` come at once; the summary enters the
  session (what the model sees) only after the turn ends.
- **/agents [role model]** — role-based routing: `driver` (live), `subagent` (/subagent, /bg),
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
  execute. Shift+Tab in the TUI toggles it too (ask ↔ plan only; `auto` stays an explicit
  `/mode auto`). The input block's bar and info row turn orange while plan is on.
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

Shell commands at 17 points of a session (the Claude Code / grok-cli event set), configured under
`hooks` in `~/.tango/config.json` or a trusted project's `.tango/config.json` (see "Project config
and trust"; Claude Code's format; global hooks run first, then project, then personal, then plugin hooks):

```json
{ "hooks": {
  "PreToolUse":       [{ "matcher": "bash", "command": "./guard.sh", "timeout": 10 }],
  "PostToolUse":      [{ "matcher": "edit|write", "command": "npm run -s lint" }],
  "UserPromptSubmit": [{ "command": "./no-secrets.sh" }],
  "Stop":             [{ "command": "./tests-green.sh" }],
  "SessionStart":     [{ "command": "./project-notes.sh" }],
  "PreCompact":       [{ "matcher": "auto", "command": "./save-transcript.sh" }],
  "Notification":     [{ "command": "osascript -e 'display notification \"tango needs you\"'" }]
} }
```

Each command runs as `sh -c` in the session's cwd with the event JSON on stdin: `hook_event_name`,
`session_id`, `cwd`, plus the event's own fields below. `matcher` is an anchored regex over the
event's match field; missing or `*` matches everything. The nested
`{matcher, hooks: [{type: "command", command}]}` form works too. `timeout` is in seconds (default
10) and a timed-out hook is killed.

| Event | When | Fields (match field first) |
| --- | --- | --- |
| `PreToolUse` | before a tool call, ahead of the permission gate | `tool_name`, `tool_input` |
| `PostToolUse` | after every tool call, refused ones included | `tool_name`, `tool_input`, `tool_response` (`text`, `is_error`) |
| `PostToolUseFailure` | after a call that failed (refused, crashed, or reported an error), next to PostToolUse | `tool_name`, `tool_input`, `error` |
| `UserPromptSubmit` | before your prompt is saved or sent | `prompt` |
| `Stop` | the model is about to finish (no tool calls) | `stop_hook_active`, `last_assistant_message` |
| `StopFailure` | the turn died on an error (model/API failure, the tool-round cap) | `error` |
| `SessionStart` | once per opened session, just before its first prompt | `source` (`startup` or `resume`) |
| `SessionEnd` | a session that ran a prompt closes (quit, `/new`, `/resume`, `/restart`, end of a one-shot or piped run) | `reason` (`exit`, `new`, `resume`, `restart`, `eof`, `sigterm`) |
| `PreCompact` | before a compaction | `trigger` (`manual` = `/compact`, `auto` = the context threshold) |
| `PostCompact` | after a compaction | `trigger`, `summary` |
| `InstructionsLoaded` | the AGENTS.md set is first read, and whenever it changes (`/cd`) | `files_loaded`, `files` (paths, global first) |
| `CwdChanged` | `/cd` moved the working directory; runs in the new one | `old_cwd`, `new_cwd` |
| `Notification` | tango needs you: a permission prompt is about to show | `notification_type` (`permission_prompt`), `message` |
| `SubagentStart` | before `/subagent` starts its worktree child | `agent_type` (`subtask`), `description` (the prompt) |
| `SubagentStop` | a `/subagent` finished (seen at the next safe point) | `agent_type`, `description`, `task_id`, `success`, `status` (`done`, `failed`, `stopped`) |
| `TaskCreated` | before `/bg` starts a background task | `agent_type` (`bg`), `description` |
| `TaskCompleted` | a `/bg` task this session started stopped running (polled every 2s) | `agent_type`, `description`, `task_id`, `success`, `status` |

Exit 0 means go on. Exit 2 acts on the event, with stderr as the reason:

| Event | Exit 2 does |
| --- | --- |
| `PreToolUse` | the call is refused; the model sees `blocked: PreToolUse hook: <reason>` |
| `PostToolUse`, `PostToolUseFailure` | the tool already ran; the reason is appended to its result for the model |
| `UserPromptSubmit` | the prompt is rejected before it is saved or sent |
| `Stop` | the model keeps going with the reason as a note; `stop_hook_active` is true on the next stop, so a hook can let it finish; at most 3 times per prompt |
| `PreCompact` | the compaction is skipped (`/compact` reports nothing compacted) |
| `SubagentStart`, `TaskCreated` | `/subagent` or `/bg` is refused with the reason |
| all others | nothing can be undone or refused there; the reason is a warning on stderr |

Any other exit code warns on stderr and never blocks. The first block stops the chain of hooks
for that event.

On exit 0 a hook can print one JSON object on stdout (other output is ignored):

| Field | Effect |
| --- | --- |
| `"decision": "block"`, `"reason"` | same as exit 2 with `reason` as stderr; `"approve"` is accepted and changes nothing |
| `"additionalContext": "…"` | text for the model: PostToolUse / PostToolUseFailure append it to the tool result, other events add it as a `[hook context]` note before the next model request (SessionEnd and Notification ignore it) |
| `"continue": false`, `"stopReason"` | ends the turn after the current step; the turn's last message is `[hook] <stopReason>` (from UserPromptSubmit it rejects the prompt, from PreToolUse it also refuses the call, from SubagentStart / TaskCreated it refuses the launch; from Stop it lets the model finish) |

The TUI sends serve a `shutdown` frame before it closes a session and waits up to 3s for
SessionEnd hooks; a killed serve (SIGKILL) skips them. The older `beforeTool` / `afterTool` string
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
Rules never override readonly mode; they only turn "ask" into "allow". Answering `a`
(始终允许) in a permission dialog creates such rules for the pending call — `bash:git
branch` for `git branch -D x`, the directory for an outside write, `tool:<name>` otherwise — and
allows the call (it no longer flips the session to auto mode). A command line with several
`; && || |` segments gets one rule per segment: `osascript … && rm -f x` saves `bash:osascript`
and `bash:rm`, because a rule for the first command alone leaves the risky tail uncovered and the
same line would ask again every time. The dialog's hint names what `a` remembers
(`a 始终允许（记住 osascript · rm）`) before you press it. Lines split segments too, and `;`, `|`
or `&` inside quotes do not, nor does the `&` of `2>&1`; `NAME=value` assignments, shell keywords (`for … do … done`,
`if … then … fi`, `{ … }`) and heredoc bodies (up to their terminator line, even when commands
follow it) are not commands, so a script written with `cat > f <<'EOF'` never turns its code into
rules. Since `bash:rm` then allows any `rm`, prefer `y` for one-offs.
Rules are live, not a snapshot from startup: they are re-read whenever a config file changes, so
an `a` pressed in one window applies to every open session from its next decision on, and a rule
that is removed stops applying just as fast. They follow the directory the session is in: after
`/cd` the rules of the new directory apply and the old ones stop. (Hooks and MCP stay those of
the directory the session opened in; `/restart` re-reads those. A project's shared and personal
layers still need trust first.)
`/import claude-code|grok|codex` maps another agent's allowed-tools into these rules and
reports every entry it could not represent (codex reads trusted `command = "…"` entries from
`~/.codex/config.toml`, loose TOML scan, `path:` override for testing). Bash rules cover only
the command segments they match: a risky command later in the same line (`git status; rm …`)
still asks unless its own command word is allowed too. `/export` and `/insights` write into `~/.tango/exports/` (never dirty the repo). `/cd <dir>` moves the session working directory (agent,
tools, checkpoints and permission rules follow live). Bare `/cd` opens the directories used before; `/cd /path`
or `/cd ~/path` switches directly. A prompt does not start when that directory is gone:
tango opens the same picker instead of calling the model. `/add_dir <dir>` registers an extra writable root.

## Observability

- `/context` — what the next request carries: system prompt / skills / tool schemas / history as
  estimated tokens, with a share bar and 60/80% warnings.
- `/queue` — the running prompt and the ones queued behind it (ctrl+c drains the queue).
- `/usage` / `/stats` — cost and token aggregates across ALL sessions, by model and by session;
  `/insights` writes the same data as an offline single-file HTML report.
- `/timestamps` toggles `[HH:MM]` prefixes; `/density 0|1|2` sets tool-detail level (compact /
  default: one line per call, failures included / full: every call's output previewed inline) and applies to restored history too.
- **Thinking blocks**: committed reasoning is kept whole (no truncation) as a block headed by `🤖`
  (the icon upright, the reasoning in dim italics)
  (an emoji, 2 cells; terminals without an emoji font can switch to `/ascii`).
  Up to 3 lines / 240 chars show in full; longer blocks show a 3-line preview ending in
  `… 双击展开思考（还有 N 字 · 共 M 行）` on its last line — the gesture is named on the first
  collapsed block only, later ones end in just `…（还有 N 字 · 共 M 行）`. A double-click on the block expands / collapses it
  in place; Ctrl+T's transcript always has the full text.
- **ASCII mode** (`/ascii`, config `ascii`): 🤖 becomes `*` and emoji in chat, the ❓ panel,
  `/todo` and the status bar become ASCII (`❓`→`?`, `➕`→`+`, `📋`→`#`, `📂`→`>`, other emoji →`*`),
  for terminals that show tofu boxes or misalign emoji.
- **Thinking bar**: committed reasoning (robot-headed blocks) carries a `┃` gutter on its left;
  while the turn runs, the newest thinking block's bar color scrolls down (a dim→bright
  cyan wave, ~8fps, in 24-bit color with `$COLORTERM=truecolor` and xterm 256 colors otherwise,
  and only the block that belongs to the running turn; older blocks and idle chats show a flat dim bar).
- **Running tools**: while a bash/other tool call is in flight its `⚙ …` line keeps the
  gear cycling through the same cyan wave (~2.5 steps/s); when the result lands, that same
  line freezes into `⚙ <tool> <summary> · <ms>` in place — one line per call, never a
  duplicate. Without truecolor the same wave runs in xterm 256 colors. The command is fitted
  to one row of the current chat width (refitted when the terminal resizes): its first line, cut
  with `…` when it doesn't fit, plus `（共 N 行）` for multi-line commands; the
  viewer (double-click / Ctrl+O) shows the whole command as `$ <command>` above the output,
  and Ctrl+T's transcript keeps it too. A call killed by your interrupt (Esc / Ctrl+C) ends
  in `[interrupted]` (not `[exit null]`) and shows in the warning tone with `· 已中断`, not red. A failed call
  (error result, non-zero exit) is likewise a single warning-tone line ending `· 出错 · 双击查看`;
  its output opens in the viewer (double-click / Ctrl+O), never as an inline red block.
- **Input history**: Up/Down in the input box walk through sent prompts (empty input
  recalls the newest). The unsent draft is stashed aside and comes back when you press
  Down past the newest entry, or Esc — and after you edit a recalled entry and send it,
  the draft returns to the box. Inside a multi-line entry Up/Down move the cursor between
  lines first. With no history at all, Up/Down still scroll the chat.
- **Mouse basics** (Claude Code / OpenCode parity): drag in the chat to select — whole
  logical lines highlight and release copies them to the system clipboard. In the chat a
  single click only anchors a selection; a **double-click** acts — on a complete local file
  path (absolute, `./…`, `~/…`, `:line` suffix tolerated) it opens the file in the read-only
  viewer, on tool/thinking/prompt lines it opens the viewer, toggles the block or pops the
  prompt menu (mis-clicks stay harmless); in the input box, a click moves the cursor there, drag selects text and release copies it, and a
  double-click selects the whole prompt and copies it (paste chips expand to the full pasted
  text), the highlight clearing on the next key; the permission modal takes `c` to copy the pending command before
  answering y/n/a. `/mouse off` still hands selection back to the terminal natively.
- **Clipboard** (`/clipboard auto|osc52|native`, config `clipboard`): every copy (selection,
  `/copy`, viewer `c`, prompt menu, session id) goes through one path and confirms with a toast
  (`✓ 已复制 N 个字符到剪贴板`) in the chat's top-right corner that disappears after 2 seconds; only a
  failed copy is written into the chat. `auto` (default) sends an
  OSC 52 escape (`ESC ]52;c;<base64> BEL`) when `$SSH_TTY`/`$SSH_CONNECTION` is set, so text lands in
  the clipboard of the machine you're sitting at; locally it runs `pbcopy` (or `wl-copy`/`xclip`/`xsel`)
  and falls back to OSC 52 if none works. Inside tmux the sequence is wrapped in tmux's DCS
  passthrough (needs `set -g set-clipboard on` or `allow-passthrough on`), inside GNU screen in
  screen's DCS. `osc52` / `native` force one path. The terminal must allow OSC 52 writes (iTerm2:
  "Applications in terminal may access clipboard"; most others on by default).
- **IME auto-switch (macOS)**: single-key dialogs (y/n/a) are unreachable through a CJK
  input method. When `macism` is on PATH, a permission dialog switches to ABC automatically
  and restores your previous input method when you answer; already-roman keyboards and a
  missing macism are left untouched.

- **Prompt queue** (while a turn runs): Enter enqueues locally (sent in order when the turn
  ends); `/queue` opens a clickable dialog (per-item [并入当前]/[删], footer [全部并入]/[清空]);
  it also works from the keyboard: ↑↓/`j` `k` or `1`–`9` select a row, Enter/`m` merges it, `d`/Delete
  removes it, `a` merges all, `c` clears, Esc/`q` closes. The dialog closes by itself once the
  queue is empty (merge all, clear, or the last item merged, removed or sent). `/todo` (pending ❓ choices and
  suggestions) works the same way: ↑↓/`j` `k`/`1`–`9` pick a row, Enter acts on it (展开 / 执行 / 答复), `d`/Delete
  drops it, Esc/`q` closes; the row buttons stay clickable. `/queue` also accepts `rm <n>`/`all`/`clear`;
  `/queue auto append` makes busy-Enter supplement the running turn instead of queueing (Alt+Enter
  semantics), `/queue auto off` restores queueing, bare `/queue auto` prints the current mode; the
  switch lives in the TUI process (not persisted, kept across serve respawns until you quit); **Ctrl+C interrupts only the current reply and keeps the queue**; **Alt+Enter appends the typed
  text to the RUNNING turn** ("forgot to mention…") — it rides the same turn's next model
  request as a `（补充）…` user message. The status bar pins the active ask (▶) and each
  finished turn leaves a `✓ 本轮：…` recap line: the answer's whole first line, never cut (long ones wrap).
- `/autocompact [N%|0.N|off]` shows or sets the auto-compaction threshold (default 75% of the
  window); persisted in config.
- **Compaction** (`/compact` or auto): the newest whole turns, up to ~20 000 estimated tokens
  (at most 25% of the window), stay in context verbatim; only the older part is summarized into
  fixed sections (`## Goal`, `## Constraints`, `## Progress`, `## Key Decisions`, `## Next Steps`,
  `## Critical Context`). A later compaction updates the previous summary instead of
  re-summarizing it. Tool results going into the summarizer are cut to 2000 characters. The
  kept tail always starts at a prompt, so tool calls keep their results; when the last turn
  alone is over budget, its first half (cut before an assistant message) is summarized and the
  second half stays. A small conversation keeps just its latest turn. The summary may use up to
  4000 output tokens. Sessions compacted before this keep their old meaning (nothing verbatim).
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
Commands: `prompt`, `btw` (ephemeral, tool-free side question), `interrupt`, `shutdown` (`reason`:
abort any turn, run SessionEnd hooks, reply, exit — sent by the TUI before it closes a session), `permission`
(answers a `permission_request` frame by request id), `models` (list presets with key availability),
`set_model` (also saved as the default), `set_mode`, `set_nvim_viewer` (persist `ask`/`readonly`),
`set_theme`, `set_mouse`, `diff` (read-only working-tree/staged patch), `timeline` (turn list with
snapshot flags), `rewind`/`undo`/`redo` (time travel; refused while a turn runs), `fork`/`branch`/
`tree` (session forking and ancestry), `send` (`session`, `text` → `to`, `targetLive`), `handoff` / `takeover` (`session`, `note?` → `brief`; handoff also → `queueId`), `handoff_list` (→ `handoffs`), `handoff_cancel` (`queueId`), `find` (search all sessions incl. abandoned futures),
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
`retry` (`attempt`, `max`, `reason`, `delayMs`, `text`: a model request failed before any output
and is retried after `delayMs`; the TUI prints `text` as a muted line),
`error` (a failed command without a request id; `fatal: true` when serve cannot start),
`cwd_missing` (`cwd`, `dirs`: the session directory is gone; `dirs` are directories the user
has used that still exist; the TUI opens a picker, and `/cd` or a typed `/路径` switches), and
`trust_request` (`root`, `keys`, `files`: the project's config has trust-gated keys and no
decision yet; answer with `set_trust`).

The remaining commands, grouped. Fields in parentheses are the command's inputs; `→` names the
reply data. Most of these also skip the prompt queue.

| group | commands |
| --- | --- |
| settings | `set_effort` (`level`), `set_plan` (`on`), `set_autocompact` (`value`, 0 = off), `set_statusline` (`template`, null clears), `set_bubble` (`bubble`), `set_animations` (`animations`), `set_ascii` (`ascii`), `set_clipboard` (`clipboard`: auto, osc52 or native) |
| config and trust | `config_show` (→ merged `config`, `sources`, `line`), `trust_status` (→ `root`, `trusted`, `gated`, `line`), `set_trust` (`trusted` → `reload`: restart serve to apply) |
| permissions | `permissions_list` (→ `allow` with each rule's layer), `permissions_add` / `permissions_remove` (`rule` → `allow`, `path` of the file written), `import` (`source`: claude-code, grok or codex; `path` → `imported`, `skipped`) |
| directories | `cd` (`dir` → `cwd`, `branch`; `~` expands), `pwd` (→ `cwd`), `cwds` (→ `dirs`, directories used before that still exist), `add_dir` (`dir` → `extraDirs`, `path`) |
| background work | `bg` (`prompt`, `name`, `long`), `subtask` (`prompt`, `name`; runs in its own worktree), `tasks` (→ `tasks`), `stop_task` (task `id`) |
| worktrees | `worktree_create` (`name`, `base`), `worktree_list`, `worktree_merge` (`name`), `worktree_remove` (`name`, `force`) |
| goals | `goal` (`op`, `text`; → `goal`, `paused`, `maxRounds`), `loop` (`prompt`, `everySec`, or `off`) |
| review | `review` (reviews the working-tree diff), `security_review` (`paths`) |
| during a turn | `supplement` (`text`, `images`: adds to the running turn; the model sees it at its next step), `editlast` (`entryIndex` optional: interrupt, rewind to before that prompt, → its original text for re-editing) |
| sessions | `session_preview` (`session` → `preview`), `session_rename` (`session`, `name`), `session_resolve` (`session` id, prefix or last `-` part → `id`; fails on no match or an ambiguous prefix), `queue` (→ `running`, `queued`) |
| memory and skills | `memory` (→ `text`, `path`), `remember` (`text`), `skills` (→ `skills`), `preset_save` / `preset_get` (`name`, `text`), `preset_list` (→ `presets`), `init` (`force`; writes AGENTS.md) |
| models and agents | `agents_show` (→ `roles`, `driver`), `agents_set` (`role`, `model`) |
| usage and diagnostics | `usage` / `stats` (requests and cost by model and session), `insights` (→ `path` of an HTML report), `doctor` (→ `checks`), `debug_zip` (→ `path`), `mcp_list` (→ `servers`), `plugins_list` (→ `plugins`) |

## TUI

`tui/` holds the Rust client (ratatui + crossterm): `cargo build --release` in `tui/`, then run
`tango` (`tango -c` / `tango -r <id>` open a previous session). It spawns `tango serve` and speaks the protocol above — fullscreen
alternate screen (every frame is sent as one DEC 2026 synchronized update, `CSI ?2026h … ?2026l`,
so supporting terminals swap whole frames and streaming or animation never flickers; others ignore
it). The TUI also reports its state to the terminal with OSC 7501 ([Program Status
Protocol](https://www.superlogical.com/rex/docs/build/program-status), `app=tango`), sent only on
change: `idle` at the input box (including a collapsed `?choice` panel), `working` while a turn
runs, `blocked:kind=permission` while the y/n/a dialog is open, `blocked:kind=question` while a
`?choice` panel is open, and `clear` while Neovim owns the terminal and on exit. Orca uses this to
recognize a tango pane and to type `orca orchestration check` mail notes only when tango is at its
prompt; terminals that don't know the sequence ignore it. A `?choice` block from the model opens a
pick panel (↑↓/click, Enter, or an option number + Enter); Tab answers in your own words — it fills
the input with the ask as a lead-in ending in `": "` (关于…，模型在等你选择: …) in the conversation's
language, and only what you write after it is sent. Also: mouse wheel scrolling, streaming answers, thinking lines, tool status lines with
durations, a centered permission modal (y/n/a), and an input block. Every dialog's bottom hint is
clickable: each key in it (`y 允许`, `esc 关闭`, `enter 确认`, `↑`/`↓`, `ctrl+e` …) is a button that does
what pressing the key does; words that aren't keys (`输入`, `滚轮`) stay plain. The input block has a tinted background (its own
shade in the light theme) with a left bar colored by risk — plan orange, auto red, readonly gray,
ask blue (plan wins) — the draft, and an info row `mode · model provider` (e.g.
`ask · claude-opus-5-5 apikeyfan`; a narrow screen drops the provider, then clips the model).
Clicking the model there opens the model picker (same as `/model`) with the current model selected;
clicking the mode label opens the mode picker (same as `/mode`) on the current mode. Clicking the
same label again closes its picker, clicking the other one swaps to that picker, and a click on a
picker row picks it.
The draft starts right under the rule, with no blank padding rows around it. Under the block, on
terminals of 24+ rows, a footer shows `~/dir:branch` on the left (refreshed by `/cd`) and key hints
(`shift+tab plan`, `ctrl+p commands`) on the right when they fit. Both hints are clickable: `shift+tab plan`
opens the mode picker (like clicking the mode label), `ctrl+p commands` the command palette (like ctrl+p). The bottom status bar (cost ·
session · ctx, fitted to the terminal width — on a narrow screen ctx and sess go first, then the
pinned ▶, while cost and the ❓ badge stay) no longer repeats the model and mode. Session ids are shown
short everywhere (status bar, `/send`, handoff/takeover, mail notices, `/tasks`, `tango --sessions`): a uuid's
last `-` part, e.g. `0ad2040a8182`, which every id-taking command accepts; clicking `sess …` copies the full id. The rule above the input
carries the work state on its left end — `idle`, or the elapsed time and
received characters behind a steady `●` — and
a rotating tip on its right end during long turns; on a narrow terminal the tip goes first, then the
counter, then the timer. While a turn runs, a cyan band of light flows along the rule's dashes: fast
while text streams, a slow drift while waiting on tools; after 10s with no output it stops and dims.
Terminals without 24-bit color get the same band (and the thinking bar's wave) in xterm 256 colors. `/animations off` (saved as
`"animations": false` in config) keeps the text and drops the motion: steady dot, no band, static
thinking bar and splash.
An empty session shows a splash: the centered `tango` wordmark in block letters (cyan gradient with
a twinkle), a dancer and a hint line. The first message drops the dancer and hint but keeps the
wordmark as the first line of the chat, pinned to the top with blank rows under it, so a screenshot
holds both the logo and the first exchange; it scrolls away as the chat grows. It is display-only (not
in the session file, transcript or exports), cannot be selected or clicked, and is skipped on terminals
under 20 rows or 30 columns.
A custom `/statusline` template can still place `{work}` in the status bar. A prompt that starts with an existing file or directory path is sent as `文件：<path>` (or `目录：`) plus the rest of the text, or a request to read it when nothing follows; known slash commands still win. The input is multi-line with Tab-completed `/` commands (also while a turn runs) and arguments (`/mode`, `/effort`, `/model`, `/send`, `/resume`: Tab inserts the first match, each further Tab the next one, Shift+Tab goes back, zsh-style) and `@file` inlining (contents ride along in the
prompt, 6KB cap), fences render as indented gray blocks, and long pasted prompts collapse to a
short summary (over 600 chars or 8 lines). Ctrl+O opens the latest full prompt/tool output in an
independent scrollable viewer; double-clicking a specific summary/tool line opens that item. That viewer
is always the built-in dialog (80% of the screen, ↑↓/PgUp/PgDn/wheel scroll, `y` copies the whole text to
the clipboard, Esc closes), never Neovim, whatever `nvimViewer` says; Ctrl+E inside it still hands the
item to Neovim. Closing the viewer restores the original chat position. All overlays use the shared `tui/src/dialog.rs` frame:
rounded complete borders, a themed surface and title background, bottom key hints, and content
confined to the inner rectangle. A modal dialog (a permission
prompt, the full-access prompt, a picker, the viewer, `/queue`, `/todo`, the message menu) stands
on a scrim: the rest of the screen is dimmed behind it — near-black `#0b0c10` under a `#23272f`
surface in the dark theme, gray `#dfe1e4` under `#fbfbf9` in the light one — so it stands out
whatever the terminal's own background is. The completion list and the `?choice` panel are not
modals and dim nothing. Focus is viewer → permission → picker → completion → chat; modal
scrolling never leaks into the chat, pickers support wheel/click, and Esc closes the focused layer.
Other full-content views (session previews and the like) check `nvim` and `vim` with `--version`; only an actual `NVIM` is used,
never standard Vim. On first detection, choose one-time read-only, persistent read-only
(`nvimViewer: "readonly"`), edit, or the internal viewer — picking the internal viewer persists it
(`nvimViewer: "internal"`, no external editor at all), and that mode is the way back from
`readonly` without hand-editing `config.json`. In the internal viewer, Ctrl+E edits the
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
is **mouse-clickable** — clicking it in the input box, or double-clicking it in the chat transcript,
  opens the image in the system viewer (macOS `open` / Linux `xdg-open`). On submit the image rides the next prompt as a
base64 upload (`images` field → `ImageBlock` → vendor `image_url`/`image` content), with the chip text
`[图片 #N]` left in the message so the model knows where it sits; queued and Alt+Enter-supplemented
prompts carry their images too. Resumed history replays images as double-clickable `[图片 <path>]` chips.
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
Esc closes popups, else does the same (OpenCode-style): with a dialog open (picker, completion list, `/todo`, `/queue`, a `?choice` panel), the first Esc only closes it, even mid-turn, and the next Esc interrupts. `TANGO_BIN` overrides the server binary (e.g. `TANGO_BIN="node bin/tango.js"`
for dev builds). Bare `/model` and `/resume` open centered pickers (`/resume <id or prefix>` switches directly); picking a session kills the
serve child and respawns it with `TANGO_RESUME`, so both the model context and the visible
transcript come back. `/new` starts fresh while keeping the current model and permission mode.
`/back` and `/forward` step through the sessions this TUI switched between, browser-style
(every `/resume`, picker pick, `/new` and `/fork` pushes the session it left; a new switch clears
forward history; kept in the TUI process only), so `/resume`-ing a subagent's session and then
`/back` returns to the parent; the welcome line after a switch names the `/back` target.
`/restart` quits and re-execs the TUI (and with it a fresh serve), so a rebuilt `tango-tui` or
`dist/` takes effect; it resumes the current session with the same permission mode. It refuses
while a turn is running.
Shift+Tab toggles plan mode (ask ↔ plan). Ctrl+P opens a filterable command palette; model/session pickers use the same filtering and mouse
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
Set `theme` in `~/.tango/config.json` to `dark`, `light`, or `auto`; `auto` asks the terminal for
its background color at startup (OSC 11, with a DA1 query so terminals that don't answer cost no
wait; 300 ms cap) and picks light when the color's brightness is above the midpoint. Terminals that
don't answer fall back to `COLORFGBG` (backgrounds 7, 15 and 255 are light); otherwise dark. The
answer is read once per launch: after switching the terminal's colors, `/restart` re-detects.
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
interrupt/clear binding even when `interrupt` is remapped, and Shift+Enter always inserts a
newline. Shift+Enter needs the Kitty keyboard protocol: at startup the TUI asks the terminal
(`CSI ? u`, in the same round trip as the OSC 11 background query) and, when it answers (kitty,
WezTerm, Ghostty, foot, iTerm2 with "Report keys using CSI u", recent Alacritty), pushes the
"disambiguate escape codes" level so Shift+Enter, Ctrl+I vs Tab and a bare Esc arrive as distinct
keys. The flags are popped on exit, `/restart` and while Neovim runs. Other terminals keep the
legacy encoding, where Shift+Enter is plain Enter; use Alt+Enter there. `TANGO_KITTY_KEYS=off`
skips the protocol. Example:

```json
{ "editor": "vim", "keys": { "expandTools": "ctrl+u" } }
```

## Permissions

Four modes, set with flags or `/mode`:

| mode       | flag           | behaviour                                                                                                                                   |
| ---------- | -------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `confirm`  | `/mode confirm` | ask before every change: file writes and every shell command ask first (allow rules still pass)                                              |
| `ask`      | (default)      | reads and writes inside cwd run; deletes, `git push`/`reset --hard`, `sudo`, `curl … \| sh`, writes outside cwd (except managed worktrees under `~/.tango/worktrees/`, `/add_dir` roots and allow rules) and unknown tools ask first |
| `auto`     | `-y` / `--yes` | everything runs                                                                                                                             |
| `readonly` | `--readonly`   | reads only; writes and shell are refused                                                                                                    |

In the TUI, bare `/mode` opens the mode picker (one line per mode with its description, plan
included). Switching to `auto` first shows the full-access acknowledgement: what it allows (any
file path, any shell command, network), a warning that mistakes may be irreversible, and the
`☐ 我已了解风险，并对自己的数据安全负责` checkbox — Space checks it, Enter enables, Esc cancels.

In the TUI's permission dialog, answer `y`, `n` or `a` (always: saves allow rules for the
commands in the call, taking effect mid-turn). One-shot runs
can't ask, so risky calls are refused there unless `--yes` is given. The gate is a `before_tool`
handler in `src/core/permissions.ts`.

## Web tools

`web_search` uses the first backend whose key is set: `TAVILY_API_KEY`, `BOCHA_API_KEY` (博查),
`BRAVE_API_KEY`. With no key it scrapes Bing's public results page, which works from mainland
China but can break if Bing changes its markup. Force one with `TANGO_SEARCH=bing|tavily|bocha|brave`.

`web_fetch` reads an http(s) URL and turns HTML into markdown-like text (headings, links, lists),
decoding GBK pages correctly. It refuses binary content, stops at 5 MB and times out after 30 s.
Both tools count as reads for permissions.

## Background processes

`bash` with `background: true` starts the command detached in its own process group and returns
at once (after a 2 s grace period that catches commands failing right away) with an id such as
`p1`, its pid and the first output lines. Use it for dev servers, watchers and other commands
that would hit the 120 s foreground timeout. At most 8 run at a time; each keeps the last 200 000
characters of combined stdout+stderr.

| tool           | input                 | what it does                                                         |
| -------------- | --------------------- | -------------------------------------------------------------------- |
| `process_logs` | `id`, `lines` (50)    | last N output lines plus state (`running Ns`, `exited N`, `killed`)  |
| `process_stop` | `id`                  | SIGTERM to the process group, SIGKILL 3 s later if it is still alive |
| `process_list` | none                  | every background process this tango started, running or finished     |

`process_logs` and `process_list` count as reads. `process_stop` never asks in `ask` mode, since
it can only signal processes this tango started; `readonly` refuses it (and `bash`). Background
processes belong to the tango process and are killed when it exits.

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
| `config.json`            | default model (`/model <id>` writes it); `feishuWebhook` for notify/tasks; `theme`: `dark` (default), `light`, or `auto` from the terminal's background (OSC 11, then `COLORFGBG`) |
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
| `theme` | `dark`, `light` or `auto` (terminal background via OSC 11, then `COLORFGBG`) | `/theme` |
| `mouse` | mouse capture; `false` keeps native selection (default `true`) | `/mouse` |
| `animations` | busy-rule, thinking and splash motion (default `true`) | `/animations` |
| `clipboard` | copy path: `auto` (OSC 52 over SSH, else native command, then OSC 52), `osc52` or `native` (default `auto`) | `/clipboard` |
| `ascii` | draw emoji (❓ ➕ 📋 📂 💡 …) and the thinking robot as ASCII (`?` `+` `#` `>` `*`) for terminals without an emoji font (default `false`) | `/ascii` |
| `bubble` | user messages as right-aligned bubbles (default `true`) or full width | `/bubble` |
| `editor` | `default` or `vim` input editing | `/vim` |
| `keys` | remap TUI actions, e.g. `{ "expandTools": "ctrl+u" }` (see "TUI") | edit |
| `nvimViewer` | `ask`, `readonly` (Neovim for read-only views) or `internal` (built-in viewer) | Neovim picker |
| `checkpoint` | `off` disables per-turn workspace snapshots (default on) | edit |
| `autocompact` | compact when context passes this share of the window (default `0.75`); `off` disables | `/autocompact` |
| `maxTurns` | model round-trips per prompt before tango stops the loop (1-500, default 64); `--max-tool-rounds` overrides it for one run | edit |
| `retries` | retries for a model request that fails before any output: network errors (reset, refused, DNS, timeout) and HTTP 408/409/425/429/5xx/529 (0-10, default 10; `0` turns retrying off). Waits 1s, 2s, 4s, 8s, then 15s each (or the server's `Retry-After`); each retry is announced ("第 1/10 次重试"); Esc cancels the wait. 401/403/400 are never retried, nor a stream that already started. A model in `models.json` can set its own `retries` | edit |
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
| global | `~/.tango/config.json` | `/theme`, `/mouse`, `/animations`, `/ascii`, `/clipboard`, `/model` write here |
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
  session.ts    append-only JSONL store, compaction = summary entry + kept verbatim tail (keep)
  context.ts    system prompt = base + AGENTS.md (upward + global) + skills + env
  skills.ts     /save-as-skill drafts → SKILL.md
  diagnostics.ts after write/edit → project checker (tsc/pyright/ruff/go vet/cargo) → tool result
  scheduler.ts  tango task: due-check, per-run sessions, Feishu webhook, daemon loop
  config.ts     ~/.tango loading, model picking, key availability
  permissions.ts  ask / auto / readonly gate on before_tool
  models/       anthropic-messages + openai-completions adapters, presets, cost
  tools/        registry + read/write/edit/bash + grep/find/ls + web_search/web_fetch + notify + diff
                + process_logs/process_stop/process_list (state in core/processes.ts)
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
