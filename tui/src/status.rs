//! Program Status Protocol (OSC 7501): tell the hosting terminal whether tango is at its
//! prompt, working, or waiting on the user. Orca reads this to decide when it may type a
//! mail note into the pane. Spec: https://www.superlogical.com/rex/docs/build/program-status
//! Terminals that don't know the sequence must ignore it, so it is always sent.

/// What tango reports about itself (the root record).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramStatus {
    /// at the input box, waiting for the next prompt
    Idle,
    /// a turn is running
    Working,
    /// a permission dialog (y/n/a) is open
    BlockedPermission,
    /// a ?choice panel is open
    BlockedQuestion,
    /// not reporting: Neovim owns the tty, or tango is exiting
    Clear,
}

impl ProgramStatus {
    /// The escape sequence for this report. Every report repeats `app` because each one
    /// replaces the record whole.
    pub fn sequence(self) -> &'static str {
        match self {
            ProgramStatus::Idle => "\x1b]7501;state=idle:app=tango\x1b\\",
            ProgramStatus::Working => "\x1b]7501;state=working:app=tango\x1b\\",
            ProgramStatus::BlockedPermission => {
                "\x1b]7501;state=blocked:kind=permission:app=tango\x1b\\"
            }
            ProgramStatus::BlockedQuestion => {
                "\x1b]7501;state=blocked:kind=question:app=tango\x1b\\"
            }
            ProgramStatus::Clear => "\x1b]7501;state=clear\x1b\\",
        }
    }
}

/// Emits a report only when the state changes.
#[derive(Default)]
pub struct Reporter {
    last: Option<ProgramStatus>,
}

impl Reporter {
    /// The sequence to write now, or None when the terminal already has this state.
    pub fn update(&mut self, now: ProgramStatus) -> Option<&'static str> {
        if self.last == Some(now) {
            return None;
        }
        self.last = Some(now);
        Some(now.sequence())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_follow_the_osc_7501_grammar() {
        for s in [
            ProgramStatus::Idle,
            ProgramStatus::Working,
            ProgramStatus::BlockedPermission,
            ProgramStatus::BlockedQuestion,
            ProgramStatus::Clear,
        ] {
            let seq = s.sequence();
            let body = seq
                .strip_prefix("\x1b]7501;")
                .and_then(|b| b.strip_suffix("\x1b\\"))
                .expect("OSC 7501 ... ST");
            for pair in body.split(':') {
                let (k, v) = pair.split_once('=').expect("key=value");
                assert!(
                    !k.is_empty() && k.bytes().all(|b| b.is_ascii_lowercase()),
                    "{pair}"
                );
                assert!(
                    v.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b))
                );
            }
        }
        assert!(
            ProgramStatus::BlockedPermission
                .sequence()
                .contains("kind=permission")
        );
        assert!(
            ProgramStatus::BlockedQuestion
                .sequence()
                .contains("kind=question")
        );
    }

    #[test]
    fn reporter_writes_only_on_change() {
        let mut r = Reporter::default();
        assert!(r.update(ProgramStatus::Idle).is_some());
        assert!(r.update(ProgramStatus::Idle).is_none());
        assert!(r.update(ProgramStatus::Working).is_some());
        assert!(r.update(ProgramStatus::Idle).is_some());
    }
}
