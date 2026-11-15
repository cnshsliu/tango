mod app;
mod child;

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
use std::thread;
use std::time::Duration;

fn main() -> Result<(), Box<dyn Error>> {
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
    if let Err(e) = result {
        eprintln!("tango-tui: {e}");
        std::process::exit(1);
    }
    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<(), Box<dyn Error>> {
    let (tx, rx) = mpsc::channel::<Event>();
    let mut serve = Serve::spawn(tx.clone(), 0, None, None, None)?;

    // crossterm has no pollable fd we own: a thread forwards terminal input into the same queue
    thread::spawn({
        let tx = tx.clone();
        move || {
        loop {
            match event::poll(Duration::from_millis(200)) {
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
    loop {
        terminal.draw(|f| app.draw(f))?;
        match rx.recv()? {
            // stale frames from a killed serve child (session switch): drop by epocheration
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
        // the app asked for another session (/resume) or a fresh one (/new): respawn serve
        if let Some(target) = app.respawn.take() {
            let mode = app.mode().to_string();
            let model = if target.is_none() { Some(app.model().to_string()) } else { None };
            let next = Serve::spawn(tx.clone(), serve.epoch + 1, target.as_deref(), Some(&mode), model.as_deref())?;
            let _old = std::mem::replace(&mut serve, next); // drop kills the previous child
        }
    }
    if let Some(e) = app.err {
        Err(e.into())
    } else {
        Ok(())
    }
}
