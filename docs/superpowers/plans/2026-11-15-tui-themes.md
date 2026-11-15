# TUI Themes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add dark, light, and automatic semantic color themes to the TUI without changing its layout or interaction behavior.

**Architecture:** TypeScript exposes the optional config value through the serve `hello` frame. A focused Rust `theme` module resolves the setting and maps semantic tones to ratatui styles; `App` retains tones in chat history and resolves them only while drawing.

**Tech Stack:** TypeScript, Node test runner, Rust, ratatui, crossterm, tmux smoke tests.

---

### Task 1: Expose theme through the serve protocol

**Files:**
- Modify: `src/core/config.ts`
- Modify: `src/serve.ts`
- Test: `tests/serve.test.ts`

- [ ] **Step 1: Write the failing protocol test**

In the existing `sessions listing` serve test, write `config.json` beside `models.json`:

```ts
await writeFile(join(home, 'config.json'), JSON.stringify({ theme: 'light' }));
```

Then extend the resumed hello assertions:

```ts
assert.equal(hello.theme, 'light');
```

- [ ] **Step 2: Run the test and verify RED**

Run:

```sh
node --import tsx --test --test-name-pattern='sessions listing' tests/serve.test.ts
```

Expected: FAIL because `hello.theme` is `undefined`, not `light`.

- [ ] **Step 3: Add the typed config and hello field**

In `src/core/config.ts`, add to `Config`:

```ts
  /** TUI palette: fixed dark/light or terminal COLORFGBG detection */
  theme?: 'dark' | 'light' | 'auto';
```

In `src/serve.ts`, import `loadConfig` (already imported), load it after `openAgent` succeeds, and include the value:

```ts
const config = await loadConfig();
```

```ts
    totalCostUsd: sessionCostUsd,
    theme: config.theme,
    tools: buildTools().schemas().map(({ name, description }) => ({ name, description }))
```

Unknown JSON strings remain untouched at runtime; the Rust client deliberately falls back to dark.

- [ ] **Step 4: Run the protocol tests and verify GREEN**

Run:

```sh
node --import tsx --test tests/serve.test.ts
```

Expected: all 5 serve tests PASS, including `hello.theme === 'light'`.

- [ ] **Step 5: Commit the protocol slice**

```sh
git add src/core/config.ts src/serve.ts tests/serve.test.ts
git commit -m "Expose configured TUI theme in serve hello"
```

### Task 2: Add and test the semantic theme module

**Files:**
- Create: `tui/src/theme.rs`
- Modify: `tui/src/main.rs`

- [ ] **Step 1: Write failing module tests**

Create `tui/src/theme.rs` with the test expectations first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_explicit_and_fallback_themes() {
        assert!(!Theme::resolve(Some("dark"), Some("0;15")).light);
        assert!(Theme::resolve(Some("light"), Some("15;0")).light);
        assert!(!Theme::resolve(Some("bogus"), Some("0;15")).light);
        assert!(!Theme::resolve(None, Some("0;15")).light);
    }

    #[test]
    fn auto_uses_light_terminal_backgrounds() {
        for value in [Some("0;7"), Some("0;15"), Some("0;255")] {
            assert!(Theme::resolve(Some("auto"), value).light);
        }
        for value in [Some("15;0"), Some("0;8"), Some("junk"), None] {
            assert!(!Theme::resolve(Some("auto"), value).light);
        }
    }
}
```

Add `mod theme;` to `tui/src/main.rs`.

- [ ] **Step 2: Compile the tests and verify RED**

Run:

```sh
cargo test --manifest-path tui/Cargo.toml theme::tests
```

Expected: FAIL because `Theme` is not defined.

- [ ] **Step 3: Implement the minimal palette**

Add above the tests in `tui/src/theme.rs`:

```rust
use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Text,
    Muted,
    User,
    Success,
    Warning,
    Error,
    Selected,
    Badge,
    ActiveBadge,
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub light: bool,
}

impl Theme {
    pub fn dark() -> Self { Self { light: false } }
    pub fn light() -> Self { Self { light: true } }

    pub fn resolve(setting: Option<&str>, colorfgbg: Option<&str>) -> Self {
        match setting {
            Some("light") => Self::light(),
            Some("auto") if colorfgbg
                .and_then(|v| v.rsplit(';').next())
                .and_then(|v| v.parse::<u16>().ok())
                .is_some_and(|bg| matches!(bg, 7 | 15 | 255)) => Self::light(),
            _ => Self::dark(),
        }
    }

