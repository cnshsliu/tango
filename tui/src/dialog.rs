//! Application modal standard: rounded complete borders, themed surface/title,
//! a key-hint row inside the frame above the bottom border (a blank row between it and
//! the content), and content confined to the returned inner rectangle.
//! App routes input by focus: viewer → permission → picker → completion → chat;
//! modal scrolling and clicks must never leak into the transcript below.

use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

pub struct DialogPalette {
    pub surface: Color,
    pub title_bg: Color,
    border: Color,
    title_fg: Color,
    hint: Color,
    /// behind a modal dialog: the rest of the screen, dimmed, so the surface stands out
    /// whatever the terminal's own background is
    pub scrim: Color,
    /// what text under the scrim keeps: readable as a shape, clearly behind the dialog
    pub scrim_text: Color,
}

impl DialogPalette {
    pub fn for_theme(theme: Theme) -> Self {
        if theme.light {
            Self {
                surface: Color::Rgb(251, 251, 249),
                title_bg: Color::Rgb(218, 228, 238),
                border: Color::Rgb(115, 120, 128),
                title_fg: Color::Rgb(24, 35, 48),
                hint: Color::Rgb(80, 85, 90),
                scrim: Color::Rgb(223, 225, 228),
                scrim_text: Color::Rgb(168, 172, 178),
            }
        } else {
            Self {
                surface: Color::Rgb(35, 39, 47),
                title_bg: Color::Rgb(50, 64, 82),
                border: Color::Rgb(94, 102, 116),
                title_fg: Color::Rgb(215, 225, 240),
                hint: Color::Rgb(150, 158, 171),
                scrim: Color::Rgb(11, 12, 16),
                scrim_text: Color::Rgb(58, 64, 76),
            }
        }
    }
}

/// Rows a dialog's hint takes inside the frame: a blank separator row, then the hint.
/// Size a dialog `HINT_ROWS` taller than its content plus borders.
pub const HINT_ROWS: u16 = 2;

/// A clickable key in a dialog's bottom hint: where it is drawn and the key it presses.
pub type HintButton = (Rect, KeyEvent);

/// Draws a dialog box, also returning the hint's keys as buttons. The hint is
/// `key label · key label …`; a key token may list alternatives (`tab/enter`, `n/N`,
/// `↑↓`), each its own button. Tokens that aren't a key (`输入`, `滚轮`, `点击`)
/// stay plain text. A single-key segment's label is part of its button.
pub fn render_dialog_buttons(
    f: &mut Frame,
    area: Rect,
    title: &str,
    hint: &str,
    theme: Theme,
) -> (Rect, Vec<HintButton>) {
    let palette = DialogPalette::for_theme(theme);
    let surface = Style::new().bg(palette.surface);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(palette.border).bg(palette.surface))
        .style(surface)
        .title(Span::styled(
            format!(" {title} "),
            Style::new()
                .fg(palette.title_fg)
                .bg(palette.title_bg)
                .add_modifier(Modifier::BOLD),
        ));
    let mut inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return (inner, Vec::new());
    }
    // the hint is the last row inside the frame, a blank row above it; the bottom border
    // stays a plain line. Too short for that, the hint takes the last inner row alone.
    let reserved = HINT_ROWS.min(inner.height);
    let row = Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1);
    inner.height -= reserved;
    let plain = Style::new().fg(palette.hint).bg(palette.surface);
    let key_style = Style::new().fg(palette.title_fg).bg(palette.title_bg);
    let (spans, buttons) = hint_layout(hint, row, plain, key_style);
    f.render_widget(Paragraph::new(Line::from(spans)), row);
    (inner, buttons)
}

