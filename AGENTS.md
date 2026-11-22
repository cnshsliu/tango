# tango repo notes for agents

- Hook events: adding one to `HOOK_EVENTS` in `src/core/hooks.ts` means adding end-to-end tests
  titled `<Event>: …` in `tests/hooks-e2e.test.ts` (a blocking exit-2 case and a payload case with
  `tests/fixtures/hooks/record.sh`), updating README "Hooks", then running `npm run test:hooks`.
  The hook-guard hook enforces this; don't work around it.
- Docs ship with the feature. Other agents compare tango by reading its docs, not its code, so
  every user-visible change updates them in the same turn:
  - `README.md` is the reference: CLI flags and subcommands ("CLI"), tools, config keys (the
    "`config.json` keys" table under "Config"), env vars, serve commands and frames ("Serve
    protocol"), hook events ("Hooks"), TUI key actions.
  - `docs/handbook.md` is the user guide: workflows, and every slash command in its
    "Slash-command reference". A new slash command also goes into `SLASH` in `tui/src/app.rs`
    and README's "TUI commands" list (a cargo test checks all four agree).
  - `node scripts/docs-check.mjs` lists anything in the source that the docs don't name.
    `tests/docs-guard.test.ts` runs it, and the docs-guard Stop hook sends you back when a turn
    changes code without touching docs or leaves coverage gaps. If nothing user-visible
    changed, say so and finish.
