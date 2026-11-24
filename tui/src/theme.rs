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
    /// a /subagent's dispatch and write-back lines: set apart from the main agent's text
    Subagent,
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

    /// The input block's tint: one step off the terminal background.
    pub fn input_bg(self) -> Color {
        if self.light { Color::Rgb(236, 236, 236) } else { Color::Rgb(30, 30, 30) }
    }

    /// Accent for a permission mode (input bar, info row): plan orange, confirm
    /// purple, auto red, readonly gray, ask blue. Plan wins: it vetoes writes whatever the mode says.
    pub fn mode_color(self, mode: &str, plan: bool) -> Color {
        let pick = |dark: (u8, u8, u8), light: (u8, u8, u8)| {
            let (r, g, b) = if self.light { light } else { dark };
            Color::Rgb(r, g, b)
        };
        if plan {
            return pick((245, 167, 110), (190, 100, 20));
        }
        match mode {
            "confirm" => pick((198, 120, 221), (150, 60, 180)),
            "auto" => pick((224, 108, 117), (190, 40, 50)),
            "readonly" => pick((140, 140, 140), (110, 110, 110)),
            _ => pick((97, 175, 239), (30, 100, 200)),
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
            (false, Tone::Subagent) => Style::new().fg(Color::Rgb(170, 150, 240)),
            (true, Tone::Subagent) => Style::new().fg(Color::Rgb(100, 70, 180)),
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
