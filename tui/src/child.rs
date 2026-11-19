use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

/// Everything the main loop receives: protocol frames, child stderr, and terminal input.
/// Server/Gone carry the child epoch so frames from a killed serve (after a session
/// switch) can be filtered out before they reach the app.
pub enum Event {
    Server(u64, Value),
    Stderr(String),
    Key(crossterm::event::KeyEvent),
    Mouse(crossterm::event::MouseEvent),
    Resized(u16, u16),
    /// bracketed paste: the whole clipboard as one event, newlines kept, never "Enter"
    Paste(String),
    Gone(u64), // the serve child of this epoch exited
    /// once a second, so elapsed timers / rotating tips redraw during silent turns
    Tick,
}

/// Owns the `tango serve` child process: JSONL out, frames in.
pub struct Serve {
    child: Child,
    stdin: std::process::ChildStdin,
    next_id: u64,
    pub epoch: u64,
    #[cfg(test)]
    pub sent: Vec<String>,
}

impl Serve {
    /// `resume` is injected as TANGO_RESUME so the child opens that session instead of a new one;
    /// `mode` is set only on respawns (None on the first spawn keeps the launch environment).
    pub fn spawn(
        tx: Sender<Event>,
        epoch: u64,
        resume: Option<&str>,
        mode: Option<&str>,
        model: Option<&str>,
    ) -> std::io::Result<Self> {
        // TANGO_BIN can hold args too, e.g. `node /path/to/bin/tango.js` for dev builds.
        let bin = std::env::var("TANGO_BIN").unwrap_or_else(|_| "tango".into());
        let mut parts = bin.split_whitespace();
        let mut cmd = Command::new(parts.next().unwrap_or("tango"));
        for a in parts {
            cmd.arg(a);
        }
        cmd.arg("serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(id) = resume {
            cmd.env("TANGO_RESUME", id);
        } else if mode.is_some() {
            cmd.env_remove("TANGO_RESUME"); // respawn for /new: a fresh session
        }
        if let Some(m) = mode {
            // respawns keep the live permission mode; the model pin is only carried into /new —
            // a resumed session brings back its own model (or the saved default)
            cmd.env("TANGO_MODE", m);
            match model {
                Some(id) => cmd.env("TANGO_MODEL", id),
                None => cmd.env_remove("TANGO_MODEL"),
            };
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        thread::spawn({
            let tx = tx.clone();
            move || {
                let mut r = BufReader::new(stdout);
                let mut line = String::new();
                while r.read_line(&mut line).unwrap_or(0) > 0 {
                    if let Ok(v) = serde_json::from_str::<Value>(line.trim())
                        && tx.send(Event::Server(epoch, v)).is_err()
                    {
                        return; // main loop gone
                    }
                    line.clear();
                }
                let _ = tx.send(Event::Gone(epoch));
            }
        });
        thread::spawn({
            let tx = tx.clone();
            move || {
                let mut r = BufReader::new(stderr);
                let mut line = String::new();
                while r.read_line(&mut line).unwrap_or(0) > 0 {
                    let l = line.trim_end().to_string();
                    if !l.is_empty() && tx.send(Event::Stderr(l)).is_err() {
                        return;
                    }
                    line.clear();
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            next_id: epoch * 1_000_000,
            epoch,
            #[cfg(test)]
            sent: Vec::new(),
        })
    }

    #[cfg(test)]
    pub fn mock() -> Self {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self {
            stdin: child.stdin.take().unwrap(),
            child,
            next_id: 0,
            epoch: 0,
            sent: Vec::new(),
        }
    }

    /// Send a command that expects a `result` frame; returns its id.
    pub fn command(&mut self, cmd: &str, extra: Vec<(&str, Value)>) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let mut v = json!({ "id": id, "cmd": cmd });
        if let Some(m) = v.as_object_mut() {
            for (k, val) in extra {
                m.insert(k.into(), val);
            }
        }
        self.write_line(&v.to_string());
        id
    }

    /// Fire-and-forget frames: interrupt and permission answers carry no id.
    pub fn notify(&mut self, frame: Value) {
        self.write_line(&frame.to_string());
    }

    fn write_line(&mut self, line: &str) {
        #[cfg(test)]
        self.sent.push(line.to_string());
        let _ = writeln!(self.stdin, "{line}");
        let _ = self.stdin.flush();
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        // reap it: an unreaped child stays a zombie until the TUI exits, and the next serve's
        // lock check (`process.kill(pid, 0)`) treats a zombie as a live concurrent session
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_serve_reaps_the_child() {
        let serve = Serve::mock();
        let pid = serve.child.id().to_string();
        drop(serve);
        // `kill -0` succeeds on a zombie; it must fail once the child is reaped
        let alive = Command::new("kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(
            !alive.success(),
            "serve child {pid} was killed but not reaped (zombie)"
        );
    }
}