/// One hint key token → the key it stands for; None when it isn't a key (`输入`, `滚轮`).
pub fn parse_key(token: &str) -> Option<KeyEvent> {
    let (mods, name) = match token.split_once('+') {
        Some(("ctrl", rest)) => (KeyModifiers::CONTROL, rest),
        Some(("alt", rest)) => (KeyModifiers::ALT, rest),
        Some(("shift", "tab")) => return Some(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)),
        _ => (KeyModifiers::NONE, token),
    };
    let code = match name {
        "esc" => KeyCode::Esc,
        "enter" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "空格" | "space" => KeyCode::Char(' '),
        "↑" => KeyCode::Up,
        "↓" => KeyCode::Down,
        "←" => KeyCode::Left,
        "→" => KeyCode::Right,
        "pgup" => KeyCode::PageUp,
        "pgdn" => KeyCode::PageDown,
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_graphic() => KeyCode::Char(c),
                _ => return None,
            }
        }
    };
    Some(KeyEvent::new(code, mods))
}

/// Split one key token into its alternatives, each with the text drawn before it:
/// `tab/enter` → [tab, "/"enter], `↑↓/滚轮` → [↑, ↓, "/"滚轮], `{/}` → [{, "/"}].
/// A lone `/` is itself a key.
fn alternatives(token: &str) -> Vec<(&'static str, &str)> {
    if token == "/" {
        return vec![("", "/")];
    }
    let mut out = Vec::new();
    for (i, part) in token.split('/').enumerate() {
        let sep = if i > 0 { "/" } else { "" };
        let arrows = part.chars().count() > 1 && part.chars().all(|c| "↑↓←→".contains(c));
        if arrows {
            for (k, (at, c)) in part.char_indices().enumerate() {
                out.push((if k == 0 { sep } else { "" }, &part[at..at + c.len_utf8()]));
            }
        } else {
            out.push((sep, part));
        }
    }
    out
}

/// Split a hint on ` · `, except inside （…）/(…): "a 始终允许（记住 git · npm）" stays one.
fn segments(hint: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut start) = (0i32, 0);
    for (i, c) in hint.char_indices() {
        match c {
            '（' | '(' => depth += 1,
            '）' | ')' => depth -= 1,
            '·' if depth == 0 && hint[..i].ends_with(' ') && hint[i + 2..].starts_with(' ') => {
                out.push(&hint[start..i - 1]);
                start = i + '·'.len_utf8() + 1;
            }
            _ => {}
        }
    }
    out.push(&hint[start..]);
    out
}

/// Lay the hint out on `row`: spans to draw plus a button per key alternative.
/// Segments are split on ` · `; each segment's first word is its key token.
fn hint_layout(
    hint: &str,
    row: Rect,
    plain: Style,
    key_style: Style,
) -> (Vec<Span<'static>>, Vec<HintButton>) {
    let mut spans = vec![Span::styled(" ", plain)];
    let mut buttons = Vec::new();
    let mut x = row.x + 1;
    let end = row.x + row.width;
    let push = |spans: &mut Vec<Span<'static>>, text: String, style: Style, x: &mut u16| {
        let w = UnicodeWidthStr::width(text.as_str()) as u16;
        spans.push(Span::styled(text, style));
        let at = *x;
        *x = x.saturating_add(w);
        (at, w)
    };
    for (i, seg) in segments(hint).into_iter().enumerate() {
        if i > 0 {
            push(&mut spans, " · ".into(), plain, &mut x);
        }
        let (head, rest) = match seg.split_once(' ') {
            Some((h, r)) => (h, Some(r)),
            None => (seg, None),
        };
        let alts = alternatives(head);
        let keys: Vec<Option<KeyEvent>> = alts.iter().map(|(_, a)| parse_key(a)).collect();
        if keys.iter().all(Option::is_none) {
            push(&mut spans, seg.to_string(), plain, &mut x);
            continue;
        }
        for ((sep, alt), key) in alts.iter().zip(&keys) {
            if !sep.is_empty() {
                push(&mut spans, sep.to_string(), plain, &mut x);
            }
            let style = if key.is_some() { key_style } else { plain };
            let (at, w) = push(&mut spans, alt.to_string(), style, &mut x);
            if let Some(k) = key {
                // a single key's button also covers its label
                let label_w = match rest {
                    Some(r) if alts.len() == 1 => 1 + UnicodeWidthStr::width(r) as u16,
                    _ => 0,
                };
                if at < end {
                    let width = (w + label_w).min(end - at);
                    buttons.push((Rect::new(at, row.y, width, 1), *k));
                }
            }
        }
        if let Some(r) = rest {
            push(&mut spans, format!(" {r}"), plain, &mut x);
        }
    }
    spans.push(Span::styled(" ", plain));
    (spans, buttons)
}

