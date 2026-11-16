//! Application modal standard: rounded complete borders, themed surface/title,
//! bottom key hints, and content confined to the returned inner rectangle.
//! App routes input by focus: viewer → permission → picker → completion → chat;
//! modal scrolling and clicks must never leak into the transcript below.

use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear};

pub struct DialogPalette {
    pub surface: Color,
    pub title_bg: Color,
    border: Color,
    title_fg: Color,
    hint: Color,
}

impl DialogPalette {
    pub fn for_theme(theme: Theme) -> Self {
        if theme.light {
            Self {
                surface: Color::Rgb(247, 247, 243),
                title_bg: Color::Rgb(218, 228, 238),
                border: Color::Rgb(115, 120, 128),
                title_fg: Color::Rgb(24, 35, 48),
                hint: Color::Rgb(80, 85, 90),
            }
        } else {
            Self {
                surface: Color::Rgb(22, 24, 29),
                title_bg: Color::Rgb(50, 64, 82),
                border: Color::Rgb(94, 102, 116),
                title_fg: Color::Rgb(215, 225, 240),
                hint: Color::Rgb(150, 158, 171),
            }
        }
    }
}

pub fn render_dialog(f: &mut Frame, area: Rect, title: &str, hint: &str, theme: Theme) -> Rect {
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
        ))
        .title_bottom(Line::from(Span::styled(
            format!(" {hint} "),
            Style::new().fg(palette.hint).bg(palette.surface),
        )));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    inner
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    #[test]
    fn dialog_draws_complete_rounded_border_and_surface() {
        let mut terminal = Terminal::new(TestBackend::new(20, 8)).unwrap();
        let area = Rect::new(2, 1, 12, 5);
        let mut inner = Rect::default();
        terminal
            .draw(|f| {
                inner = render_dialog(f, area, "title", "hint", Theme::dark());
            })
            .unwrap();
        assert_eq!(inner, Rect::new(3, 2, 10, 3));
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(2, 1)].symbol(), "╭");
        assert_eq!(buffer[(13, 1)].symbol(), "╮");
        assert_eq!(buffer[(13, 2)].symbol(), "│");
        assert_eq!(buffer[(13, 5)].symbol(), "╯");
        assert_eq!(
            buffer[(4, 2)].bg,
            DialogPalette::for_theme(Theme::dark()).surface
        );
    }

    #[test]
    fn light_dialog_has_its_own_surface() {
        let dark = DialogPalette::for_theme(Theme::dark());
        let light = DialogPalette::for_theme(Theme::light());
        assert_ne!(dark.surface, light.surface);
        assert_ne!(dark.title_bg, light.title_bg);
    }
}
