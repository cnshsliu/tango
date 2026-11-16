use crate::child::{Event, Serve};
use crate::keys::{Action, Keymap};
use crate::theme::{Theme, Tone};
use crate::viewer::Viewer;
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
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
    tone: Tone,
    /// full tool output or collapsed user prompt; shown in the independent viewer
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
    Models,
    Sessions,
    Mode,
    Compact,
    Status,
    Skill,
    History,
}

#[derive(Clone, Copy, PartialEq)]
enum PickerKind {
    Model,
    Session,
}

#[derive(Clone)]
struct PickItem {
    id: String,
    /// left column: model id or "prefix · msgs · date"
    label: String,
    /// right column: context window, or session name/cwd
    note: String,
    warn: bool,
    current: bool,
}

struct Picker {
    kind: PickerKind,
    items: Vec<PickItem>,
    sel: usize,
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
    ("/clear", "清空屏幕（上下文保留）"),
    ("/compact", "压缩较早的上下文"),
    ("/cost", "本会话花费与上下文条数"),
    ("/exit", "退出"),
    ("/help", "命令列表"),
    ("/interrupt", "停止当前回合"),
    ("/model <id>", "切换模型（存为默认），裸 /model 打开列表"),
    ("/mode <m>", "ask | auto | readonly"),
    ("/new", "开新会话"),
    ("/quit", "退出"),
    ("/resume", "切换到历史会话，打开列表"),
    (
        "/skill [title]",
        "把本会话存成技能草稿 · /skill finish <file> 定稿",
    ),
    ("/status", "会话信息"),
];

/// inline @path tokens are expanded into file contents at send time (Claude Code style)
const AT_FILE_CAP: usize = 6000;

#[derive(Clone, Copy, Debug, PartialEq)]
enum EditMode {
    Insert,
    Normal,
}