    pub fn style(self, tone: Tone) -> Style {
        match (self.light, tone) {
            (_, Tone::Muted) => Style::new().fg(Color::DarkGray),
            (false, Tone::User) => Style::new().fg(Color::Cyan),
            (true, Tone::User) => Style::new().fg(Color::Blue),
            (_, Tone::Success) => Style::new().fg(Color::Green),
            (false, Tone::Warning) => Style::new().fg(Color::Yellow),
            (true, Tone::Warning) => Style::new().fg(Color::Rgb(150, 90, 0)),
            (_, Tone::Error) => Style::new().fg(Color::Red),
            (false, Tone::Selected) => Style::new().add_modifier(Modifier::BOLD),
            (true, Tone::Selected) => Style::new().fg(Color::Black).add_modifier(Modifier::BOLD),
            (false, Tone::Badge) => Style::new().fg(Color::Black).bg(Color::White),
            (true, Tone::Badge) => Style::new().fg(Color::White).bg(Color::Black),
            (_, Tone::ActiveBadge) => Style::new().fg(Color::Black).bg(Color::Yellow),
            (true, Tone::Text) => Style::new().fg(Color::Black),
            (false, Tone::Text) => Style::new(),
        }
    }
}
```

- [ ] **Step 4: Run the module tests and verify GREEN**

Run:

```sh
cargo test --manifest-path tui/Cargo.toml theme::tests
```

Expected: both theme tests PASS.

- [ ] **Step 5: Commit the isolated palette**

```sh
git add tui/src/theme.rs tui/src/main.rs
git commit -m "Add semantic dark and light TUI palettes"
```

### Task 3: Store tones and resolve styles at render time

**Files:**
- Modify: `tui/src/app.rs`

- [ ] **Step 1: Write failing app tests**

Add to the existing `app::tests` module:

```rust
    #[test]
    fn retained_chat_lines_use_semantic_tones() {
        let mut app = App::new();
        app.push("old", Tone::Muted);
        app.theme = Theme::light();
        let lines = app.chat_lines();
        assert_eq!(lines[0].tone, Tone::Muted);
        assert_eq!(app.theme.style(lines[0].tone), Theme::light().style(Tone::Muted));
    }

    #[test]
    fn hello_resolves_theme() {
        let mut app = App::new();
        app.frame(json!({
            "type": "hello", "sessionId": "12345678", "model": "mock/mock",
            "mode": "ask", "theme": "light", "tools": []
        }));
        assert!(app.theme.light);
    }
```

- [ ] **Step 2: Compile and verify RED**

Run:

```sh
cargo test --manifest-path tui/Cargo.toml app::tests
```

Expected: FAIL because `ChatLine` has no `tone` and `App` has no `theme`.

- [ ] **Step 3: Convert retained lines to tones**

Import `crate::theme::{Theme, Tone}`. Replace `ChatLine.style: Style` with `tone: Tone`; add `theme: Theme` to `App`, initialized with `Theme::dark()`. Change the push helpers:

```rust
    fn push(&mut self, text: impl Into<String>, tone: Tone) {
        for line in text.into().split('\n') {
            self.lines.push(ChatLine { text: line.to_string(), tone, output: None });
        }
    }

    fn push_tool(&mut self, text: impl Into<String>, tone: Tone, output: Option<String>) {
        self.lines.push(ChatLine { text: text.into(), tone, output });
    }
```

Mechanically map existing calls: default/`Style::new()` → `Tone::Text`; `dark_gray()` → `Muted`; `cyan()` → `User`; `green()` → `Success`; `yellow()` → `Warning`; `red()` → `Error`; bold selection → `Selected`; black-on-white → `Badge`; black-on-yellow → `ActiveBadge`. In `chat_lines`, clone `tone` instead of `style`.

- [ ] **Step 4: Resolve the hello setting and tones while drawing**

At the start of the hello arm:

```rust
self.theme = Theme::resolve(v["theme"].as_str(), std::env::var("COLORFGBG").ok().as_deref());
```

Use `self.theme.style(...)` for all drawing-only styles. In particular:

```rust
Span::styled(l.text, self.theme.style(l.tone))
```

Status, popup, picker, permission, input rule, expanded output, and code blocks use the corresponding tones from Step 3. Layout, wrapping, text content, key handling, and protocol handling remain unchanged.

- [ ] **Step 5: Run all Rust tests and verify GREEN**

Run:

```sh
cargo test --manifest-path tui/Cargo.toml
cargo build --release --manifest-path tui/Cargo.toml
```

Expected: all app and theme tests PASS; release build succeeds without warnings.

- [ ] **Step 6: Commit the app integration**

```sh
git add tui/src/app.rs
git commit -m "Render TUI chat and dialogs through semantic themes"
```

### Task 4: Document and smoke-test the delivered configurations

**Files:**
- Modify: `README.md`
- Modify: `src/serve.ts` protocol comment if needed to describe the hello field

- [ ] **Step 1: Document configuration**

Add to the README config table:

```md
| `config.json` | default model (`/model <id>` writes it); `feishuWebhook`; `theme`: `dark` (default), `light`, or `auto` from terminal `COLORFGBG`; restart TUI to apply |
```

Document that missing, malformed, or unknown values use dark and that theme changes do not alter sessions or model configuration.

- [ ] **Step 2: Run all automated checks**

Run:

```sh
npm run typecheck
npm run build
npm test
cargo test --release --manifest-path tui/Cargo.toml
cargo build --release --manifest-path tui/Cargo.toml
git diff --check
```

Expected: all TypeScript and Rust tests PASS, both builds succeed, and diff check is clean.

- [ ] **Step 3: Exercise real terminal startups**

Use separate temporary `TANGO_HOME` directories containing a valid test model config and `config.json` values `dark`, `light`, `auto`, and `bogus`. Launch each under tmux with `COLORFGBG=0;15`, capture the pane, assert the hello banner renders, then exit with `/exit`. No model prompt is needed.

Expected: all four configurations start cleanly; no raw escape sequences, panics, or protocol errors appear.

- [ ] **Step 4: Commit documentation and verify repository state**

```sh
git add README.md src/serve.ts
git commit -m "Document TUI theme configuration"
git status --short
```

Expected: clean working tree; the release binary contains the tested implementation.
