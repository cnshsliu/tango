# TUI themes

## Goal

Add built-in dark and light palettes to the Rust TUI. `~/.tango/config.json` accepts `"theme": "dark" | "light" | "auto"`; missing or unknown values fall back to dark. `auto` uses the terminal's `COLORFGBG` background hint and otherwise falls back to dark. There is no theme picker or live switching in this iteration.

## Architecture

- TypeScript `Config` gains the optional `theme` field. `serve` includes its string value in the existing `hello` frame; absent values are omitted.
- A Rust `Theme` maps semantic tones to ratatui styles. `Tone` covers text, muted, user, assistant, success, warning, error, selection, and reversed status/badge styles.
- `App` owns one resolved `Theme`. `ChatLine` stores a `Tone`, not a concrete `Style`, so history remains independent of the active palette.
- Existing hard-coded `Style::new()` call sites become semantic tones. Rendering resolves tones through the theme, including chat, status bar, completion popup, pickers, permission dialog, code blocks, and expanded tool output.
- On `hello`, the TUI resolves `dark`/`light` directly. For `auto`, a light numeric background in `COLORFGBG` selects light; dark, missing, or malformed values select dark.

## Error handling

Theme selection is non-critical: absent, malformed, and unknown values never prevent startup and consistently use dark. Other config loading errors retain their current behavior.

## Verification

- Rust unit tests cover palette resolution, `COLORFGBG` parsing/fallback, and semantic styling of retained chat lines.
- A serve protocol test covers `theme` in `hello` without requiring a real user config file.
- The existing TypeScript and Rust suites must pass.
- tmux smoke tests launch the real TUI in forced dark, light, auto, and invalid-value configurations and check that each starts and renders without control-sequence corruption.

## Documentation

README documents the config key, accepted values, fallback behavior, and the fact that a restart is required to apply changes.
