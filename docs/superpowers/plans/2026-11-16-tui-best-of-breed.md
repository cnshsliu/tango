# Tango Best-of-Breed TUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `executing-plans` for inline autopilot execution. Execute tasks in order with test-first red/green checks. Do not dispatch subagents and do not create git commits unless the user explicitly requests them.

**Goal:** Build a coherent terminal UI that combines OpenCode's command/window model, Claude Code's composer and long-session reading experience, Codex's auditability, and grok-build's rendering discipline without replacing Tango's core.

**Architecture:** Keep `tango serve` as the only agent/session boundary. Keep local presentation state in the Rust TUI, add protocol commands only for operations requiring core state or persistence, and reuse the existing dialog/viewer/theme components. Every feature must preserve the current input, permission, Neovim, session-resume, and Vim-mode behavior.

**Tech Stack:** Rust 2024, ratatui, crossterm, serde_json; TypeScript/Node for `serve` and core; Node test runner; Rust unit tests; tmux with a local mock OpenAI-SSE server for end-to-end checks.

---

## Boundaries and shared rules

- Modify `tui/src/app.rs`, `tui/src/viewer.rs`, `tui/src/main.rs`, `tui/src/keys.rs`, `tui/src/theme.rs`, `src/serve.ts`, `src/core/agent.ts`, `src/core/config.ts`, `src/core/tools/diff.ts`, and their existing test files only when the relevant task requires it.
- Do not replace the JSONL session store, model adapters, permission system, or agent tool loop.
- Each task adds its failing tests first, observes the expected failure, writes the minimal implementation, runs the focused tests, then runs the complete Rust suite and relevant TypeScript suite.
- The final task runs all checks and real tmux scenarios with a mock model; no paid model calls.
- Update `README.md` whenever user-facing commands, keys, protocol frames, or configuration change.

### Task 1: Unified command palette and filterable pickers

**Files:** `tui/src/app.rs`, `tui/src/dialog.rs`

- [x] Add failing tests proving `Ctrl+P` opens a command picker containing `/model`, `/resume`, `/theme`, and `/diff`; typing `res` narrows visible results to `/resume`; Enter runs the selected command without sending a prompt.
- [x] Run `cargo test --manifest-path tui/Cargo.toml command_palette -- --nocapture` and confirm missing `PickerKind::Command` / filtering behavior.
- [x] Introduce `Picker::new(kind, items)`, `query`, `filtered_items()`, and `accept_typed_key()`. Keep non-filterable permission/edit-resolution pickers unchanged. Filter case-insensitively across `label`, `note`, and `id`, clamp selection after every filter change, and route mouse hit-testing through the same filtered list.
- [x] Add `PickerKind::Command` and open it on `Ctrl+P`. Its `PickItem.id` is the slash command, its label is the command, and its note is the description from `SLASH`. Selection calls `run_slash`.
- [x] Apply filtering to model and session pickers; show the query in the dialog title or first row and keep `↑↓`, wheel, click, Enter, Tab, and Esc behavior consistent.
- [x] Run `cargo test --manifest-path tui/Cargo.toml picker` and the complete Rust suite.

### Task 2: Prompt history search and draft stash/restore

**Files:** `tui/src/app.rs`

- [x] Add failing tests: submitting two prompts records their original unexpanded text; `Ctrl+R` opens newest-first history; filtering and Enter restore the complete draft and cursor without sending it; consecutive duplicate prompts are stored once.
- [x] Add a failing paste test: `Alt+S` with a non-empty composer stashes text, cursor, and `PastedSpan` metadata; pressing it on the empty composer restores all three and leaves submission explicit.
- [x] Run `cargo test --manifest-path tui/Cargo.toml prompt_history` and `cargo test --manifest-path tui/Cargo.toml draft_stash` to verify the missing behavior.
- [x] Add `prompt_history: Vec<String>`, `stashed_draft: Option<InputDraft>`, and `PickerKind::History`. Record history in `send_prompt` before `@file` expansion. Reuse Task 1 filtering for `Ctrl+R`; restoration uses `replace_input_range` and never calls `serve.command`.
- [x] Use `Alt+S` because `Ctrl+S` already means send. Preserve paste placeholders and the current Vim mode. Session switches clear transient stash/history selection but do not write anything to disk.
- [x] Run focused tests and the complete Rust suite; update `/help` and the command palette descriptions.

### Task 3: `@file` line-range expansion

**Files:** `tui/src/app.rs`

