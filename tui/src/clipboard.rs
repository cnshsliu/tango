use std::io::{self, Write};
use std::process::{Command, Stdio};

pub fn copy(text: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    let candidates = ["pbcopy"];
    #[cfg(not(target_os = "macos"))]
    let candidates = [
        "wl-copy",
        "xclip -selection clipboard",
        "xsel --clipboard --input",
    ];
    copy_with(&candidates, text)
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
}
