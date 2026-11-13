# tango

> it takes two — you and the agent.

A light agent harness for work and code. China-first: DeepSeek / Kimi / Qwen / GLM work
out of the box with a single API key, no proxy.

## Status

Core loop is working: agent loop, JSONL sessions with compaction, Anthropic + OpenAI-compatible
adapters, four built-in tools (read / write / edit / bash), AGENTS.md + skills context assembly,
typed event bus, cost tracking. CLI REPL only; TUI, message bridges and scheduler come later.

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

REPL commands: `/model`, `/models`, `/sessions`, `/compact`, `/cost`, `/new`, `/help`, `/exit`.

## Config

All user state lives in `~/.tango/` (override with `TANGO_HOME`):

| file | purpose |
| --- | --- |
| `config.json` | default model (`/model <id>` writes it) |
| `models.json` | extra/overriding model specs, same shape as built-in presets |
| `AGENTS.md` | global instructions appended to the system prompt |
| `skills/<name>/SKILL.md` | global skills (project-level: `<cwd>/.tango/skills/`) |
| `sessions/*.jsonl` | append-only sessions, one line per entry |

Model specs resolve keys from env: `"apiKey": "$DEEPSEEK_API_KEY"`.

## Layout

```
src/core/
  types.ts      shared message/model/tool/session types (imports nothing)
  events.ts     typed bus; before_tool can veto, before_request can mutate
  agent.ts      the loop: model → tools → model, persist + emit each step
  session.ts    append-only JSONL store, compaction = summary entry
  context.ts    system prompt = base + AGENTS.md (upward + global) + skills + env
  config.ts     ~/.tango loading, model picking, key availability
  models/       anthropic-messages + openai-completions adapters, presets, cost
  tools/        registry + read/write/edit/bash
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