/// A modal dialog's backdrop: everything outside `keep` goes to the scrim color, its text to
/// the faint one (styles dropped, so a reversed cell cannot light up). The dialog itself is
/// untouched — it was drawn last, and sits on top.
pub fn dim_outside(f: &mut Frame, keep: Rect, theme: Theme) {
    let palette = DialogPalette::for_theme(theme);
    let buf = f.buffer_mut();
    let width = buf.area.width;
    if width == 0 {
        return;
    }
    for (i, cell) in buf.content.iter_mut().enumerate() {
        let at = Position::new((i as u16) % width, (i as u16) / width);
        if keep.contains(at) {
            continue;
        }
        cell.set_fg(palette.scrim_text).set_bg(palette.scrim);
        cell.modifier = Modifier::empty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    #[test]
    fn dialog_draws_complete_rounded_border_and_surface() {
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
        let area = Rect::new(2, 1, 12, 6);
        let mut inner = Rect::default();
        let mut buttons = Vec::new();
        terminal
            .draw(|f| {
                (inner, buttons) = render_dialog_buttons(f, area, "title", "esc 关闭", Theme::dark());
            })
            .unwrap();
        // content: rows 2..4; row 4 blank; row 5 the hint; row 6 the bottom border
        assert_eq!(inner, Rect::new(3, 2, 10, 2));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 1)].symbol(), "╭");
        assert_eq!(buffer[(13, 1)].symbol(), "╮");
        assert_eq!(buffer[(13, 2)].symbol(), "│");
        assert_eq!(buffer[(13, 6)].symbol(), "╯");
        let row = |y: u16| (3..13).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert_eq!(row(4).trim(), "", "a blank row above the hint");
        assert!(row(5).contains("esc"), "the hint is the last inner row: {:?}", row(5));
        assert_eq!(row(6), "─".repeat(10), "the bottom border stays a plain line");
        assert_eq!(buttons[0].0.y, 5);
        assert_eq!(buffer[(2, 5)].symbol(), "│", "the hint sits inside the side borders");
        assert_eq!(
            buffer[(4, 2)].bg,
            DialogPalette::for_theme(Theme::dark()).surface
        );
    }

    #[test]
    fn hint_keys_become_buttons() {
        let row = Rect::new(10, 5, 120, 1);
        let s = Style::new();
        let (spans, buttons) = hint_layout(
            "↑↓/滚轮 滚动 · y 允许 · tab/enter 选择 · 输入 过滤 · ctrl+e 用 nvim 打开 · esc 关闭",
            row,
            s,
            s,
        );
        let text: String = spans.iter().map(|sp| sp.content.as_ref()).collect();
        assert_eq!(
            text,
            " ↑↓/滚轮 滚动 · y 允许 · tab/enter 选择 · 输入 过滤 · ctrl+e 用 nvim 打开 · esc 关闭 "
        );
        let keys: Vec<KeyEvent> = buttons.iter().map(|(_, k)| *k).collect();
        let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(
            keys,
            vec![
                k(KeyCode::Up),
                k(KeyCode::Down),
                k(KeyCode::Char('y')),
                k(KeyCode::Tab),
                k(KeyCode::Enter),
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
                k(KeyCode::Esc),
            ],
            "输入 / 滚轮 are not keys"
        );
        // "y 允许" is one button covering key and label, at its drawn column
        let col = |needle: &str| 10 + UnicodeWidthStr::width(&text[..text.find(needle).unwrap()]) as u16;
        let (y_rect, _) = buttons[2];
        assert_eq!((y_rect.x, y_rect.width), (col("y 允许"), 6), "允许 is two wide chars");
        assert_eq!(buttons[6].0.x, col("esc 关闭"));
        assert!(buttons.iter().all(|(r, _)| r.y == 5));
    }

    #[test]
    fn a_parenthesized_dot_stays_in_its_segment() {
        assert_eq!(
            segments("y 允许 · a 始终允许（记住 git · npm） · esc 关闭"),
            vec!["y 允许", "a 始终允许（记住 git · npm）", "esc 关闭"]
        );
    }

    #[test]
    fn key_tokens_parse() {
        assert_eq!(parse_key("shift+tab"), Some(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)));
        assert_eq!(parse_key("空格"), Some(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE)));
        assert_eq!(parse_key("N"), Some(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE)));
        assert_eq!(parse_key("点击"), None);
        assert_eq!(parse_key("待定"), None);
        let a = alternatives("{/}");
        assert_eq!(a, vec![("", "{"), ("/", "}")]);
        assert_eq!(alternatives("/"), vec![("", "/")]);
    }

    fn luminance(c: Color) -> u32 {
        match c {
            Color::Rgb(r, g, b) => (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1000,
            _ => 0,
        }
    }

    #[test]
    fn the_scrim_sets_the_surface_apart_in_both_themes() {
        for (name, theme) in [("dark", Theme::dark()), ("light", Theme::light())] {
            let p = DialogPalette::for_theme(theme);
            assert_ne!(p.scrim, p.surface, "{name}");
            // the scrim is clearly darker than the surface: gray under near-white, near-black under slate
            assert!(luminance(p.scrim) + 20 < luminance(p.surface), "{name}: {:?} vs {:?}", p.scrim, p.surface);
            // the text under it is a little lighter: a shape, clearly behind the dialog
            let step = luminance(p.scrim_text).abs_diff(luminance(p.scrim));
            assert!((15..60).contains(&step), "{name}: {:?} on {:?} = {step}", p.scrim_text, p.scrim);
            // and the surface is not a common terminal background in disguise: it sits well
            // above the darkest dark themes' #16181d / #1a1b26 and below paper white
            let l = luminance(p.surface);
            if theme.light {
                assert!(l >= 248, "{name}: {l}");
            } else {
                assert!(l >= 30, "{name}: {l}");
            }
        }
    }

    #[test]
    fn dim_outside_recolors_everything_but_the_dialog() {
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
        let keep = Rect::new(5, 2, 10, 4);
        let theme = Theme::dark();
        terminal
            .draw(|f| {
                f.render_widget(
                    Paragraph::new("hi").style(Style::new().add_modifier(Modifier::REVERSED)),
                    Rect::new(0, 0, 2, 1),
                );
                dim_outside(f, keep, theme);
            })
            .unwrap();
        let p = DialogPalette::for_theme(theme);
        let b = terminal.backend().buffer();
        // the text stays, faint; a reversed cell cannot light up
        let cell = &b[(0, 0)];
        assert_eq!((cell.symbol(), cell.fg, cell.bg, cell.modifier), ("h", p.scrim_text, p.scrim, Modifier::empty()));
        assert_eq!((b[(19, 7)].fg, b[(19, 7)].bg), (p.scrim_text, p.scrim), "the far corner too");
        assert_eq!(b[(keep.x, keep.y)].bg, Color::Reset, "inside the dialog is not ours to paint");
    }

    #[test]
    fn light_dialog_has_its_own_surface() {
        let dark = DialogPalette::for_theme(Theme::dark());
        let light = DialogPalette::for_theme(Theme::light());
        assert_ne!(dark.surface, light.surface);
        assert_ne!(dark.title_bg, light.title_bg);
    }
}
