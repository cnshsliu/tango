mod app;
mod child;
mod clipboard;
mod dialog;
mod keys;
mod launch;
mod nvim;
mod theme;
mod viewer;

use app::App;
use child::{Event, Serve};
use crossterm::{
    event::{self, Event as CEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::layout::Rect;
use ratatui::{Terminal, backend::CrosstermBackend};
use std::error::Error;
use std::io::Stdout;
use std::sync::mpsc;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;

/// One frame inside DEC 2026 synchronized output (`CSI ?2026h … ?2026l`): the
/// terminal holds the old screen until the whole frame has arrived and swaps it
/// in at once, so streaming/animation redraws never show half-painted rows.
/// Terminals without 2026 ignore both sequences.
fn draw_synced<W: std::io::Write>(
    terminal: &mut Terminal<CrosstermBackend<W>>,
    app: &mut App,
) -> std::io::Result<()> {
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
    crossterm::queue!(terminal.backend_mut(), BeginSynchronizedUpdate)?;
    let drawn = terminal.draw(|f| app.draw(f)).map(|_| ());
    // always close the update, even if drawing failed, or the screen freezes
    execute!(terminal.backend_mut(), EndSynchronizedUpdate)?;
    drawn
}

fn main() -> Result<(), Box<dyn Error>> {
    entry();
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::cursor::Hide,
        crossterm::event::EnableMouseCapture,
        crossterm::event::EnableBracketedPaste
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run(&mut terminal);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture,
        crossterm::cursor::Show,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    match result {
        Err(e) => {
            eprintln!("tango-tui: {e}");
            std::process::exit(1);
        }
        Ok(Exit::Restart(session, mode)) => restart(&session, &mode),
        Ok(Exit::Quit(Some(note))) => {
            println!("{note}");
        }
        Ok(Exit::Quit(None)) => {}
    }
    Ok(())
}

enum Exit {
    Quit(Option<String>),
    /// /restart: (session id, permission mode) for the fresh process
    Restart(String, String),
}

/// `tango` on PATH is this binary: interactive launches fall through to the TUI,
/// everything else is exec'd into the Node CLI (src/cli.ts) with the same args.
fn entry() {
    use std::io::IsTerminal;
    let raw: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let root = launch::repo_root();
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let utf8: Option<Vec<String>> = raw.iter().map(|a| a.to_str().map(String::from)).collect();
    match utf8.and_then(|args| launch::parse(&args, tty)) {
        Some(opts) => {
            if let Err(e) = launch::apply(&opts, root.as_deref()) {
                eprintln!("\x1b[31m{e}\x1b[0m");
                std::process::exit(1);
            }
        }
        None => match &root {
            Some(root) => launch::exec_node(root, &raw),
            None => {
                eprintln!("tango: bin/tango.js not found next to this binary (expected <repo>/tui/target/release/)");
                std::process::exit(1);
            }
        },
    }
}

/// Replace this process with a fresh copy of the TUI binary (picks up a rebuilt
/// executable; its serve child picks up a rebuilt dist/) resuming the same session.
fn restart(session: &str, mode: &str) {
    let fail = |e: &dyn std::fmt::Display| {
        eprintln!("tango-tui: restart failed: {e}\n恢复：tango --resume {session}");
        std::process::exit(1);
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };
    let mut cmd = std::process::Command::new(exe);
    // launch args were applied at startup (cwd, TANGO_* env); `tui` keeps the
    // fresh copy in the TUI and the env below picks the session and mode
    cmd.arg("tui")
        .env("TANGO_RESUME", session)
        .env("TANGO_MODE", mode)
        // a resumed session brings back its own model
        .env_remove("TANGO_MODEL");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let e = cmd.exec(); // only returns on failure
        fail(&e);
    }
    #[cfg(not(unix))]
    match cmd.status() {
        Ok(s) => std::process::exit(s.code().unwrap_or(1)),
        Err(e) => fail(&e),
    }
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<Exit, Box<dyn Error>> {
    let (tx, rx) = mpsc::channel::<Event>();
    let mut serve = Serve::spawn(tx.clone(), 0, None, None, None)?;
    let input_enabled = Arc::new(AtomicBool::new(true));
    // while a turn runs, the heartbeat speeds up (~8fps) so the thinking bar's
    // color wave can scroll; idle stays at the cheap 1s heartbeat
    let animating = Arc::new(AtomicBool::new(false));

    // crossterm has no pollable fd we own: a thread forwards terminal input into the same queue
    thread::spawn({
        let tx = tx.clone();
        let input_enabled = input_enabled.clone();
        let animating = animating.clone();
        move || {
            let mut last_tick = std::time::Instant::now();
            loop {
                // heartbeat: 1s while idle (timers and rotating tips), or ~120ms
                // while animating (the thinking bar's scrolling color wave)
                let beat = if animating.load(Ordering::Relaxed) {
                    Duration::from_millis(120)
                } else {
                    Duration::from_secs(1)
                };
                if last_tick.elapsed() >= beat {
                    last_tick = std::time::Instant::now();
                    let _ = tx.send(Event::Tick);
                }
                if !input_enabled.load(Ordering::Relaxed) {
                    thread::sleep(beat.min(Duration::from_millis(50)));
                    continue;
                }
                match event::poll(beat.min(Duration::from_millis(200))) {
                    Ok(true) => match event::read() {
                        Ok(CEvent::Key(k)) => {
                            if tx.send(Event::Key(k)).is_err() {
                                return;
                            }
                        }
                        Ok(CEvent::Mouse(m)) => {
                            if tx.send(Event::Mouse(m)).is_err() {
                                return;
                            }
                        }
                        Ok(CEvent::Resize(w, h)) => {
                            if tx.send(Event::Resized(w, h)).is_err() {
                                return;
                            }
                        }
                        Ok(CEvent::Paste(s)) => {
                            if tx.send(Event::Paste(s)).is_err() {
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => return,
                    },
                    Ok(false) => {}
                    Err(_) => return, // terminal gone; the main loop will notice via draw
                }
            }
        }
    });

    let mut app = App::new();
    animating.store(app.busy && app.animations, Ordering::Relaxed);
    loop {
        draw_synced(terminal, &mut app)?;
        match rx.recv()? {
            // stale frames from a killed serve child (session switch): drop by epoch
            Event::Server(epoch, _) if epoch != serve.epoch => {}
            Event::Gone(epoch) if epoch != serve.epoch => {}
            // idle heartbeat only when nothing else is happening — no busy redraw loop
            Event::Resized(w, h) => {
                terminal.resize(Rect::new(0, 0, w, h))?;
                let _ = app.handle(Event::Resized(w, h), &mut serve);
            }
            ev => {
                if app.handle(ev, &mut serve) || app.quit {
                    break;
                }
            }
        }
        animating.store(app.busy && app.animations, Ordering::Relaxed);
        if let Some(enabled) = app.mouse_capture.take() {
            if enabled {
                execute!(terminal.backend_mut(), crossterm::event::EnableMouseCapture)?;
            } else {
                execute!(
                    terminal.backend_mut(),
                    crossterm::event::DisableMouseCapture
                )?;
            }
        }
        if let Some(request) = app.nvim_request.take() {
            input_enabled.store(false, Ordering::Relaxed);
            // let the input thread finish its current poll before Neovim takes the tty
            thread::sleep(Duration::from_millis(220));
            disable_raw_mode()?;
            execute!(
                terminal.backend_mut(),
                crossterm::event::DisableBracketedPaste,
                crossterm::event::DisableMouseCapture,
                crossterm::cursor::Show,
                LeaveAlternateScreen
            )?;
            let result = if request.plain {
                crate::nvim::run_plain(&request.editor.executable, &request.viewer.text)
            } else {
                crate::nvim::run(&request.editor, &request.viewer.text, request.read_only)
            };
            enable_raw_mode()?;
            execute!(
                terminal.backend_mut(),
                EnterAlternateScreen,
                crossterm::cursor::Hide,
                crossterm::event::EnableBracketedPaste
            )?;
            if app.mouse_enabled() {
                execute!(terminal.backend_mut(), crossterm::event::EnableMouseCapture)?;
            }
            terminal.clear()?;
            input_enabled.store(true, Ordering::Relaxed);
            match result {
                Ok(text) => match request.target {
                    nvim::NvimTarget::Draft => app.finish_draft_nvim(&request.viewer.text, text),
                    nvim::NvimTarget::Viewer => {
                        app.finish_nvim(request.viewer, text, request.read_only)
                    }
                },
                Err(error) => match request.target {
                    nvim::NvimTarget::Draft => {
                        app.nvim_draft_error(&request.viewer.text, error.to_string())
                    }
                    nvim::NvimTarget::Viewer => app.nvim_error(request.viewer, error.to_string()),
                },
            }
        }
        // the app asked for another session (/resume) or a fresh one (/new): respawn serve
        if let Some(target) = app.respawn.take() {
            let mode = app.mode().to_string();
            let model = if target.is_none() {
                Some(app.model().to_string())
            } else {
                None
            };
            let next = Serve::spawn(
                tx.clone(),
                serve.epoch + 1,
                target.as_deref(),
                Some(&mode),
                model.as_deref(),
            )?;
            let _old = std::mem::replace(&mut serve, next); // drop kills the previous child
        }
    }
    if let Some(e) = app.err {
        Err(e.into())
    } else if let Some((session, mode)) = app.restart.take() {
        drop(serve); // kill + reap the old serve so its session lock is free for the new one
        Ok(Exit::Restart(session, mode))
    } else {
        // hand the user the way back before the process goes away
        Ok(Exit::Quit(app.exit_note()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn every_frame_is_wrapped_in_synchronized_output() {
        let buf = Shared::default();
        let options = ratatui::TerminalOptions {
            viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, 40, 10)),
        };
        let mut terminal =
            Terminal::with_options(CrosstermBackend::new(buf.clone()), options).unwrap();
        let mut app = App::new();
        draw_synced(&mut terminal, &mut app).unwrap();
        let out = String::from_utf8_lossy(&buf.0.lock().unwrap()).to_string();
        let begin = out.find("\x1b[?2026h").expect("begin sync");
        let end = out.rfind("\x1b[?2026l").expect("end sync");
        assert_eq!(begin, 0, "nothing painted before the update opens");
        assert!(out[begin..end].len() > 20, "the frame's cells are inside");
        assert_eq!(end + "\x1b[?2026l".len(), out.len(), "update closes last");
    }
}
