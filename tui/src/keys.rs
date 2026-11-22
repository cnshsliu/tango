use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;

#[derive(Clone, Copy, PartialEq)]
pub enum Action {
    ExpandTools,
    EditLast,
    ToggleThinking,
    Newline,
    Interrupt,
    ClearInput,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    PromptPrev,
    PromptNext,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::ExpandTools => "expandTools",
            Action::EditLast => "editLast",
            Action::ToggleThinking => "toggleThinking",
            Action::Newline => "newline",
            Action::Interrupt => "interrupt",
            Action::ClearInput => "clearInput",
            Action::ScrollUp => "scrollUp",
            Action::ScrollDown => "scrollDown",
            Action::PageUp => "pageUp",
            Action::PageDown => "pageDown",
            Action::PromptPrev => "promptPrev",
            Action::PromptNext => "promptNext",
        }
    }
}

impl std::fmt::Display for Binding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            write!(f, "ctrl+")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            write!(f, "alt+")?;
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            write!(f, "shift+")?;
        }
        match self.code {
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Enter => write!(f, "enter"),
            KeyCode::Up => write!(f, "up"),
            KeyCode::Down => write!(f, "down"),
            KeyCode::PageUp => write!(f, "pageup"),
            KeyCode::PageDown => write!(f, "pagedown"),
            KeyCode::Esc => write!(f, "esc"),
            other => write!(f, "{other:?}"),
        }
    }
}

struct Binding {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl Binding {
    fn parse(value: &str) -> Option<Self> {
        let mut modifiers = KeyModifiers::NONE;
        let normalized = value.trim().to_ascii_lowercase();
        let mut parts = normalized.split('+').collect::<Vec<_>>();
        let key = parts.pop()?;
        for part in parts {
            match part.trim() {
                "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                "alt" => modifiers |= KeyModifiers::ALT,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                _ => return None,
            }
        }
        let code = match key.trim() {
            "esc" => KeyCode::Esc,
            "enter" => KeyCode::Enter,
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "pageup" => KeyCode::PageUp,
            "pagedown" => KeyCode::PageDown,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "space" => KeyCode::Char(' '),
            "tab" => KeyCode::Tab,
            "backspace" => KeyCode::Backspace,
            "delete" => KeyCode::Delete,
            one if one.chars().count() == 1 => KeyCode::Char(one.chars().next()?),
            _ => return None,
        };
        Some(Self { code, modifiers })
    }

    fn matches(&self, key: KeyEvent) -> bool {
        self.code == key.code && self.modifiers == key.modifiers
    }
}

pub struct Keymap {
    bindings: [(Action, Binding); 12],
}

impl Keymap {
    /// human-readable (action, key) pairs for /keymap
    pub fn summary(&self) -> Vec<(&'static str, String)> {
        self.bindings
            .iter()
            .map(|(action, binding)| (action.name(), binding.to_string()))
            .collect()
    }

    /// the key currently bound to `action`, as shown in hints (e.g. "alt+t")
    pub fn label(&self, action: Action) -> String {
        self.bindings
            .iter()
            .find(|(a, _)| *a == action)
            .map(|(_, b)| b.to_string())
            .unwrap_or_default()
    }

    pub fn from_json(config: Option<&Value>) -> Self {
        let ctrl = |c| Binding {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::CONTROL,
        };
        let plain = |code| Binding {
            code,
            modifiers: KeyModifiers::NONE,
        };
        let alt = |c| Binding {
            code: KeyCode::Char(c),
            modifiers: KeyModifiers::ALT,
        };
        let mut keys = Self {
            bindings: [
                (Action::ExpandTools, ctrl('o')),
                (Action::EditLast, alt('e')),
                (Action::ToggleThinking, alt('t')),
                (
                    Action::Newline,
                    Binding {
                        code: KeyCode::Enter,
                        modifiers: KeyModifiers::ALT,
                    },
                ),
                (Action::Interrupt, ctrl('c')),
                (Action::ClearInput, plain(KeyCode::Esc)),
                (Action::ScrollUp, plain(KeyCode::Up)),
                (Action::ScrollDown, plain(KeyCode::Down)),
                (Action::PageUp, plain(KeyCode::PageUp)),
                (Action::PageDown, plain(KeyCode::PageDown)),
                (
                    Action::PromptPrev,
                    Binding {
                        code: KeyCode::Up,
                        modifiers: KeyModifiers::ALT,
                    },
                ),
                (
                    Action::PromptNext,
                    Binding {
                        code: KeyCode::Down,
                        modifiers: KeyModifiers::ALT,
                    },
                ),
            ],
        };
        let names = [
            "expandTools",
            "editLast",
            "toggleThinking",
            "newline",
            "interrupt",
            "clearInput",
            "scrollUp",
            "scrollDown",
            "pageUp",
            "pageDown",
            "promptPrev",
            "promptNext",
        ];
        if let Some(object) = config.and_then(Value::as_object) {
            for (slot, name) in keys.bindings.iter_mut().zip(names) {
                if let Some(binding) = object
                    .get(name)
                    .and_then(Value::as_str)
                    .and_then(Binding::parse)
                {
                    slot.1 = binding;
                }
            }
        }
        keys
    }

    pub fn matches(&self, action: Action, key: KeyEvent) -> bool {
        let custom = self
            .bindings
            .iter()
            .any(|(a, b)| *a == action && b.matches(key));
        custom
            || (matches!(action, Action::Interrupt)
                && key.code == KeyCode::Char('c')
                && key.modifiers == KeyModifiers::CONTROL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;

    #[test]
    fn defaults_match_existing_bindings() {
        let keys = Keymap::from_json(None);
        assert!(keys.matches(
            Action::ExpandTools,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)
        ));
        assert!(keys.matches(
            Action::Newline,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
        ));
        assert!(keys.matches(
            Action::Interrupt,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ));
        assert!(keys.matches(
            Action::ClearInput,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)
        ));
        assert!(keys.matches(
            Action::PageUp,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)
        ));
    }

    #[test]
    fn custom_binding_replaces_default_but_ctrl_c_interrupt_is_safe() {
        let keys = Keymap::from_json(Some(
            &json!({ "expandTools": "ctrl+u", "interrupt": "ctrl+x" }),
        ));
        assert!(keys.matches(
            Action::ExpandTools,
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
        ));
        assert!(!keys.matches(
            Action::ExpandTools,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)
        ));
        assert!(keys.matches(
            Action::Interrupt,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
        ));
        assert!(keys.matches(
            Action::Interrupt,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn malformed_values_fall_back_to_defaults() {
        let keys = Keymap::from_json(Some(
            &json!({ "expandTools": "not a key", "newline": 42, "unknown": "ctrl+z" }),
        ));
        assert!(keys.matches(
            Action::ExpandTools,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)
        ));
        assert!(keys.matches(
            Action::Newline,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
        ));
    }
}
