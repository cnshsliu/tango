//! Terminal background detection for `theme: auto`: ask with OSC 11
//! (`ESC ]11;? BEL`), followed by DA1 (`CSI c`) as a sentinel — every terminal
//! answers DA1, so a terminal without OSC 11 ends the wait at once instead of
//! at the timeout. The reply is read straight from stdin, so callers must make
//! sure crossterm's input thread is not reading at the same time (it would turn
//! the reply into Alt+] and typed text).

/// Parse an OSC 11 reply (`ESC ]11;rgb:RRRR/GGGG/BBBB` or `#rrggbb`, ended by
/// BEL or ST) anywhere in `buf` into 8-bit RGB.
pub fn parse_reply(buf: &[u8]) -> Option<(u8, u8, u8)> {
    let s = String::from_utf8_lossy(buf);
    let start = s.find("\x1b]11;")? + 5;
    let body = &s[start..];
    let end = body.find(['\x07', '\x1b'])?;
    let body = &body[..end];
    if let Some(hex) = body.strip_prefix('#') {
        if hex.len() != 6 {
            return None;
        }
        let c = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        return Some((c(0)?, c(2)?, c(4)?));
    }
    let rgb = body.strip_prefix("rgb:").or_else(|| body.strip_prefix("rgba:"))?;
    let mut parts = rgb.split('/').map(scale);
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// 1–4 hex digits scaled to 0–255 (`f` → 255, `ffff` → 255, `80` → 128).
fn scale(component: &str) -> Option<u8> {
    if component.is_empty() || component.len() > 4 {
        return None;
    }
    let v = u32::from_str_radix(component, 16).ok()?;
    let max = (1u32 << (4 * component.len())) - 1;
    Some(((v * 255 + max / 2) / max) as u8)
}

/// Perceived brightness (ITU-R 601 weights, as OpenTUI does) above the midpoint.
pub fn is_light((r, g, b): (u8, u8, u8)) -> bool {
    (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000 > 128
}

/// True once `buf` holds a DA1 reply (`CSI ? … c`): the terminal has answered
/// everything it is going to.
fn has_da1(buf: &[u8]) -> bool {
    let s = String::from_utf8_lossy(buf);
    s.match_indices("\x1b[?").any(|(i, _)| {
        s[i + 3..]
            .chars()
            .find(|c| !(c.is_ascii_digit() || *c == ';'))
            == Some('c')
    })
}

/// Ask the terminal for its background and wait up to `timeout` for the
/// reply. `None` when the terminal does not answer OSC 11 (or stdin is not a
/// tty). Raw mode must be on.
static DETECTED: std::sync::OnceLock<Option<bool>> = std::sync::OnceLock::new();

/// Run the startup query once and remember the answer for `detected()`.
pub fn detect_once(timeout: std::time::Duration) {
    DETECTED.get_or_init(|| query(timeout));
}

/// The startup answer: `Some(true)` light, `Some(false)` dark, `None` unknown.
pub fn detected() -> Option<bool> {
    DETECTED.get().copied().flatten()
}

#[cfg(unix)]
pub fn query(timeout: std::time::Duration) -> Option<bool> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    if unsafe { libc::isatty(fd) } != 1 {
        return None;
    }
    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x07\x1b[c").ok()?;
    out.flush().ok()?;
    let deadline = std::time::Instant::now() + timeout;
    let mut buf = Vec::new();
    while !has_da1(&buf) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis().max(1) as libc::c_int) } <= 0 {
            break;
        }
        let mut chunk = [0u8; 256];
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n as usize]);
    }
    parse_reply(&buf).map(is_light)
}

#[cfg(not(unix))]
pub fn query(_timeout: std::time::Duration) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xterm_and_hex_replies() {
        assert_eq!(parse_reply(b"\x1b]11;rgb:ffff/ffff/ffff\x07"), Some((255, 255, 255)));
        assert_eq!(parse_reply(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"), Some((30, 30, 46)));
        assert_eq!(parse_reply(b"\x1b]11;rgb:f/8/0\x07"), Some((255, 136, 0)));
        assert_eq!(parse_reply(b"\x1b]11;#FDF6E3\x07"), Some((253, 246, 227)));
        // junk around it (DA1 reply, stray keys) is fine
        assert_eq!(parse_reply(b"x\x1b]11;rgb:00/00/00\x07\x1b[?62;22c"), Some((0, 0, 0)));
        assert_eq!(parse_reply(b"\x1b[?62;22c"), None);
        assert_eq!(parse_reply(b"\x1b]11;rgb:zz/00/00\x07"), None);
        assert_eq!(parse_reply(b"\x1b]11;rgb:ff/ff"), None, "unterminated");
    }

    #[test]
    fn brightness_splits_light_from_dark() {
        assert!(is_light((255, 255, 255)));
        assert!(is_light((253, 246, 227))); // solarized light
        assert!(!is_light((0, 43, 54))); // solarized dark
        assert!(!is_light((30, 30, 46)));
    }

    #[test]
    fn da1_ends_the_wait() {
        assert!(has_da1(b"\x1b]11;rgb:0/0/0\x07\x1b[?62;22;52c"));
        assert!(has_da1(b"\x1b[?1;2c"));
        assert!(!has_da1(b"\x1b]11;rgb:0/0/0\x07"));
        assert!(!has_da1(b"\x1b[?62;22"));
    }
}
