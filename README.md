# tango

> it takes two — you and the agent.

A light agent harness for work and code. China-first: DeepSeek / Kimi / Qwen / GLM work
out of the box with a single API key, no proxy.

## Status

Core loop is working: agent loop, JSONL sessions with compaction, Anthropic + OpenAI-compatible
adapters, built-in tools (read / write / edit / bash / grep / find / ls / web_search / web_fetch /
office_read / office_write / notify / diff), a permission gate, AGENTS.md + skills context assembly,
typed event bus, cost tracking with long-prompt price tiers, `/save-as-skill`, scheduled tasks
(`tango task`) with Feishu webhook delivery, auto type diagnostics after edits, and a JSONL
serve protocol (`tango serve --stdio`) ready for a TUI client to plug into.

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
npm run dev --                                                  # REPL
```

Install globally after `npm run build`; `tango` / `tang` are the bin names.

## CLI

```
tango [prompt...]      run one prompt and exit
tango                  interactive REPL
tango -c               continue most recent session
tango -r <id>          resume by id/prefix
tango -m <model>       pick model            tango --models
tango --sessions       list sessions         --cwd <dir>
```

REPL commands: `/model`, `/models`, `/mode`, `/sessions`, `/compact`, `/save-as-skill`, `/cost`, `/new`, `/help`, `/exit`.

## Scheduled tasks

`tango task add 新闻摘要 2h 搜集今天的 AI 新闻，写成摘要发飞书 [--mode auto]` stores a task in
`~/.tango/tasks.json`. `tango task run --loop` is the daemon: every minute it runs tasks whose
interval has passed, each in its own session (`task-<name>-<time>.jsonl`, visible in `--sessions`).
Results go to the `feishuWebhook` in config as a card. Headless tasks can't answer prompts, so
they default to `readonly` (reads, web, writes files); `--mode auto` also allows bash.
The model can also push mid-task with the `notify` tool. cron works too — `tango task run` runs
due tasks once and exits.

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

`tango serve --stdio` speaks JSONL: one JSON command per stdin line, one JSON frame per stdout
line (human logs go to stderr, stdout is protocol-only). Commands: `prompt`, `interrupt`,
`permission` (answers a `permission_request` frame by request id), `set_model`, `set_mode`,
`compact`, `status`. Frames: `hello` (session/model/tools) → `stream` (token deltas) →
`tool_result` / `permission_request` → `message` → `turn_end` (usage + running cost) → `result`
(per command). stdin EOF lets queued work finish, so pipes work too. This is the seam the TUI —
and later web/IDE shells — plug into; the core never knows which face is attached.

## Permissions

Three modes, set with flags or `/mode`:

| mode | flag | behaviour |
| --- | --- | --- |
| `ask` | (default) | reads and writes inside cwd run; deletes, `git push`/`reset --hard`, `sudo`, `curl … \| sh`, writes outside cwd and unknown tools ask first |
| `auto` | `-y` / `--yes` | everything runs |
| `readonly` | `--readonly` | reads only; writes and shell are refused |

In the REPL, answer `y`, `n` or `a` (always, switches to `auto` for the session). One-shot runs
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
| `config.json` | default model (`/model <id>` writes it); `feishuWebhook` for notify/tasks |
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
src/serve.ts    tango serve --stdio: JSONL protocol for external UIs (the TUI seam)
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
