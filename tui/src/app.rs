use crate::child::{Event, Serve};
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use serde_json::{Value, json};
use std::cmp::min;
use std::fs;
use std::path::Path;

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
    /// full tool output; revealed under the status line when verbose mode is on (ctrl+o)
    output: Option<String>,
}

struct Permission {
    request: u64,
    name: String,
    detail: String,
    reason: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Pending {
    Model,
    Mode,
    Compact,
    Status,
}

#[derive(Clone, Copy, PartialEq)]
enum PopKind {
    Slash,
    File,
}

struct Popup {
    items: Vec<String>,
    descs: Vec<String>,
    sel: usize,
    kind: PopKind,
}

const SLASH: &[(&str, &str)] = &[
    ("/compact", "summarize older context"),
    ("/exit", "quit the session"),
    ("/help", "list commands"),
    ("/interrupt", "stop the running turn"),
    ("/model <id>", "switch model"),
    ("/mode <m>", "ask | auto | readonly"),
    ("/quit", "quit the session"),
    ("/status", "session info"),
];

/// inline @path tokens are expanded into file contents at send time (Claude Code style)
const AT_FILE_CAP: usize = 6000;

pub struct App {
    lines: Vec<ChatLine>,
    /// assistant text arriving right now; committed as chat lines by the `message` frame
    stream: String,
    thinking: String,
    pub input: Vec<char>,
    pub cursor: usize,
    popup: Option<Popup>,
    /// 0 = pinned to the newest line (follow mode); increases when scrolling back
    scroll_back: usize,
    verbose_tools: bool,
    busy: bool,
    prompt_id: u64,
    pending: Vec<(u64, Pending)>,
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
            popup: None,
            scroll_back: 0,
            verbose_tools: false,
            busy: false,
            prompt_id: 0,
            pending: Vec::new(),
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
            self.lines.push(ChatLine { text: line.to_string(), style, output: None });
        }
    }

    fn push_tool(&mut self, text: impl Into<String>, style: Style, output: Option<String>) {
        self.lines.push(ChatLine { text: text.into(), style, output });
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

    /// Lightweight markdown commit: fences become indented gray blocks (markers stripped),
    /// headings turn bold, ** and ` markers vanish. No inline spans — one style per line.
    fn commit_stream(&mut self) {
        if !self.thinking.is_empty() {
            let t: String = self.thinking.chars().take(400).collect();
            self.push(format!("✻ {t}"), Style::new().dark_gray().italic());
            self.thinking.clear();
        }
        if self.stream.is_empty() {
            return;
        }
        let s = std::mem::take(&mut self.stream);
        let mut in_fence = false;
        for raw in s.trim_end().split('\n') {
            let line = raw.trim_end();
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                self.push(format!("  {line}"), Style::new().dark_gray());
                continue;
            }
            if line.starts_with('#') {
                self.push(line.trim_start_matches('#').trim(), Style::new().bold());
                continue;
            }
            let t = line.replace("**", "").replace('`', "");
            self.push(t, Style::new());
        }
    }

    // ---------- input popups (slash commands + @file) ----------

    /// whitespace-delimited token containing the cursor
    fn token_at_cursor(&self) -> (String, usize, usize) {
        let s: String = self.input.iter().collect();
        let start = s[..self.cursor_byte()].rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
        let rel_end = s[start..].find(char::is_whitespace).map(|i| start + i).unwrap_or(s.len());
        (s[start..rel_end].to_string(), start, rel_end)
    }

    /// char index → byte index (tokens cut on byte boundaries need real byte offsets)
    fn cursor_byte(&self) -> usize {
        self.input[..self.cursor].iter().map(|c| c.len_utf8()).sum()
    }

    fn refresh_popup(&mut self) {
        let text: String = self.input.iter().collect();
        // slash palette: input is the command word only, idle
        if !self.busy && text.starts_with('/') && !text.contains(' ') {
            let items: Vec<(&str, &str)> =
                SLASH.iter().copied().filter(|(c, _)| c.starts_with(text.as_str())).collect();
            if items.is_empty() || (items.len() == 1 && items[0].0 == text) {
                self.popup = None;
            } else {
                self.popup = Some(Popup {
                    items: items.iter().map(|(c, _)| c.to_string()).collect(),
                    descs: items.iter().map(|(_, d)| d.to_string()).collect(),
                    sel: 0,
                    kind: PopKind::Slash,
                });
            }
            return;
        }
        // @file palette: cursor sits inside an @token
        let (tok, _, _) = self.token_at_cursor();
        if tok.starts_with('@') && tok.len() > 1 {
            let items = Self::file_candidates(&tok);
            if items.is_empty() {
                self.popup = None;
            } else {
                self.popup =
                    Some(Popup { items: items.clone(), descs: vec![String::new(); items.len()], sel: 0, kind: PopKind::File });
            }
            return;
        }
        self.popup = None;
    }

    fn file_candidates(token: &str) -> Vec<String> {
        let body = &token[1..];
        let (dir, frag) = match body.rfind('/') {
            Some(i) => (&body[..=i], &body[i + 1..]),
            None => ("", body),
        };
        let base = if dir.is_empty() { Path::new(".").to_path_buf() } else { Path::new(dir).to_path_buf() };
        let mut out: Vec<String> = Vec::new();
        if let Ok(rd) = fs::read_dir(base) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !frag.is_empty() && !name.starts_with(frag) {
                    continue;
                }
                let is_dir = e.path().is_dir();
                out.push(format!("@{dir}{name}{}", if is_dir { "/" } else { "" }));
                if out.len() >= 12 {
                    break;
                }
            }
        }
        out.sort();
        out
    }

    /// replace the current token (or the whole input for slash) with the selected item
    fn accept_popup(&mut self) {
        let Some(p) = &self.popup else { return };
        let Some(item) = p.items.get(p.sel).cloned() else { return };
        match p.kind {
            PopKind::Slash => {
                // never insert the "<id>"-style placeholder; commands with args get a trailing space
                let word = item.split(' ').next().unwrap_or(&item);
                let has_arg = item.contains(' ');
                self.input = word.chars().collect();
                if has_arg {
                    self.input.push(' ');
                }
                self.cursor = self.input.len();
            }
            PopKind::File => {
                let (tok, start, end) = self.token_at_cursor();
                let _ = tok;
                let s: String = self.input.iter().collect();
                let cb = self.cursor_byte();
                let sb: usize = s[..min(start, cb)].chars().count();
                let eb: usize = s[..min(end, s.len())].chars().count();
                let mut next: Vec<char> = self.input[..sb].to_vec();
                next.extend(item.chars());
                next.extend(self.input[eb..].iter());
                let adv = item.chars().count();
                self.input = next;
                self.cursor = sb + adv;
            }
        }
        self.popup = None;
    }

    // ---------- slash commands ----------

    fn run_slash(&mut self, text: &str, serve: &mut Serve) {
        let trimmed = text.trim();
        let (head, rest) = match trimmed.split_once(' ') {
            Some((h, r)) => (h.to_lowercase(), r.trim()),
            None => (trimmed.to_lowercase(), ""),
        };
        match head.as_str() {
            "/exit" | "/quit" | ":q" => self.quit = true,
            "/help" => {
                self.push("commands:", Style::new().bold());
                for (c, d) in SLASH {
                    self.push(format!("  {c:<14} {d}"), Style::new().dark_gray());
                }
                self.push("  @path         inline a file into the next prompt", Style::new().dark_gray());
                self.push("  tab / ↑↓      complete · ctrl+o tool output · esc interrupt/clear", Style::new().dark_gray());
            }
            "/clear" | "/new" => self.lines.clear(),
            "/interrupt" => {
                serve.notify(json!({ "cmd": "interrupt" }));
            }
            "/model" => {
                if rest.is_empty() {
                    self.push("usage: /model <id> (see status bar)", Style::new().dark_gray());
                } else {
                    let id = serve.command("set_model", vec![("modelId", Value::String(rest.into()))]);
                    self.pending.push((id, Pending::Model));
                }
            }
            "/mode" => {
                if rest.is_empty() {
                    self.push("usage: /mode ask|auto|readonly", Style::new().dark_gray());
                } else {
                    let id = serve.command("set_mode", vec![("mode", Value::String(rest.into()))]);
                    self.pending.push((id, Pending::Mode));
                }
            }
            "/compact" => {
                let id = serve.command("compact", vec![]);
                self.pending.push((id, Pending::Compact));
            }
            "/status" => {
                let id = serve.command("status", vec![]);
                self.pending.push((id, Pending::Status));
            }
            other => {
                self.push(format!("unknown command {other} · /help"), Style::new().red());
            }
        }
    }

    // ---------- @file expansion ----------

    fn expand_at(text: &str) -> (String, Vec<String>) {
        let chars: Vec<char> = text.chars().collect();
        let mut hits: Vec<(usize, usize, String)> = Vec::new(); // [start,end) char range, path
        let mut i = 0;
        while i < chars.len() {
            let boundary = i == 0 || chars[i - 1].is_whitespace();
            if boundary && chars[i] == '@' {
                let mut j = i + 1;
                while j < chars.len() && !chars[j].is_whitespace() {
                    j += 1;
                }
                let raw: String = chars[i + 1..j].iter().collect();
                let path = raw.trim_end_matches(|c: char| ")]}.,;:!?\"'".contains(c));
                if !path.is_empty() && Path::new(path).is_file() {
                    hits.push((i, j, path.to_string()));
                }
                i = j.max(i + 1);
                continue;
            }
            i += 1;
        }
        let mut warns = Vec::new();
        let mut out = String::new();
        let mut pos = 0;
        for (start, end, path) in hits {
            out.extend(chars[pos..start].iter());
            pos = end;
            match fs::read_to_string(&path) {
                Ok(body) => {
                    let clipped: String = body.chars().take(AT_FILE_CAP).collect();
                    let tail = if body.chars().count() > AT_FILE_CAP { "\n…[truncated]" } else { "" };
                    out.push_str(&format!("{path}:\n{clipped}{tail}"));
                }
                Err(e) => warns.push(format!("@{path}: {e}")),
            }
        }
        out.extend(chars[pos..].iter());
        (out, warns)
    }

    // ---------- event handling ----------

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
                self.popup = None;
            }
            return;
        }
        if ctrl && k.code == KeyCode::Char('o') {
            self.verbose_tools = !self.verbose_tools;
            return;
        }
        // popup grabs navigation keys while it is open
        if let Some(p) = &mut self.popup {
            match k.code {
                KeyCode::Up if p.sel > 0 => {
                    p.sel -= 1;
                    return;
                }
                KeyCode::Down => {
                    p.sel = min(p.sel + 1, p.items.len().saturating_sub(1));
                    return;
                }
                KeyCode::Tab | KeyCode::Right => {
                    self.accept_popup();
                    return;
                }
                KeyCode::Enter if p.kind == PopKind::File => {
                    self.accept_popup();
                    return;
                }
                KeyCode::Esc => {
                    self.popup = None;
                    return;
                }
                _ => {}
            }
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
            KeyCode::Enter if self.input.iter().collect::<String>().trim_start().starts_with('/') => {
                let text: String = std::mem::take(&mut self.input).iter().collect();
                self.cursor = 0;
                self.run_slash(&text, serve); // commands stay live during a turn
            }
            KeyCode::Enter => {
                self.push("[queued — ctrl+c interrupts]", Style::new().dark_gray());
            }
            KeyCode::Char('\n') | KeyCode::Char('\r') if k.modifiers.contains(KeyModifiers::ALT) => {
                self.input.insert(self.cursor, '\n');
                self.cursor += 1;
                self.refresh_popup();
            }
            KeyCode::Char(c) if !ctrl => {
                self.input.insert(self.cursor, c);
                self.cursor += 1;
                self.refresh_popup();
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.input.remove(self.cursor);
                    self.refresh_popup();
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    self.input.remove(self.cursor);
                    self.refresh_popup();
                }
            }
            KeyCode::Left => self.cursor = cursor_step(&self.input, self.cursor, true),
            KeyCode::Right => self.cursor = cursor_step(&self.input, self.cursor, false),
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
        self.input.clear();
        self.cursor = 0;
        self.popup = None;
        if text.trim().starts_with('/') || text.trim() == ":q" {
            self.run_slash(&text, serve);
            return;
        }
        let (expanded, warns) = Self::expand_at(&text);
        for w in warns {
            self.push(format!("! {w}"), Style::new().dark_gray());
        }
        self.push(format!("❯ {}", text.replace('\n', " ⏎ ")), Style::new().cyan());
        self.scroll_back = 0;
        self.busy = true;
        self.prompt_id = serve.command("prompt", vec![("text", Value::String(expanded))]);
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
                    format!("tango serve · session {} · {n} tools · /help · esc/ctrl+c interrupt · /exit quits", self.session),
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
                let full = v["output"].as_str().map(|s| {
                    let mut out = String::new();
                    for line in s.lines().take(200) {
                        out.push_str(line);
                        out.push('\n');
                    }
                    out.truncate(out.trim_end().len());
                    out
                });
                let note = if ok { "" } else { " · error (ctrl+o)" };
                self.push_tool(format!("⚙ {name} {summary} · {ms}ms{note}"), style, full);
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
                let id = v["id"].as_u64().unwrap_or(0);
                let ok = v["ok"].as_bool().unwrap_or(false);
                if id == self.prompt_id {
                    self.busy = false;
                    self.refresh_popup();
                    if !ok {
                        self.push(format!("error: {}", v["error"].as_str().unwrap_or("?")), Style::new().red());
                    }
                    return;
                }
                if let Some(pos) = self.pending.iter().position(|(pid, _)| *pid == id) {
                    let kind = self.pending.remove(pos).1;
                    let d = &v["data"];
                    match (kind, ok) {
                        (Pending::Model, true) => {
                            self.model = d["model"].as_str().unwrap_or("?").into();
                            self.push(format!("model → {}", self.model), Style::new().dark_gray());
                        }
                        (Pending::Mode, true) => {
                            self.mode = d["mode"].as_str().unwrap_or("?").into();
                            self.push(format!("mode → {}", self.mode), Style::new().dark_gray());
                        }
                        (Pending::Compact, true) => self.push(
                            format!(
                                "compact: {} · context {} messages",
                                d["compacted"].as_bool().unwrap_or(false),
                                d["contextMessages"].as_u64().unwrap_or(0)
                            ),
                            Style::new().dark_gray(),
                        ),
                        (Pending::Status, true) => {
                            self.push(
                                format!(
                                    "session {} · {} · {} · {} msgs · ${:.4} · {}",
                                    d["sessionId"].as_str().unwrap_or("?"),
                                    d["model"].as_str().unwrap_or("?"),
                                    d["mode"].as_str().unwrap_or("?"),
                                    d["contextMessages"].as_u64().unwrap_or(0),
                                    d["totalCostUsd"].as_f64().unwrap_or(0.0),
                                    d["cwd"].as_str().unwrap_or("?"),
                                ),
                                Style::new().dark_gray(),
                            );
                        }
                        (_, false) => self.push(format!("error: {}", v["error"].as_str().unwrap_or("?")), Style::new().red()),
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

    /// the chat exactly as drawn, tool outputs expanded or not — scroll math uses this
    fn chat_lines(&self) -> Vec<ChatLine> {
        let mut out = Vec::with_capacity(self.lines.len());
        for l in &self.lines {
            out.push(ChatLine { text: l.text.clone(), style: l.style, output: None });
            if self.verbose_tools
                && let Some(o) = &l.output
            {
                for cl in o.lines().take(200) {
                    out.push(ChatLine { text: format!("  │ {cl}"), style: Style::new().dark_gray(), output: None });
                }
            }
        }
        out
    }

    fn visual_lines(lines: &[ChatLine], width: usize) -> usize {
        let w = width.max(1);
        lines.iter().map(|l| display_width(&l.text).div_ceil(w).max(1)).sum()
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
        let mut spans = vec![
            Span::styled(format!(" {} ", self.model), Style::new().black().on_white()),
            Span::styled(format!(" {} ", self.mode), Style::new().dark_gray()),
            Span::styled(format!(" ${:.4} ", self.cost), Style::new().yellow()),
            Span::styled(format!(" {work} "), if self.busy { Style::new().green() } else { Style::new().dark_gray() }),
            Span::styled(format!(" sess {} ", self.session), Style::new().dark_gray()),
        ];
        if self.verbose_tools {
            spans.push(Span::styled(" tools▾ ", Style::new().black().on_yellow()));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), status_a);

        // chat with bottom-anchored scrolling (scroll_back=0 follows the newest line)
        let shown: Vec<Line> = self
            .chat_lines()
            .into_iter()
            .map(|l| Line::from(Span::styled(l.text, l.style)))
            .collect();
        let inner_w = chat_a.width as usize;
        let total = Self::visual_lines(&self.chat_lines(), inner_w)
            + if self.stream.is_empty() { 0 } else { display_width(&self.stream).div_ceil(inner_w.max(1)) };
        let offset = total.saturating_sub(chat_h.saturating_sub(self.scroll_back).max(1));
        // live assistant text (not committed yet) hangs off the last line, markdown stripped
        let mut shown = shown;
        if !self.stream.is_empty() {
            let preview = self.stream.replace("**", "").replace('`', "");
            shown.push(Line::from(Span::raw(preview)));
        }
        f.render_widget(
            Paragraph::new(Text::from(shown)).wrap(Wrap { trim: false }).scroll((offset as u16, 0)),
            chat_a,
        );

        // completion popup, glued above the input rule
        if let Some(p) = &self.popup {
            let ph = (p.items.len().min(6) + 1) as u16;
            let pop = Rect {
                x: 0,
                y: input_a.y.saturating_sub(ph),
                width: area.width,
                height: ph,
            };
            f.render_widget(Clear, pop);
            let rows: Vec<Line> = p
                .items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let style = if i == p.sel { Style::new().black().on_white() } else { Style::new().dark_gray() };
                    let desc = p.descs.get(i).cloned().unwrap_or_default();
                    Line::from(Span::styled(format!(" {item:<24}{desc}"), style))
                })
                .collect();
            f.render_widget(Paragraph::new(rows), pop);
        }

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

/// Left/Right step over a display cluster boundary (CJK cells count double, but per-char
/// stepping is fine — this mainly skips to the other edge of a CJK pair cleanly).
fn cursor_step(input: &[char], cursor: usize, left: bool) -> usize {
    if left {
        cursor.saturating_sub(1)
    } else {
        min(cursor + 1, input.len())
    }
}