- [x] Add failing tests for `@src/main.ts#20-45`, `@src/main.ts#L20-L45`, and `@src/main.ts#20`; verify only the selected inclusive lines are inlined, headers retain the range, directories with a range are not expanded, and unrelated `@` text is unchanged.
- [x] Run `cargo test --manifest-path tui/Cargo.toml at_file_line_range -- --nocapture` and confirm current expansion does not recognize ranges.
- [x] Add `parse_at_target(raw) -> (path, Option<LineRange>)` and `LineRange { start, end }`, accepting a range suffix only when it matches the numeric forms above. Reject zero, reversed, and malformed ranges without touching the original token.
- [x] Read the file once, slice with `lines().skip(start - 1).take(end - start + 1)`, retain the existing 6 KB cap, and return warnings for out-of-range or unreadable selections.
- [x] Run focused tests and the complete Rust suite; document the syntax in `/help` and `README.md`.

### Task 4: Stable reading position, unread count, and jump-to-bottom

**Files:** `tui/src/app.rs`

- [x] Add a failing two-draw `TestBackend` test: scroll up, append streamed and committed lines, draw again, and assert the original visual rows remain stationary while a `N 行新内容` indicator appears. Activate `Ctrl+End` and assert auto-follow resumes and the indicator disappears.
- [x] Run `cargo test --manifest-path tui/Cargo.toml unread_jump_bottom -- --nocapture` and confirm new output currently shifts the reading viewport.
- [x] Track `last_visual_total`, `unread_rows`, and `jump_area`. When `scroll_back > 0`, add each newly arrived visual-row delta to `scroll_back` so the absolute offset remains stable. When following, keep the existing bottom anchor.
- [x] Render a compact `↓ N 行新内容 · ctrl+end 回到底部` badge over the chat's lower edge. Clicking the badge or pressing `Ctrl+End` sets `scroll_back = 0` and clears unread state. Scrolling manually to the bottom has the same effect.
- [x] Run the focused test and complete Rust suite; include wrapped CJK lines and live `stream` growth in the checks.

### Task 5: Transcript search, prompt navigation, and copy

**Files:** `tui/src/viewer.rs`, `tui/src/app.rs`, `tui/src/clipboard.rs`, `tui/src/main.rs`

- [x] Add failing viewer tests for `/query`, `n/N`, no-match handling, and `{`/`}` moving between recorded prompt rows. Add an app test proving `Ctrl+T` creates a transcript viewer rather than the latest-item viewer.
- [x] Run `cargo test --manifest-path tui/Cargo.toml transcript_search -- --nocapture` and confirm the viewer currently accepts only scrolling/editing keys.
- [x] Add transcript metadata and search state to `Viewer`: `prompt_rows`, `search_input`, `query`, `matches`, and `match_index`. Recompute case-insensitive matches when the query changes; scrolling to a match uses the same line-based viewport as ordinary viewer scrolling.
- [x] `Ctrl+T` snapshots visible chat into a transcript viewer with prompt row indices. In transcript mode `/` enters search, Enter accepts, Esc cancels, `n/N` changes matches, `{`/`}` changes prompts, and `q` returns without changing chat position.
- [x] Add `clipboard::copy(text)` using `pbcopy` on macOS and `wl-copy`, `xclip`, or `xsel` on Linux. Bind `c` to copy the current viewer text and show a success/error notice without closing it. Test command failure with a temporary fake executable; do not overwrite the developer's real clipboard during tests.
- [x] Run focused tests and the complete Rust suite; update the shared dialog key hints.

### Task 6: Auditable tool failures and quiet focus mode

**Files:** `tui/src/app.rs`

- [x] Add failing tests proving an error `tool_result` shows the first meaningful output lines inline, successful tools stay summarized, `/focus` hides successful tool activity and thinking but never hides errors or assistant/user messages, and full output remains available in the viewer.
- [x] Run `cargo test --manifest-path tui/Cargo.toml focus_and_tool_errors -- --nocapture` and confirm current error output requires manual expansion.
- [x] Add `focus: bool` and explicit tool metadata to `ChatLine`. `chat_lines()` becomes the single filtering/expansion point: error rows append at most eight non-empty output lines, while focus removes successful tool rows and thinking previews only.
- [x] Toggle focus with `/focus`; keep the state visible in the status bar. Do not automatically open a modal on failure and do not interrupt streaming or permissions.
- [x] Apply identical rendering rules to restored history. Run focused tests and the complete Rust suite.

### Task 7: Context-window usage in the status bar

**Files:** `src/serve.ts`, `src/core/agent.ts`, `tui/src/app.rs`, `tests/serve.test.ts`, `tests/agent.test.ts`

- [x] Add failing protocol tests proving `hello` and `turn_end` include `contextWindow` and `contextTokens`, and `set_model` returns the new window. Add a failing Rust status-bar test for token/percentage display and warning thresholds.
- [x] Run `node --import tsx --test tests/serve.test.ts` and `cargo test --manifest-path tui/Cargo.toml context_usage` to observe the missing fields/rendering.
- [x] Use the latest assistant request's `usage.input` for live usage and `agent.model.contextWindow ?? 128_000` for capacity. On `hello`, recover the latest assistant usage from session history. On `turn_end`, forward the new values without changing cost accounting.
- [x] Add a public read-only `Agent::contextTokenEstimate()` for post-compaction/status responses; use the existing estimator and do not expose mutable session state.
- [x] Render `ctx <used>k/<window>k <pct>%` with normal tone below 60%, warning at 60–79%, and error at 80% or above. Update it on hello, turn end, model switch, and compaction.
- [x] Run focused TypeScript/Rust tests, then both complete suites.

