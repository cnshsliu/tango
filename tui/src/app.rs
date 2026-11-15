use crate::child::{Event, Serve};
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use serde_json::{Value, json};
use std::cmp::min;

/// Rough cell width: CJK/fullwidth codepoints take 2 cells. Good enough for scroll math and the cursor.
pub fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| {
            let u = c as u32;
            if (0x1100..=0x115F).contains(&u)
                || (0x2E80..=0xA4CF).contains(&u)
                || (0xAC00..=0xD7A3).contains(&u)
                || (0xF900..=0xFAFF).contains(&u)
                || (0xFE30..=0xFE6F).contains(&u)
                || (0xFF00..=0xFF60).contains(&u)
                || (0xFFE0..=0xFFE6).contains(&u)
                || (0x20000..=0x3FFFD).contains(&u)
            {
                2
            } else {
                1
            }
        })
        .sum()
}

struct ChatLine {
    text: String,
    style: Style,
}

struct Permission {
    request: u64,
    name: String,
    detail: String,
    reason: String,
}

pub struct App {
    lines: Vec<ChatLine>,
    /// assistant text arriving right now; committed as chat lines by the `message` frame
    stream: String,
    thinking: String,
    pub input: Vec<char>,
    pub cursor: usize,
    /// 0 = pinned to the newest line (follow mode); increases when scrolling back
    scroll_back: usize,
    busy: bool,
    prompt_id: u64,
    permission: Option<Permission>,
    model: String,
    mode: String,
    session: String,
    cost: f64,
    pub quit: bool,
    pub err: Option<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            lines: Vec::new(),
            stream: String::new(),
            thinking: String::new(),
            input: Vec::new(),
            cursor: 0,
            scroll_back: 0,
            busy: false,
            prompt_id: 0,
            permission: None,
            model: "…".into(),
            mode: String::new(),
            session: String::new(),
            cost: 0.0,
            quit: false,
            err: None,
        }
    }

    fn push(&mut self, text: impl Into<String>, style: Style) {
        for line in text.into().split('\n') {
            self.lines.push(ChatLine { text: line.to_string(), style });
        }
    }

    /// One-dimension summary of a tool call for its status line.
    fn call_summary(input: &Value) -> String {
        let m = input.as_object();
        let detail = ["command", "path", "url", "query", "pattern"]
            .iter()
            .find_map(|k| m.and_then(|o| o.get(*k)).and_then(|v| v.as_str()))
            .map(String::from)
            .unwrap_or_else(|| input.to_string());
        let d: String = detail.chars().take(90).collect();
        d.replace('\n', " ")
    }

    fn commit_stream(&mut self) {
        if !self.thinking.is_empty() {
            let t: String = self.thinking.chars().take(400).collect();
            self.push(format!("✻ {t}"), Style::new().dark_gray().italic());
            self.thinking.clear();
        }
        if !self.stream.is_empty() {
            let s = std::mem::take(&mut self.stream);
            self.push(s.trim_end(), Style::new());
        }
    }

    /// Handle one event; returns true when the loop should end.
    pub fn handle(&mut self, ev: Event, serve: &mut Serve) -> bool {
        match ev {
            Event::Gone => {
                if !self.quit {
                    self.err = Some("tango serve exited".into());
                }
                return true;
            }
            Event::Stderr(l) => self.push(format!("! {l}"), Style::new().dark_gray()),
            Event::Resized(_, _) => {}
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => self.scroll_back += 3,
                MouseEventKind::ScrollDown => self.scroll_back = self.scroll_back.saturating_sub(3),
                _ => {}
            },
            Event::Key(k) if k.kind == KeyEventKind::Release => {}
            Event::Key(k) => self.key(k, serve),
            Event::Server(v) => self.frame(v),
        }
        false
    }

    fn key(&mut self, k: crossterm::event::KeyEvent, serve: &mut Serve) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // permission modal grabs the keyboard
        if self.permission.is_some() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.answer(serve, true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.answer(serve, false),
                _ => {}
            }
            return;
        }
        // Ctrl+C never quits (that is /exit's job): busy turns get interrupted,
        // idle input gets cleared — mashing it is always safe.
        if ctrl && k.code == KeyCode::Char('c') {
            if self.busy {
                serve.notify(json!({ "cmd": "interrupt" }));
                self.push("[interrupt requested]", Style::new().dark_gray());
            } else if !self.input.is_empty() {
                self.input.clear();
                self.cursor = 0;
            }
            return;
        }
        match k.code {
            KeyCode::Esc if self.busy => {
                serve.notify(json!({ "cmd": "interrupt" }));
            }
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Enter if !self.busy => self.send_prompt(serve),
            KeyCode::Enter => {
                self.push("[queued — ctrl+c interrupts]", Style::new().dark_gray());
            }
            KeyCode::Char('\n') | KeyCode::Char('\r') if k.modifiers.contains(KeyModifiers::ALT) => {
                self.input.insert(self.cursor, '\n');
                self.cursor += 1;
            }
            KeyCode::Char(c) if !ctrl => {
                self.input.insert(self.cursor, c);
                self.cursor += 1;
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.input.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = min(self.cursor + 1, self.input.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Up => self.scroll_back += 1,
            KeyCode::Down => self.scroll_back = self.scroll_back.saturating_sub(1),
            KeyCode::PageUp => self.scroll_back += 20,
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(20),
            _ => {}
        }
    }

    fn answer(&mut self, serve: &mut Serve, allow: bool) {
        if let Some(p) = self.permission.take() {
            serve.notify(json!({ "cmd": "permission", "request": p.request, "allow": allow }));
            self.push(
                format!("{} {} — {}", if allow { "✓ allowed" } else { "✗ denied" }, p.name, p.reason),
                if allow { Style::new().dark_gray() } else { Style::new().red() },
            );
        }
    }

    fn send_prompt(&mut self, serve: &mut Serve) {
        let text: String = self.input.iter().collect();
        if text.trim().is_empty() {
            return;
        }
        // The only way out: /exit or /quit. No key chord can kill the session by accident.
        let cmd = text.trim().to_lowercase();
        if cmd == "/exit" || cmd == "/quit" || cmd == ":q" {
            self.quit = true;
            return;
        }
        self.push(format!("❯ {}", text.replace('\n', " ⏎ ")), Style::new().cyan());
        self.input.clear();
        self.cursor = 0;
        self.scroll_back = 0;
        self.busy = true;
        self.prompt_id = serve.command("prompt", vec![("text", Value::String(text))]);
    }

    fn frame(&mut self, v: Value) {
        let t = v.get("type").and_then(Value::as_str).unwrap_or("");
        match t {
            "hello" => {
                self.model = v["model"].as_str().unwrap_or("?").into();
                self.mode = v["mode"].as_str().unwrap_or("?").into();
                self.session = v["sessionId"].as_str().unwrap_or("?").chars().take(8).collect();
                let n = v["tools"].as_array().map_or(0, |a| a.len());
                self.push(
                    format!("tango serve · session {} · {n} tools (/exit quit · esc/ctrl+c interrupt · wheel scrolls)", self.session),
                    Style::new().dark_gray(),
                );
            }
            "stream" => {
                let ev = &v["event"];
                let d = ev["delta"].as_str().unwrap_or("");
                match ev["type"].as_str() {
                    Some("text") => self.stream.push_str(d),
                    Some("thinking") => self.thinking.push_str(d),
                    Some("tool_call_start") => self.push(
                        format!("⚙ {} …", ev["name"].as_str().unwrap_or("?")),
                        Style::new().dark_gray(),
                    ),
                    _ => {}
                }
            }
            "message" => self.commit_stream(),
            "tool_result" => {
                let name = v["call"]["name"].as_str().unwrap_or("?");
                let summary = Self::call_summary(&v["call"]["input"]);
                let ms = v["ms"].as_u64().unwrap_or(0);
                let ok = !v["isError"].as_bool().unwrap_or(false);
                let style = if ok { Style::new().dark_gray() } else { Style::new().red() };
                self.push(format!("⚙ {name} {summary} · {ms}ms{}", if ok { "" } else { " · error" }), style);
                if !ok {
                    let peek: String = v["output"].as_str().unwrap_or("").chars().take(200).collect();
                    self.push(peek.replace('\n', " ⏎ "), Style::new().red().dark_gray());
                }
            }
            "permission_request" => {
                let name = v["call"]["name"].as_str().unwrap_or("?").to_string();
                let detail = Self::call_summary(&v["call"]["input"]);
                let reason = v["reason"].as_str().unwrap_or("").to_string();
                self.push(format!("? {name}: {reason}"), Style::new().yellow());
                self.permission = Some(Permission {
                    request: v["request"].as_u64().unwrap_or(0),
                    name,
                    detail,
                    reason,
                });
            }
            "turn_end" => {
                if let Some(c) = v["totalCostUsd"].as_f64() {
                    self.cost = c;
                }
            }
            "result" => {
                if v["id"].as_u64() == Some(self.prompt_id) {
                    self.busy = false;
                    if v["ok"].as_bool() == Some(false) {
                        self.push(format!("error: {}", v["error"].as_str().unwrap_or("?")), Style::new().red());
                    }
                }
            }
            "error" => self.push(format!("error: {}", v["message"].as_str().unwrap_or("?")), Style::new().red()),
            _ => {}
        }
    }

    // ---------- rendering ----------

    fn input_height(&self, width: u16) -> u16 {
        let text: String = self.input.iter().collect();
        let w = width.max(1) as usize;
        let h: usize = text.split('\n').map(|l| display_width(l).div_ceil(w).max(1)).sum::<usize>().max(1);
        (h.min(8)) as u16 + 1 // +1 for the rule above the input
    }

    fn chat_height(&self, area: Rect, input_h: u16) -> usize {
        (area.height.saturating_sub(1 + input_h)) as usize
    }

    /// Visual (post-wrap) line count of the chat at the given inner width.
    fn chat_visual_lines(&self, width: usize) -> usize {
        let w = width.max(1);
        self.lines.iter().map(|l| display_width(&l.text).div_ceil(w).max(1)).sum()
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let input_h = self.input_height(area.width);
        let chat_h = self.chat_height(area, input_h);
        let [status_a, chat_a, input_a] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(input_h),
        ])
        .areas(area);

        // status bar
        let work = if self.busy { "● working" } else { "idle" };
        let status = Line::from(vec![
            Span::styled(format!(" {} ", self.model), Style::new().black().on_white()),
            Span::styled(format!(" {} ", self.mode), Style::new().dark_gray()),
            Span::styled(format!(" ${:.4} ", self.cost), Style::new().yellow()),
            Span::styled(format!(" {work} "), if self.busy { Style::new().green() } else { Style::new().dark_gray() }),
            Span::styled(format!(" sess {} ", self.session), Style::new().dark_gray()),
        ]);
        f.render_widget(Paragraph::new(status), status_a);

        // chat with bottom-anchored scrolling (scroll_back=0 follows the newest line)
        let text: Vec<Line> = self
            .lines
            .iter()
            .map(|l| Line::from(Span::styled(l.text.clone(), l.style)))
            .collect();
        let inner_w = chat_a.width as usize;
        let total = self.chat_visual_lines(inner_w);
        let offset = total.saturating_sub(chat_h.saturating_sub(self.scroll_back).max(1));
        // live assistant text (not committed yet) hangs off the last line
        let mut shown = text;
        if !self.stream.is_empty() {
            shown.push(Line::from(Span::raw(self.stream.clone())));
        }
        f.render_widget(
            Paragraph::new(Text::from(shown)).wrap(Wrap { trim: false }).scroll((offset as u16, 0)),
            chat_a,
        );

        // input
        f.render_widget(
            Paragraph::new("─".repeat(area.width as usize)).style(Style::new().dark_gray()),
            Rect { x: 0, y: input_a.y, width: area.width, height: 1 },
        );
        let input_text: String = self.input.iter().collect();
        f.render_widget(
            Paragraph::new(input_text.clone()).wrap(Wrap { trim: false }),
            Rect { x: 0, y: input_a.y + 1, width: area.width, height: input_a.height.saturating_sub(1) },
        );
        // cursor: walk to the cursor char's wrapped position
        let w = area.width.max(1) as usize;
        let upto: String = self.input[..self.cursor].iter().collect();
        let cur_line = upto.rsplit('\n').next().unwrap_or("").to_string();
        // widths of wrapped visual rows before the cursor's logical line
        let logical = input_text.split('\n').collect::<Vec<_>>();
        let cursor_logical = upto.matches('\n').count();
        let mut rows = 0usize;
        for l in &logical[..cursor_logical.min(logical.len())] {
            rows += display_width(l).div_ceil(w).max(1);
        }
        let cw = display_width(&cur_line);
        let row = rows + cw / w;
        let col = cw % w;
        f.set_cursor_position(Position::new(input_a.x + col as u16, input_a.y + 1 + row as u16));

        // permission modal
        if let Some(p) = &self.permission {
            let w = (area.width.min(72)).max(40);
            let h = 7;
            let popup = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            f.render_widget(Clear, popup);
            let body = Paragraph::new(vec![
                Line::from(Span::styled(p.reason.clone(), Style::new().yellow())),
                Line::from(""),
                Line::from(Span::raw(p.detail.chars().take(240).collect::<String>())),
                Line::from(""),
                Line::from(Span::styled("y allow · n deny", Style::new().bold())),
            ])
            .wrap(Wrap { trim: true })
            .block(Block::bordered().title(format!(" {} ", p.name)));
            f.render_widget(body, popup);
        }
    }
}
