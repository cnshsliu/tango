use std::io::{self, Write};
use std::process::{Command, Stdio};

/// How copies reach a clipboard: `auto` uses OSC 52 over SSH (the local
/// clipboard is the one that matters) and the native command otherwise, falling
/// back to OSC 52 when no command works; `osc52` / `native` force one path.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Auto,
    Osc52,
    Native,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Mode::Auto),
            "osc52" => Some(Mode::Osc52),
            "native" => Some(Mode::Native),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Osc52 => "osc52",
            Mode::Native => "native",
        }
    }
}

static MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_mode(mode: Mode) {
    MODE.store(mode as u8, std::sync::atomic::Ordering::Relaxed);
}

pub fn mode() -> Mode {
    match MODE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Mode::Osc52,
        2 => Mode::Native,
        _ => Mode::Auto,
    }
}

fn remote() -> bool {
    ["SSH_TTY", "SSH_CONNECTION", "SSH_CLIENT"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

pub fn copy(text: &str) -> io::Result<()> {
    match mode() {
        Mode::Osc52 => osc52(text),
        Mode::Native => native(text),
        Mode::Auto if remote() => osc52(text),
        Mode::Auto => native(text).or_else(|_| osc52(text)),
    }
}

fn native(text: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let candidates = ["pbcopy"];
    #[cfg(target_os = "windows")]
    let candidates = ["clip"]; // clip.exe: the clipboard of every Windows since Vista
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates = [
        "wl-copy",
        "xclip -selection clipboard",
        "xsel --clipboard --input",
    ];
    copy_with(&candidates, text)
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The OSC 52 "set clipboard" sequence for `text`. Inside tmux it is wrapped in
/// tmux's DCS passthrough (inner ESCs doubled; needs `allow-passthrough on` or
/// `set-clipboard on`), inside GNU screen in screen's DCS.
fn osc52_sequence(text: &str, tmux: bool, screen: bool) -> String {
    let osc = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    if tmux {
        format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b"))
    } else if screen {
        format!("\x1bP{osc}\x1b\\")
    } else {
        osc
    }
}

/// Copy through the terminal itself: works over SSH and in tmux, as long as the
/// terminal honors OSC 52 (most do; some ask first or cap the size). Written to
/// /dev/tty so it never interleaves with a half-flushed frame on stdout.
fn osc52(text: &str) -> io::Result<()> {
    let tmux = std::env::var_os("TMUX").is_some();
    let screen = !tmux
        && std::env::var("TERM").is_ok_and(|t| t.starts_with("screen"))
        && std::env::var_os("STY").is_some();
    let seq = osc52_sequence(text, tmux, screen);
    let mut tty = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/tty")
        .or_else(|_| std::fs::OpenOptions::new().write(true).open("/dev/stdout"))?;
    tty.write_all(seq.as_bytes())?;
    tty.flush()
}

fn copy_with(candidates: &[&str], text: &str) -> io::Result<()> {
    for candidate in candidates {
        let mut parts = candidate.split_whitespace();
        let Some(program) = parts.next() else {
            continue;
        };
        let result = (|| {
            let mut child = Command::new(program)
                .args(parts)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            child.stdin.take().unwrap().write_all(text.as_bytes())?;
            let status = child.wait()?;
            if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(format!("{program} exited with {status}")))
            }
        })();
        if result.is_ok() {
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no working clipboard command",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_failures_are_reported() {
        assert!(copy_with(&["/definitely/missing/pbcopy"], "text").is_err());
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "{input:?}");
        }
        assert_eq!(base64("中".as_bytes()), "5Lit");
    }

    #[test]
    fn osc52_sequences_for_plain_tmux_and_screen() {
        assert_eq!(osc52_sequence("hi", false, false), "\x1b]52;c;aGk=\x07");
        assert_eq!(
            osc52_sequence("hi", true, false),
            "\x1bPtmux;\x1b\x1b]52;c;aGk=\x07\x1b\\",
            "tmux passthrough doubles the inner ESC"
        );
        assert_eq!(osc52_sequence("hi", false, true), "\x1bP\x1b]52;c;aGk=\x07\x1b\\");
    }

    #[test]
    fn mode_names_round_trip() {
        for m in [Mode::Auto, Mode::Osc52, Mode::Native] {
            assert_eq!(Mode::parse(m.name()), Some(m));
        }
        assert_eq!(Mode::parse("x"), None);
    }
}