### Task 8: Live theme switching and mouse-capture control

**Files:** `src/core/config.ts`, `src/serve.ts`, `tui/src/app.rs`, `tui/src/main.rs`, `tui/src/theme.rs`, `tests/serve.test.ts`

- [x] Add failing protocol tests for `set_theme` (`dark|light|auto`) and `set_mouse` (boolean), including invalid values and preservation of unrelated config fields. Add failing Rust tests for `/theme` and `/mouse off` requests.
- [x] Run the focused tests and confirm the commands/fields are absent.
- [x] Add optional `mouse` config, include it in `hello`, and implement both protocol commands with `saveConfig({ ...(await loadConfig()), ... })`. Add `Pending::Theme` and `Pending::Mouse` result handling.
- [x] `/theme` opens a picker; choosing a theme persists it and immediately resolves `Theme::resolve`. `/mouse [on|off]` toggles or sets the desired state and emits a terminal-control request.
- [x] In `main.rs`, centralize `EnableMouseCapture`/`DisableMouseCapture` so the new request and Neovim suspend/resume use the same state. Mouse-off keeps keyboard scrolling and leaves native terminal selection available; Neovim restoration must respect the saved preference.
- [x] Run focused tests and both complete suites; verify real escape sequences in tmux without corrupting the alternate screen.

### Task 9: Read-only `/diff` panel

**Files:** `src/serve.ts`, `tui/src/app.rs`, `tests/serve.test.ts`

- [x] Add a failing protocol test in a temporary git repository: `diff` returns a full working-tree patch, supports `staged`, returns a readable non-repo error, and never mutates the repository.
- [x] Run `node --import tsx --test tests/serve.test.ts` and confirm the command is unknown.
- [x] Reuse `diffTool.execute` in `serve` with the session cwd and a fresh abort signal. Bare `/diff` requests all tracked changes; `/diff --staged` requests the staged patch; optional paths pass through without shell interpolation.
- [x] Add `Pending::Diff`; successful results open the shared read-only viewer titled `工作区 diff` or `暂存 diff`. Empty results display `clean — nothing changed`; failures appear in the viewer without changing chat scroll or input.
- [x] Run focused tests and both complete suites; verify modified, staged, untracked-summary, and clean cases.

### Task 10: `/btw` ephemeral context question

**Files:** `src/core/agent.ts`, `src/serve.ts`, `tui/src/app.rs`, `tests/agent.test.ts`, `tests/serve.test.ts`

- [x] Add a failing agent test proving an ephemeral question sends existing context plus the question with `tools: []`, returns streamed text, and leaves `store.allMessages()` byte-for-byte unchanged.
- [x] Add a failing serve test proving `/btw`'s protocol response does not appear in later `history`, while ordinary prompts remain unchanged.
- [x] Run focused tests and confirm `Agent::ask` and the serve command are missing.
- [x] Implement `Agent::ask(text, onStream)` using `buildContext`, `store.messages()`, `adapterFor`, and a local message array. Do not append user/assistant messages, emit ordinary `message` frames, or expose tools. Preserve usage/cost events and abort behavior.
- [x] Add serve command `btw` with request id-tagged `btw_stream` frames and a final `result`. Add `Pending::Btw`; the TUI displays streamed content in a separate read-only dialog and never calls `push_user` or adds it to the main transcript.
- [x] Run focused tests and both complete suites; verify a later ordinary model request contains no `/btw` question or answer.

### Task 11: Documentation and final end-to-end acceptance

**Files:** `README.md`, `docs/superpowers/plans/2026-11-16-tui-best-of-breed.md`

- [x] Mark completed plan checkboxes only after their focused and complete tests pass. Document every new key, command, protocol field, and config value in `README.md`.
- [x] Run `npm run typecheck && npm run build && npm test` and require 56 or more passing tests with zero failures.
- [x] Run `cargo fmt --manifest-path tui/Cargo.toml -- --check`, `cargo test --release --manifest-path tui/Cargo.toml`, and `cargo build --release --manifest-path tui/Cargo.toml` with zero warnings/errors.
- [x] In tmux with a local mock model, exercise command filtering, history restore, stash/restore, ranged file expansion, paused follow/unread/jump, transcript search, failed-tool output, context status, live theme, mouse toggle, diff, and `/btw` history isolation.
- [x] Run `git diff --check` and `git status --short`. Remove only temporary artifacts created by these checks, preserve all pre-existing user changes, and report modified files and verification evidence without committing.
