# tango repo notes for agents

- Hook events: adding one to `HOOK_EVENTS` in `src/core/hooks.ts` means adding end-to-end tests
  titled `<Event>: …` in `tests/hooks-e2e.test.ts` (a blocking exit-2 case and a payload case with
  `tests/fixtures/hooks/record.sh`), updating README "Hooks", then running `npm run test:hooks`.
  The hook-guard hook enforces this; don't work around it.
