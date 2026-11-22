use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Text,
    Muted,
    Thinking,
    User,
    Success,
    Warning,
    Error,
    Selected,
    Badge,
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub light: bool,
}

impl Theme {
    pub fn dark() -> Self {
        Self { light: false }
    }

    pub fn light() -> Self {
        Self { light: true }
    }

    /// `auto` trusts the terminal's OSC 11 answer (`osc11`), then `COLORFGBG`.
    pub fn resolve(setting: Option<&str>, colorfgbg: Option<&str>, osc11: Option<bool>) -> Self {
        match setting {
            Some("light") => Self::light(),
            Some("auto") if osc11 == Some(true) => Self::light(),
            Some("auto") if osc11 == Some(false) => Self::dark(),
            Some("auto")
                if colorfgbg
                    .and_then(|v| v.rsplit(';').next())
                    .and_then(|v| v.parse::<u16>().ok())
                    .is_some_and(|bg| matches!(bg, 7 | 15 | 255)) =>
            {
                Self::light()
            }
            _ => Self::dark(),
        }
    }

    /// The user-message bubble: the line's own fg over a contrasty bg + bold, so
    /// it never blends into the agent's text. Wrapped rows all carry it, forming
    /// one rectangle hugging the right margin.
    pub fn user_bubble(self, base: Style) -> Style {
        if self.light {
            base.bg(Color::Rgb(205, 222, 240))
                .add_modifier(Modifier::BOLD)
        } else {
            base.bg(Color::Rgb(31, 58, 78)).add_modifier(Modifier::BOLD)
        }
    }

    pub fn style(self, tone: Tone) -> Style {
        match (self.light, tone) {
            (_, Tone::Muted) => Style::new().fg(Color::DarkGray),
            (_, Tone::Thinking) => Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
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
            (true, Tone::Text) => Style::new().fg(Color::Black),
            (false, Tone::Text) => Style::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_explicit_and_fallback_themes() {
        assert!(!Theme::resolve(Some("dark"), Some("0;15"), None).light);
        assert!(Theme::resolve(Some("light"), Some("15;0"), None).light);
        assert!(!Theme::resolve(Some("bogus"), Some("0;15"), None).light);
        assert!(!Theme::resolve(None, Some("0;15"), None).light);
    }

    #[test]
    fn auto_uses_light_terminal_backgrounds() {
        for value in [Some("0;7"), Some("0;15"), Some("0;255")] {
            assert!(Theme::resolve(Some("auto"), value, None).light);
        }
        for value in [Some("15;0"), Some("0;8"), Some("junk"), None] {
            assert!(!Theme::resolve(Some("auto"), value, None).light);
        }
    }
}
