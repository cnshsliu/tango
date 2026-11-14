# tango

> it takes two — you and the agent.

A light agent harness for work and code. China-first: DeepSeek / Kimi / Qwen / GLM work
out of the box with a single API key, no proxy.

## Status

Core loop is working: agent loop, JSONL sessions with compaction, Anthropic + OpenAI-compatible
adapters, built-in tools (read / write / edit / bash / grep / find / ls / web_search / web_fetch), a permission gate,
AGENTS.md + skills context assembly, typed event bus, cost tracking. CLI REPL only; TUI,
message bridges and scheduler come later.

Known blocker: the tknet.ai gateway currently drops `tools` from requests, so models behind it
can chat but not call tools. Fix in progress on the gateway side.

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

REPL commands: `/model`, `/models`, `/mode`, `/sessions`, `/compact`, `/cost`, `/new`, `/help`, `/exit`.

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
  permissions.ts  ask / auto / readonly gate on before_tool
  models/       anthropic-messages + openai-completions adapters, presets, cost
  tools/        registry + read/write/edit/bash + grep/find/ls + web_search/web_fetch
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