pub struct App {
    theme: Theme,
    keys: Keymap,
    vim: bool,
    edit_mode: EditMode,
    lines: Vec<ChatLine>,
    /// assistant text arriving right now; committed as chat lines by the `message` frame
    stream: String,
    thinking: String,
    pub input: Vec<char>,
    pub cursor: usize,
    popup: Option<Popup>,
    picker: Option<Picker>,
    viewer: Option<Viewer>,
    chat_area: Rect,
    /// 0 = pinned to the newest line (follow mode); increases when scrolling back
    scroll_back: usize,
    busy: bool,
    prompt_id: u64,
    pending: Vec<(u64, Pending)>,
    permission: Option<Permission>,
    model: String,
    mode: String,
    session: String,
    cost: f64,
    pub quit: bool,
    /// serve respawn request for the main loop: Some(Some(id)) resumes, Some(None) starts fresh
    pub respawn: Option<Option<String>>,
    pub err: Option<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            theme: Theme::dark(),
            keys: Keymap::from_json(None),
            vim: false,
            edit_mode: EditMode::Insert,
            lines: Vec::new(),
            stream: String::new(),
            thinking: String::new(),
            input: Vec::new(),
            cursor: 0,
            popup: None,
            picker: None,
            viewer: None,
            chat_area: Rect::default(),
            scroll_back: 0,
            busy: false,
            prompt_id: 0,
            pending: Vec::new(),
            permission: None,
            model: "…".into(),
            mode: String::new(),
            session: String::new(),
            cost: 0.0,
            quit: false,
            respawn: None,
            err: None,
        }
    }

    pub fn mode(&self) -> &str {
        &self.mode
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// switching context means a new serve child; wipe the old transcript and turn state
    fn switch_session(&mut self, target: Option<String>) {
        let keep: Vec<ChatLine> = self.lines.drain(..).last().into_iter().collect();
        self.lines = keep; // the "resuming…/new session" notice just pushed
        self.stream.clear();
        self.thinking.clear();
        self.pending.clear();
        self.permission = None;
        self.viewer = None;
        self.busy = false;
        self.cost = 0.0;
        self.scroll_back = 0;
        self.respawn = Some(target);
    }

    fn push(&mut self, text: impl Into<String>, tone: Tone) {
        for line in text.into().split('\n') {
            self.lines.push(ChatLine {
                text: line.to_string(),
                tone,
                output: None,
            });
        }
    }

    fn push_tool(&mut self, text: impl Into<String>, tone: Tone, output: Option<String>) {
        self.lines.push(ChatLine {
            text: text.into(),
            tone,
            output,
        });
    }

    fn push_user(&mut self, text: &str) {
        let chars = text.chars().count();
        let line_count = text.lines().count();
        if chars > 600 || line_count > 8 {
            let preview = text.lines().take(2).collect::<Vec<_>>().join(" ⏎ ");
            let preview = preview.chars().take(160).collect::<String>();
            self.push_tool(
                format!("❯ [粘贴 {chars} 字 · {line_count} 行 · ctrl+o/点击展开]\n  {preview} …"),
                Tone::User,
                Some(text.to_string()),
            );
        } else {
            self.push(format!("❯ {}", text.replace('\n', " ⏎ ")), Tone::User);
        }
    }

    fn open_latest_viewer(&mut self) {
        let found = self.lines.iter().rev().find_map(|line| {
            line.output.as_ref().map(|output| {
                let title = line
                    .text
                    .lines()
                    .next()
                    .unwrap_or("content")
                    .chars()
                    .take(64)
                    .collect::<String>();
                (title, output.clone())
            })
        });
        if let Some((title, text)) = found {
            self.viewer = Some(Viewer::new(title, text));
        }
    }

    fn open_line_viewer(&mut self, index: usize) {
        let found = self.lines.get(index).and_then(|line| {
            line.output.as_ref().map(|output| {
                let title = line
                    .text
                    .lines()
                    .next()
                    .unwrap_or("content")
                    .chars()
                    .take(64)
                    .collect::<String>();
                (title, output.clone())
            })
        });
        if let Some((title, text)) = found {
            self.viewer = Some(Viewer::new(title, text));
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

    /// Lightweight markdown commit: fences become indented gray blocks (markers stripped),
    /// headings turn bold, ** and ` markers vanish, `|` tables are re-flowed into aligned
    /// columns. No inline spans — one style per line.
    fn commit_stream(&mut self) {
        if !self.thinking.is_empty() {
            let t: String = self.thinking.chars().take(400).collect();
            self.push(format!("✻ {t}"), Tone::Thinking);
            self.thinking.clear();
        }
        if self.stream.is_empty() {
            return;
        }
        let s = std::mem::take(&mut self.stream);
        let mut in_fence = false;
        let lines: Vec<&str> = s.trim_end().split('\n').collect();
        let mut i = 0;
        while i < lines.len() {
            let line = lines[i].trim_end();
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
                i += 1;
                continue;
            }
            if in_fence {
                self.push(format!("  {line}"), Tone::Muted);
                i += 1;
                continue;
            }
            // table run: consecutive | rows (the |---| separator row folds into a rule)
            if line.trim_start().starts_with('|') {
                let mut run = vec![line.trim()];
                while i + 1 < lines.len() && lines[i + 1].trim_start().starts_with('|') {
                    i += 1;
                    run.push(lines[i].trim());
                }
                Self::push_table(self, &run);
                i += 1;
                continue;
            }
            if line.starts_with('#') {
                self.push(line.trim_start_matches('#').trim(), Tone::Selected);
                i += 1;
                continue;
            }
            let t = line.replace("**", "").replace('`', "");
            self.push(t, Tone::Text);
            i += 1;
        }
    }

    /// Re-flow a markdown table into fixed-width terminal columns (CJK-aware padding).
    fn push_table(&mut self, run: &[&str]) {
        let rows: Vec<Vec<String>> = run
            .iter()
            .filter(|l| {
                let core = l.trim_matches('|');
                // drop blanks and rows that are nothing but ---/:-- separators (pipes included)
                !core.is_empty()
                    && !(core.contains('-')
                        && core.chars().all(|c| matches!(c, '-' | ':' | '|' | ' ')))
            })
            .map(|l| {
                l.trim_matches('|')
                    .split('|')
                    .map(|c| {
                        c.trim()
                            .replace("**", "")
                            .replace('`', "")
                            .trim()
                            .to_string()
                    })
                    .collect()
            })
            .collect();
        if rows.len() < 2 {
            // not a real table — keep the raw pipes as plain lines
            for l in run {
                self.push(*l, Tone::Text);
            }
            return;
        }
        let cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut width = vec![0usize; cols];
        for r in &rows {
            for (c, cell) in r.iter().enumerate() {
                width[c] = width[c].max(display_width(cell));
            }
        }
        for (n, r) in rows.iter().enumerate() {
            let mut out = String::from(" ");
            for (c, cell) in r.iter().enumerate() {
                out.push_str(cell);
                out.push_str(&" ".repeat(width[c].saturating_sub(display_width(cell)) + 2));
            }
            let style = if n == 0 { Tone::Selected } else { Tone::Text };
            self.push(out.trim_end(), style);
            if n == 0 {
                self.push(
                    format!(" {}", "─".repeat(width.iter().sum::<usize>() + 2 * cols)),
                    Tone::Muted,
                );
            }
        }
    }

    // ---------- input popups (slash commands + @file) ----------

    /// whitespace-delimited token containing the cursor
    fn token_at_cursor(&self) -> (String, usize, usize) {
        let s: String = self.input.iter().collect();
        let start = s[..self.cursor_byte()]
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let rel_end = s[start..]
            .find(char::is_whitespace)
            .map(|i| start + i)
            .unwrap_or(s.len());
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
            let items: Vec<(&str, &str)> = SLASH
                .iter()
                .copied()
                .filter(|(c, _)| c.starts_with(text.as_str()))
                .collect();
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
                self.popup = Some(Popup {
                    items: items.clone(),
                    descs: vec![String::new(); items.len()],
                    sel: 0,
                    kind: PopKind::File,
                });
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
        let base = if dir.is_empty() {
            Path::new(".").to_path_buf()
        } else {
            Path::new(dir).to_path_buf()
        };
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
        let Some(item) = p.items.get(p.sel).cloned() else {
            return;
        };
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
                self.push("commands:", Tone::Selected);
                for (c, d) in SLASH {
                    self.push(format!("  {c:<14} {d}"), Tone::Muted);
                }
                self.push(
                    "  @path         inline a file or dir listing into the next prompt",
                    Tone::Muted,
                );
                self.push(
                    "  tab / ↑↓      complete · ctrl+o tools (default) · esc interrupt/clear/mode",
                    Tone::Muted,
                );
            }
            "/clear" => self.lines.clear(),
            "/new" => {
                self.push("↻ new session …", Tone::Muted);
                self.switch_session(None);
            }
            "/sessions" => self.run_slash("/resume", serve),
            "/cost" => {
                let id = serve.command("status", vec![]);
                self.pending.push((id, Pending::Status));
            }
            "/skill" | "/save-as-skill" => {
                let id = serve.command("save_skill", vec![("arg", Value::String(rest.into()))]);
                self.pending.push((id, Pending::Skill));
            }
            "/interrupt" => {
                serve.notify(json!({ "cmd": "interrupt" }));
            }
            "/model" => {
                if rest.is_empty() {
                    let id = serve.command("models", vec![]);
                    self.pending.push((id, Pending::Models));
                } else {
                    let id =
                        serve.command("set_model", vec![("modelId", Value::String(rest.into()))]);
                    self.pending.push((id, Pending::Model));
                }
            }
            "/mode" => {
                if rest.is_empty() {
                    self.push("usage: /mode ask|auto|readonly", Tone::Muted);
                } else {
                    let id = serve.command("set_mode", vec![("mode", Value::String(rest.into()))]);
                    self.pending.push((id, Pending::Mode));
                }
            }
            "/compact" => {
                let id = serve.command("compact", vec![]);
                self.pending.push((id, Pending::Compact));
            }
            "/resume" => {
                let id = serve.command("sessions", vec![]);
                self.pending.push((id, Pending::Sessions));
            }
            "/status" => {
                let id = serve.command("status", vec![]);
                self.pending.push((id, Pending::Status));
            }
            other => {
                self.push(format!("unknown command {other} · /help"), Tone::Error);
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
                let p = Path::new(path);
                if !path.is_empty() && (p.is_file() || p.is_dir()) {
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
            if Path::new(&path).is_dir() {
                out.push_str(&format!("{path}/:\n{}", Self::dir_listing(&path)));
                continue;
            }
            match fs::read_to_string(&path) {
                Ok(body) => {
                    let clipped: String = body.chars().take(AT_FILE_CAP).collect();
                    let tail = if body.chars().count() > AT_FILE_CAP {
                        "\n…[truncated]"
                    } else {
                        ""
                    };
                    out.push_str(&format!("{path}:\n{clipped}{tail}"));
                }
                Err(e) => warns.push(format!("@{path}: {e}")),
            }
        }
        out.extend(chars[pos..].iter());
        (out, warns)
    }

    /// one-level directory listing for @dir references; heavy machine dirs stay out of prompts
    fn dir_listing(path: &str) -> String {
        const SKIP: [&str; 4] = [".git", "node_modules", "target", "dist"];
        let mut entries: Vec<(String, bool)> = fs::read_dir(path)
            .map(|rd| {
                rd.flatten()
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        let is_dir = e.path().is_dir();
                        (name, is_dir)
                    })
                    .collect()
            })
            .unwrap_or_default();
        entries.sort();
        let mut out = String::new();
        let mut shown = 0;
        for (name, is_dir) in entries {
            if SKIP.contains(&name.as_str()) {
                continue;
            }
            let line = if is_dir {
                format!("{name}/\n")
            } else {
                format!("{name}\n")
            };
            out.push_str(&line);
            shown += 1;
            if shown >= 200 {
                out.push_str("…[truncated]\n");
                break;
            }
        }
        out
    }

    // ---------- event handling ----------

    /// Handle one event; returns true when the loop should end.
    pub fn handle(&mut self, ev: Event, serve: &mut Serve) -> bool {
        match ev {
            Event::Gone(_) => {
                if !self.quit {
                    self.err = Some("tango serve exited".into());
                }
                return true;
            }
            Event::Stderr(l) => self.push(format!("! {l}"), Tone::Muted),
            Event::Resized(_, _) => {}
            Event::Paste(s) => {
                if self.permission.is_none()
                    && self.picker.is_none()
                    && self.viewer.is_none()
                    && (!self.vim || self.edit_mode == EditMode::Insert)
                {
                    let s = s.replace("\r\n", "\n").replace('\r', "\n");
                    for c in s.chars() {
                        self.input.insert(self.cursor, c);
                        self.cursor += 1;
                    }
                    self.refresh_popup();
                }
            }
            Event::Mouse(m) => {
                if let Some(viewer) = self.viewer.as_mut() {
                    match m.kind {
                        MouseEventKind::ScrollUp => viewer.scroll_by(-3, viewer.max_scroll),
                        MouseEventKind::ScrollDown => viewer.scroll_by(3, viewer.max_scroll),
                        _ => {}
                    }
                } else {
                    match m.kind {
                        MouseEventKind::ScrollUp => self.scroll_back += 3,
                        MouseEventKind::ScrollDown => {
                            self.scroll_back = self.scroll_back.saturating_sub(3)
                        }
                        MouseEventKind::Down(MouseButton::Left)
                            if self.chat_area.contains(Position::new(m.column, m.row)) =>
                        {
                            let row = m.row.saturating_sub(self.chat_area.y);
                            if let Some(index) = self.line_at_visual_row(
                                self.chat_area.width as usize,
                                self.chat_area.height as usize,
                                row as usize,
                            ) {
                                self.open_line_viewer(index);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Event::Key(k) if k.kind == KeyEventKind::Release => {}
            Event::Key(k) => self.key(k, serve),
            Event::Server(_, v) => {
                // a (re)attached child: redraw the session's past turns before anything new
                if v["type"] == "hello" {
                    let id = serve.command("history", vec![]);
                    self.pending.push((id, Pending::History));
                }
                self.frame(v)
            }
        }
        false
    }

    fn normal_edit(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('h') => self.cursor = cursor_step(&self.input, self.cursor, true),
            KeyCode::Char('l') => self.cursor = cursor_step(&self.input, self.cursor, false),
            KeyCode::Char('0') => self.cursor = 0,
            KeyCode::Char('$') => self.cursor = self.input.len(),
            KeyCode::Char('x') if self.cursor < self.input.len() => {
                self.input.remove(self.cursor);
                self.refresh_popup();
            }
            KeyCode::Char('i') => self.edit_mode = EditMode::Insert,
            KeyCode::Char('a') => {
                self.cursor = min(self.cursor + 1, self.input.len());
                self.edit_mode = EditMode::Insert;
            }
            KeyCode::Char('I') => {
                self.cursor = 0;
                self.edit_mode = EditMode::Insert;
            }
            KeyCode::Char('A') => {
                self.cursor = self.input.len();
                self.edit_mode = EditMode::Insert;
            }
            KeyCode::Char('j') => self.scroll_back = self.scroll_back.saturating_sub(1),
            KeyCode::Char('k') => self.scroll_back += 1,
            KeyCode::Char('/') | KeyCode::Char('@') => {
                self.edit_mode = EditMode::Insert;
                return false;
            }
            KeyCode::Enter => return false,
            _ => {}
        }
        true
    }

    fn key(&mut self, k: crossterm::event::KeyEvent, serve: &mut Serve) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.viewer.is_some() {
            if k.code == KeyCode::Esc || self.keys.matches(Action::ExpandTools, k) {
                self.viewer = None;
                return;
            }
            let viewer = self.viewer.as_mut().unwrap();
            match k.code {
                KeyCode::Up | KeyCode::Char('k') => viewer.scroll_by(-1, viewer.max_scroll),
                KeyCode::Down | KeyCode::Char('j') => viewer.scroll_by(1, viewer.max_scroll),
                KeyCode::PageUp => viewer.scroll_by(-10, viewer.max_scroll),
                KeyCode::PageDown => viewer.scroll_by(10, viewer.max_scroll),
                KeyCode::Home => viewer.home(),
                KeyCode::End => viewer.end(viewer.max_scroll),
                _ => {}
            }
            return;
        }
        // permission modal grabs the keyboard
        if self.permission.is_some() {
            match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.answer(serve, true)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.answer(serve, false),
                // always: allow this one and stop asking for the rest of the session (REPL parity)
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.run_slash("/mode auto", serve);
                    self.answer(serve, true);
                }
                _ => {}
            }
            return;
        }
        if self.keys.matches(Action::ExpandTools, k) {
            self.open_latest_viewer();
            return;
        }
        // Ctrl+C always remains a safe interrupt even when the action is remapped.
        if self.busy && (self.keys.matches(Action::Interrupt, k) || k.code == KeyCode::Esc) {
            serve.notify(json!({ "cmd": "interrupt" }));
            self.push("[interrupt requested]", Tone::Muted);
            return;
        }
        if self.vim && self.edit_mode == EditMode::Insert && k.code == KeyCode::Esc {
            self.edit_mode = EditMode::Normal;
            self.popup = None;
            self.cursor = self.cursor.saturating_sub(1);
            return;
        }
        if self.keys.matches(Action::Interrupt, k) || self.keys.matches(Action::ClearInput, k) {
            self.input.clear();
            self.cursor = 0;
            self.popup = None;
            return;
        }
        // the picker grabs navigation keys while it is open
        if self.picker.is_some() {
            match k.code {
                KeyCode::Up => {
                    if let Some(p) = self.picker.as_mut() {
                        p.sel = p.sel.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    if let Some(p) = self.picker.as_mut() {
                        p.sel = (p.sel + 1).min(p.items.len().saturating_sub(1));
                    }
                }
                KeyCode::Esc => self.picker = None,
                KeyCode::Enter | KeyCode::Tab => {
                    let picked = self
                        .picker
                        .as_ref()
                        .and_then(|p| p.items.get(p.sel).map(|i| (p.kind, i.clone())));
                    self.picker = None;
                    if let Some((kind, item)) = picked {
                        match kind {
                            PickerKind::Model => {
                                if item.warn {
                                    self.push(
                                        format!("! {} has no api key configured", item.id),
                                        Tone::Muted,
                                    );
                                }
                                self.run_slash(&format!("/model {}", item.id), serve);
                            }
                            PickerKind::Session => {
                                self.push(format!("↻ resuming session {} …", item.id), Tone::Muted);
                                self.switch_session(Some(item.id));
                            }
                        }
                    }
                }
                _ => {}
            }
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
        if self.keys.matches(Action::ScrollUp, k) {
            self.scroll_back += 1;
            return;
        }
        if self.keys.matches(Action::ScrollDown, k) {
            self.scroll_back = self.scroll_back.saturating_sub(1);
            return;
        }
        if self.keys.matches(Action::PageUp, k) {
            self.scroll_back += 20;
            return;
        }
        if self.keys.matches(Action::PageDown, k) {
            self.scroll_back = self.scroll_back.saturating_sub(20);
            return;
        }
        if self.keys.matches(Action::Newline, k) {
            self.input.insert(self.cursor, '\n');
            self.cursor += 1;
            self.refresh_popup();
            return;
        }
        if self.vim && self.edit_mode == EditMode::Normal && self.normal_edit(k.code) {
            return;
        }
        match k.code {
            KeyCode::Enter if !self.busy => self.send_prompt(serve),
            KeyCode::Enter
                if self
                    .input
                    .iter()
                    .collect::<String>()
                    .trim_start()
                    .starts_with('/') =>
            {
                let text: String = std::mem::take(&mut self.input).iter().collect();
                self.cursor = 0;
                self.run_slash(&text, serve); // commands stay live during a turn
            }
            KeyCode::Enter => {
                self.push("[queued — ctrl+c interrupts]", Tone::Muted);
            }
            KeyCode::Char(c) if !ctrl && (!self.vim || self.edit_mode == EditMode::Insert) => {
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
            _ => {}
        }
    }

    fn answer(&mut self, serve: &mut Serve, allow: bool) {
        if let Some(p) = self.permission.take() {
            serve.notify(json!({ "cmd": "permission", "request": p.request, "allow": allow }));
            self.push(
                format!(
                    "{} {} — {}",
                    if allow { "✓ allowed" } else { "✗ denied" },
                    p.name,
                    p.reason
                ),
                if allow { Tone::Muted } else { Tone::Error },
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
            self.push(format!("! {w}"), Tone::Muted);
        }
        self.push_user(&text);
        self.scroll_back = 0;
        self.busy = true;
        self.prompt_id = serve.command("prompt", vec![("text", Value::String(expanded))]);
    }

    fn frame(&mut self, v: Value) {
        let t = v.get("type").and_then(Value::as_str).unwrap_or("");
        match t {
            "hello" => {
                self.keys = Keymap::from_json(v.get("keys"));
                self.vim = v["editor"].as_str() == Some("vim");
                self.edit_mode = EditMode::Insert;
                self.theme = Theme::resolve(
                    v["theme"].as_str(),
                    std::env::var("COLORFGBG").ok().as_deref(),
                );
                self.model = v["model"].as_str().unwrap_or("?").into();
                self.mode = v["mode"].as_str().unwrap_or("?").into();
                self.session = v["sessionId"]
                    .as_str()
                    .unwrap_or("?")
                    .chars()
                    .take(8)
                    .collect();
                self.cost = v["totalCostUsd"].as_f64().unwrap_or(0.0);
                let n = v["tools"].as_array().map_or(0, |a| a.len());
                self.push(
                    format!("tango serve · session {} · {n} tools · /help · esc/ctrl+c interrupt · /exit quits", self.session),
                    Tone::Muted,
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
                        Tone::Muted,
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
                let style = if ok { Tone::Muted } else { Tone::Error };
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
                self.push(format!("? {name}: {reason}"), Tone::Warning);
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
                        // the same failure usually arrived a moment ago as an `error` frame
                        let msg = format!("error: {}", v["error"].as_str().unwrap_or("?"));
                        if self.lines.last().map(|l| l.text.as_str()) != Some(msg.as_str()) {
                            self.push(msg, Tone::Error);
                        }
                    }
                    return;
                }
                if let Some(pos) = self.pending.iter().position(|(pid, _)| *pid == id) {
                    let kind = self.pending.remove(pos).1;
                    let d = &v["data"];
                    match (kind, ok) {
                        (Pending::Models, true) => {
                            let cur = d["current"].as_str().unwrap_or("").to_string();
                            let items: Vec<PickItem> = d["models"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|m| {
                                            let id = m["id"].as_str()?.to_string();
                                            let cw = m["contextWindow"].as_u64().unwrap_or(0);
                                            Some(PickItem {
                                                warn: !m["keyed"].as_bool().unwrap_or(false),
                                                note: if cw == 0 {
                                                    String::new()
                                                } else {
                                                    format!("{}k", cw / 1000)
                                                },
                                                current: id == cur,
                                                label: id.clone(),
                                                id,
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let sel = items.iter().position(|i| i.current).unwrap_or(0);
                            self.picker = Some(Picker {
                                kind: PickerKind::Model,
                                items,
                                sel,
                            });
                        }
                        (Pending::Sessions, true) => {
                            let items: Vec<PickItem> = d["sessions"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|s| {
                                            let id = s["id"].as_str()?.to_string();
                                            Some(PickItem {
                                                note: {
                                                    let name = s["name"].as_str().unwrap_or("");
                                                    if name.is_empty() {
                                                        s["cwd"].as_str().unwrap_or("").to_string()
                                                    } else {
                                                        name.to_string()
                                                    }
                                                },
                                                warn: false,
                                                current: s["current"].as_bool().unwrap_or(false),
                                                label: format!(
                                                    "{} · {} msgs · {}",
                                                    s["id"]
                                                        .as_str()
                                                        .unwrap_or("")
                                                        .chars()
                                                        .take(8)
                                                        .collect::<String>(),
                                                    s["messages"].as_u64().unwrap_or(0),
                                                    s["createdAt"]
                                                        .as_str()
                                                        .unwrap_or("")
                                                        .replace('T', " ")
                                                ),
                                                id,
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            if items.is_empty() {
                                self.push("no sessions yet", Tone::Muted);
                            } else {
                                let sel = items.iter().position(|i| i.current).unwrap_or(0);
                                self.picker = Some(Picker {
                                    kind: PickerKind::Session,
                                    items,
                                    sel,
                                });
                            }
                        }
                        (Pending::Model, true) => {
                            self.model = d["model"].as_str().unwrap_or("?").into();
                            self.push(
                                format!("model → {} (saved as default)", self.model),
                                Tone::Muted,
                            );
                            if let Some(w) = d["warning"].as_str() {
                                self.push(format!("! {w}"), Tone::Warning);
                            }
                        }
                        (Pending::Mode, true) => {
                            self.mode = d["mode"].as_str().unwrap_or("?").into();
                            self.push(format!("mode → {}", self.mode), Tone::Muted);
                        }
                        (Pending::Compact, true) => self.push(
                            format!(
                                "compact: {} · context {} messages",
                                d["compacted"].as_bool().unwrap_or(false),
                                d["contextMessages"].as_u64().unwrap_or(0)
                            ),
                            Tone::Muted,
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
                                Tone::Muted,
                            );
                        }
                        (Pending::Skill, true) => {
                            if let Some(f) = d["draft"].as_str() {
                                self.push(format!("draft → {f}"), Tone::Muted);
                                self.push(
                                    "edit its description + steps, then: /skill finish <file>",
                                    Tone::Muted,
                                );
                            } else if let Some(f) = d["skill"].as_str() {
                                self.push(format!("skill → {f}"), Tone::Success);
                            } else if let Some(list) = d["drafts"].as_array() {
                                self.push("usage: /skill finish <draft file>", Tone::Muted);
                                for f in list.iter().filter_map(|x| x.as_str()) {
                                    self.push(format!("  {f}"), Tone::Muted);
                                }
                            }
                        }
                        (Pending::History, true) => self.load_history(d),
                        (_, false) => self.push(
                            format!("error: {}", v["error"].as_str().unwrap_or("?")),
                            Tone::Error,
                        ),
                    }
                }
            }
            "error" => self.push(
                format!("error: {}", v["message"].as_str().unwrap_or("?")),
                Tone::Error,
            ),
            _ => {}
        }
    }

    fn load_history(&mut self, d: &Value) {
        let items = match d["items"].as_array() {
            Some(a) if !a.is_empty() => a,
            _ => return,
        };
        let total = d["total"].as_u64().unwrap_or(0) as usize;
        let mut lines = std::mem::take(&mut self.lines);
        let hello = match lines.last() {
            // the banner stays the newest line
            Some(l) if l.text.starts_with("tango serve") => lines.pop(),
            _ => None,
        };
        if total > items.len() {
            self.push(
                format!("… {} earlier items not shown", total - items.len()),
                Tone::Muted,
            );
        }
        for it in items {
            let text = it["text"].as_str().unwrap_or("");
            match it["kind"].as_str() {
                Some("user") => self.push_user(text),
                Some("assistant") => {
                    self.stream = text.to_string();
                    self.commit_stream();
                }
                Some("tool") => {
                    let name = it["name"].as_str().unwrap_or("?");
                    let ok = !it["isError"].as_bool().unwrap_or(false);
                    let style = if ok { Tone::Muted } else { Tone::Error };
                    let out = it["output"]
                        .as_str()
                        .map(|s| s.lines().take(200).collect::<Vec<_>>().join("\n"));
                    self.push_tool(
                        format!("⚙ {name} {}", Self::call_summary(&it["input"])),
                        style,
                        out,
                    );
                }
                _ => {}
            }
        }
        self.push("── resumed ──", Tone::Muted);
        let restored = std::mem::take(&mut self.lines);
        lines.extend(restored);
        lines.extend(hello);
        self.lines = lines;
    }

    // ---------- rendering ----------

    fn input_height(&self, width: u16) -> u16 {
        let text: String = self.input.iter().collect();
        let w = width.max(1) as usize;
        let h: usize = text
            .split('\n')
            .map(|l| display_width(l).div_ceil(w).max(1))
            .sum::<usize>()
            .max(1);
        (h.min(8)) as u16 + 1 // +1 for the rule above the input
    }

    fn chat_height(&self, area: Rect, input_h: u16) -> usize {
        (area.height.saturating_sub(1 + input_h)) as usize
    }

    /// the chat exactly as drawn — scroll math and mouse hit testing use the same lines
    fn chat_lines(&self) -> Vec<ChatLine> {
        self.lines
            .iter()
            .map(|l| ChatLine {
                text: l.text.clone(),
                tone: l.tone,
                output: None,
            })
            .collect()
    }

    fn visual_lines(lines: &[ChatLine], width: usize) -> usize {
        let w = width.max(1);
        lines
            .iter()
            .flat_map(|l| l.text.split('\n'))
            .map(|line| display_width(line).div_ceil(w).max(1))
            .sum()
    }

    fn line_at_visual_row(&self, width: usize, chat_height: usize, row: usize) -> Option<usize> {
        let total = Self::visual_lines(&self.lines, width);
        let offset = total.saturating_sub(chat_height.saturating_sub(self.scroll_back).max(1));
        let target = offset + row;
        let mut top = 0;
        for (index, line) in self.lines.iter().enumerate() {
            let height = Self::visual_lines(std::slice::from_ref(line), width);
            if target >= top && target < top + height {
                return line.output.is_some().then_some(index);
            }
            top += height;
        }
        None
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

        self.chat_area = chat_a;

        // status bar
        let work = if self.busy { "● working" } else { "idle" };
        let mut spans = vec![
            Span::styled(format!(" {} ", self.model), self.theme.style(Tone::Badge)),
            Span::styled(format!(" {} ", self.mode), self.theme.style(Tone::Muted)),
            Span::styled(
                format!(" ${:.4} ", self.cost),
                self.theme.style(Tone::Warning),
            ),
            Span::styled(
                format!(" {work} "),
                if self.busy {
                    self.theme.style(Tone::Success)
                } else {
                    self.theme.style(Tone::Muted)
                },
            ),
            Span::styled(
                format!(" sess {} ", self.session),
                self.theme.style(Tone::Muted),
            ),
        ];
        if self.vim {
            let mode = if self.edit_mode == EditMode::Insert {
                " INSERT "
            } else {
                " NORMAL "
            };
            spans.push(Span::styled(mode, self.theme.style(Tone::Muted)));
        }

        f.render_widget(Paragraph::new(Line::from(spans)), status_a);

        // chat with bottom-anchored scrolling (scroll_back=0 follows the newest line)
        let shown: Vec<Line> = self
            .chat_lines()
            .into_iter()
            .map(|l| Line::from(Span::styled(l.text, self.theme.style(l.tone))))
            .collect();
        let inner_w = chat_a.width as usize;
        let total = Self::visual_lines(&self.chat_lines(), inner_w)
            + if self.stream.is_empty() {
                0
            } else {
                display_width(&self.stream).div_ceil(inner_w.max(1))
            };
        let offset = total.saturating_sub(chat_h.saturating_sub(self.scroll_back).max(1));
        // live assistant text (not committed yet) hangs off the last line, markdown stripped
        let mut shown = shown;
        if !self.stream.is_empty() {
            let preview = self.stream.replace("**", "").replace('`', "");
            shown.push(Line::from(Span::raw(preview)));
        }
        f.render_widget(
            Paragraph::new(Text::from(shown))
                .wrap(Wrap { trim: false })
                .scroll((offset as u16, 0)),
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
                    let style = if i == p.sel {
                        self.theme.style(Tone::Badge)
                    } else {
                        self.theme.style(Tone::Muted)
                    };
                    let desc = p.descs.get(i).cloned().unwrap_or_default();
                    Line::from(Span::styled(format!(" {item:<24}{desc}"), style))
                })
                .collect();
            f.render_widget(Paragraph::new(rows), pop);
        }

        // input
        f.render_widget(
            Paragraph::new("─".repeat(area.width as usize)).style(self.theme.style(Tone::Muted)),
            Rect {
                x: 0,
                y: input_a.y,
                width: area.width,
                height: 1,
            },
        );
        let input_text: String = self.input.iter().collect();
        f.render_widget(
            Paragraph::new(input_text.clone()).wrap(Wrap { trim: false }),
            Rect {
                x: 0,
                y: input_a.y + 1,
                width: area.width,
                height: input_a.height.saturating_sub(1),
            },
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
        f.set_cursor_position(Position::new(
            input_a.x + col as u16,
            input_a.y + 1 + row as u16,
        ));

        // model/session picker overlay
        if let Some(p) = &self.picker {
            let h = (p.items.len().min(14) + 3) as u16;
            let w = (area.width.min(72)).max(44);
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            f.render_widget(Clear, pop);
            let label_w = (w as usize).saturating_sub(30);
            // keep the selection inside the 14-row window
            let start = if p.sel < 13 { 0 } else { p.sel - 12 };
            let win = &p.items[start..(start + 14).min(p.items.len())];
            let mut rows: Vec<Line> = win
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    let i = i + start;
                    let marks = format!(
                        "{}{} {}",
                        if it.warn { "⚠" } else { " " },
                        if it.current { "✓" } else { " " },
                        if i == p.sel { "▸" } else { " " }
                    );
                    let style = if it.warn {
                        self.theme.style(Tone::Muted)
                    } else if i == p.sel {
                        self.theme.style(Tone::Selected)
                    } else {
                        self.theme.style(Tone::Text)
                    };
                    let pad = label_w.saturating_sub(display_width(&it.label));
                    let note: String = it
                        .note
                        .chars()
                        .take((w as usize).saturating_sub(label_w + 10))
                        .collect();
                    Line::from(Span::styled(
                        format!(" {marks} {}{} {}", it.label, " ".repeat(pad), note),
                        style,
                    ))
                })
                .collect();
            if p.items.len() > 14 {
                rows.push(Line::from(Span::styled(
                    format!("   {}–{} / {}", start + 1, start + win.len(), p.items.len()),
                    self.theme.style(Tone::Muted),
                )));
            }
            rows.push(Line::from(""));
            let hint = match p.kind {
                PickerKind::Model => " ↑↓ 选择 · enter 切换 · esc 取消 · ⚠=缺 key · ✓=当前",
                PickerKind::Session => " ↑↓ 选择 · enter 恢复会话(重启 serve) · esc 取消 · ✓=当前",
            };
            rows.push(Line::from(Span::styled(
                hint,
                self.theme.style(Tone::Muted),
            )));
            let title = match p.kind {
                PickerKind::Model => " 选择模型 ",
                PickerKind::Session => " 选择会话 ",
            };
            f.render_widget(
                Paragraph::new(rows).block(Block::bordered().title(title)),
                pop,
            );
        }

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
                Line::from(Span::styled(
                    p.reason.clone(),
                    self.theme.style(Tone::Warning),
                )),
                Line::from(""),
                Line::from(Span::raw(p.detail.chars().take(240).collect::<String>())),
                Line::from(""),
                Line::from(Span::styled(
                    "y allow · n deny · a always (mode → auto)",
                    self.theme.style(Tone::Selected),
                )),
            ])
            .wrap(Wrap { trim: true })
            .block(Block::bordered().title(format!(" {} ", p.name)));
            f.render_widget(body, popup);
        }

        if let Some(viewer) = self.viewer.as_mut() {
            let w = (area.width * 9 / 10).max(20).min(area.width);
            let h = (area.height * 9 / 10).max(8).min(area.height);
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            let inner_width = w.saturating_sub(2).max(1) as usize;
            let inner_height = h.saturating_sub(2) as usize;
            let visual = viewer
                .text
                .lines()
                .map(|line| display_width(line).div_ceil(inner_width).max(1))
                .sum::<usize>();
            viewer.max_scroll = visual.saturating_sub(inner_height);
            viewer.scroll = viewer.scroll.min(viewer.max_scroll);
            f.render_widget(Clear, pop);
            let body = Paragraph::new(viewer.text.clone())
                .wrap(Wrap { trim: false })
                .scroll((viewer.scroll.min(u16::MAX as usize) as u16, 0))
                .block(Block::bordered().title(format!(
                    " {} · esc/ctrl+o 关闭 · ↑↓/滚轮滚动 ",
                    viewer.title
                )));
            f.render_widget(body, pop);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_chat_lines_use_semantic_tones() {
        let mut app = App::new();
        app.push("old", Tone::Muted);
        app.theme = Theme::light();
        let lines = app.chat_lines();
        assert_eq!(lines[0].tone, Tone::Muted);
        assert_eq!(
            app.theme.style(lines[0].tone),
            Theme::light().style(Tone::Muted)
        );
    }

    #[test]
    fn hello_resolves_theme() {
        let mut app = App::new();
        app.frame(json!({
            "type": "hello", "sessionId": "12345678", "model": "mock/mock",
            "mode": "ask", "theme": "light", "tools": []
        }));
        assert!(app.theme.light);
    }

    #[test]
    fn long_user_prompt_is_collapsed_but_full_text_is_retained() {
        let mut app = App::new();
        let text = (1..=30)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push_user(&text);
        assert_eq!(app.lines.len(), 1);
        assert!(app.lines[0].text.contains("粘贴"));
        assert!(app.lines[0].text.contains("line-1"));
        assert!(!app.lines[0].text.contains("line-30"));
        assert_eq!(app.lines[0].output.as_deref(), Some(text.as_str()));
    }

    #[test]
    fn short_user_prompt_stays_inline() {
        let mut app = App::new();
        app.push_user("hello\nworld");
        assert_eq!(app.lines[0].text, "❯ hello ⏎ world");
        assert_eq!(app.lines[0].output, None);
    }

    #[test]
    fn latest_full_text_opens_in_viewer_without_moving_chat() {
        let mut app = App::new();
        app.push_user(&"x".repeat(1000));
        app.scroll_back = 7;
        app.open_latest_viewer();
        assert_eq!(app.scroll_back, 7);
        assert_eq!(app.viewer.as_ref().unwrap().text, "x".repeat(1000));
    }

    #[test]
    fn click_row_maps_to_the_visible_expandable_card() {
        let mut app = App::new();
        app.push_tool("first", Tone::Muted, Some("one".into()));
        app.push_tool("second", Tone::Muted, Some("two".into()));
        app.push_tool("third", Tone::Muted, Some("three".into()));
        assert_eq!(app.line_at_visual_row(80, 2, 0), Some(1));
        assert_eq!(app.line_at_visual_row(80, 2, 1), Some(2));
        assert_eq!(app.line_at_visual_row(80, 2, 2), None);
    }

    #[test]
    fn hello_configures_custom_keys_and_vim() {
        let mut app = App::new();
        app.frame(json!({
            "type": "hello", "sessionId": "12345678", "model": "mock/mock",
            "mode": "ask", "editor": "vim", "keys": { "expandTools": "ctrl+u" }, "tools": []
        }));
        assert!(app.keys.matches(
            Action::ExpandTools,
            crossterm::event::KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
        ));
        assert!(app.vim);
        assert_eq!(app.edit_mode, EditMode::Insert);
    }

    #[test]
    fn vim_normal_mode_moves_deletes_and_returns_to_insert() {
        let mut app = App::new();
        app.vim = true;
        app.edit_mode = EditMode::Normal;
        app.input = "abc".chars().collect();
        app.cursor = 3;
        assert!(app.normal_edit(KeyCode::Char('h')));
        assert_eq!(app.cursor, 2);
        assert!(app.normal_edit(KeyCode::Char('x')));
        assert_eq!(app.input.iter().collect::<String>(), "ab");
        assert!(app.normal_edit(KeyCode::Char('0')));
        assert_eq!(app.cursor, 0);
        assert!(app.normal_edit(KeyCode::Char('$')));
        assert_eq!(app.cursor, 2);
        assert!(app.normal_edit(KeyCode::Char('i')));
        assert_eq!(app.edit_mode, EditMode::Insert);
    }

    #[test]
    fn hello_restores_session_cost() {
        let mut app = App::new();
        app.frame(json!({
            "type": "hello",
            "sessionId": "12345678-abcd",
            "model": "mock/mock",
            "mode": "ask",
            "totalCostUsd": 1.25,
            "tools": []
        }));
        assert_eq!(app.cost, 1.25);
    }
}
