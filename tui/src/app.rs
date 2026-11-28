use crate::child::{Event, Serve};
use crate::dialog::{HINT_ROWS, HintButton, render_dialog_buttons};
use crate::keys::{Action, Keymap};
use crate::nvim::{EditResolution, Neovim, NvimRequest, NvimTarget, edit_submission};
use crate::theme::{Theme, Tone};
use crate::viewer::{PromptState, Viewer};
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::{Value, json};
use std::cmp::min;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

/// Recognize a pasted image: terminals drop a clipboard image to a temp file and paste
/// its path (e.g. `/var/folders/…/orca-paste-….png`). A single-line paste that names an
/// existing image file becomes an attachment chip instead of raw path text.
/// Fuzzy @file search over repo paths: `query`'s characters must appear in order (case-insensitive).
/// Ranked: substring of the file name, then subsequence of the file name, then of the whole path;
/// shorter paths first within a rank. At most 12.
fn fuzzy_files(query: &str, files: &[String]) -> Vec<String> {
    if query.is_empty() {
        return Vec::new();
    }
    let q = query.to_lowercase();
    let subseq = |hay: &str| {
        let mut it = hay.chars();
        q.chars().all(|c| it.any(|h| h == c))
    };
    let mut hits: Vec<(u8, usize, &String)> = files
        .iter()
        .filter_map(|f| {
            let lower = f.to_lowercase();
            let name = lower.rsplit('/').next().unwrap_or(&lower);
            let rank = if name.contains(&q) {
                0
            } else if subseq(name) {
                1
            } else if subseq(&lower) {
                2
            } else {
                return None;
            };
            Some((rank, f.len(), f))
        })
        .collect();
    hits.sort();
    hits.into_iter().take(12).map(|(_, _, f)| f.clone()).collect()
}

fn image_path_from_paste(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.contains('\n') {
        return None;
    }
    // drag-and-drop from some file managers wraps the path in quotes
    let t = t.trim_matches(|c| c == '\'' || c == '"');
    let lower = t.to_lowercase();
    if ![".png", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|ext| lower.ends_with(ext))
    {
        return None;
    }
    let expanded = if let Some(rest) = t.strip_prefix("~/") {
        match std::env::var("HOME") {
            Ok(home) => format!("{home}/{rest}"),
            Err(_) => t.to_string(),
        }
    } else {
        t.to_string()
    };
    Path::new(&expanded).is_file().then_some(expanded)
}

/// A submission that starts with an existing file or directory path (pasted or dragged in,
/// so it begins with `/`, `~/` or a quote) is a prompt about that file, not a slash
/// command. Known commands always win. Returns the resolved path, whether it is a
/// directory, and the text after it. Paths with spaces work quoted, backslash-escaped
/// (drag and drop) or bare: the longest existing prefix of the first line wins.
fn leading_path(text: &str) -> Option<(String, bool, String)> {
    let t = text.trim_start();
    let word = t.split_whitespace().next().unwrap_or("").to_lowercase();
    if word == ":q" || SLASH.iter().any(|(c, _)| c.split(' ').next() == Some(word.as_str())) {
        return None;
    }
    let unquoted = t.trim_start_matches(['\'', '"']);
    if !(unquoted.starts_with('/') || unquoted.starts_with("~/")) {
        return None;
    }
    let line = &t[..t.find('\n').unwrap_or(t.len())];
    let resolve = |raw: &str| -> Option<String> {
        let raw = raw.trim_end();
        let raw = match (raw.chars().next(), raw.chars().last()) {
            (Some(q @ ('\'' | '"')), Some(e)) if q == e && raw.len() > 1 => &raw[1..raw.len() - 1],
            _ => raw,
        };
        let mut path = String::new();
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            // drag and drop escapes spaces and shell characters: `My\ File.md`
            path.push(if c == '\\' { chars.next().unwrap_or('\\') } else { c });
        }
        if let Some(rest) = path.strip_prefix("~/") {
            path = format!("{}/{rest}", std::env::var("HOME").ok()?);
        }
        let p = Path::new(&path);
        (path.len() > 1 && p.exists()).then_some(path)
    };
    let mut cuts: Vec<usize> = line
        .char_indices()
        .filter(|(i, c)| c.is_whitespace() && !line[..*i].ends_with('\\'))
        .map(|(i, _)| i)
        .collect();
    cuts.push(line.len());
    for &cut in cuts.iter().rev() {
        if let Some(path) = resolve(&line[..cut]) {
            let is_dir = Path::new(&path).is_dir();
            return Some((path, is_dir, t[cut..].trim().to_string()));
        }
    }
    None
}

/// The prompt for a submission that leads with a path: name it as a file (or directory),
/// and when nothing else was written, ask for what a bare path almost always means.
fn file_prompt(text: &str) -> Option<String> {
    let (path, is_dir, rest) = leading_path(text)?;
    let label = if is_dir { "目录" } else { "文件" };
    Some(if rest.is_empty() {
        if is_dir {
            format!("{label}：{path}\n请查看这个目录，了解里面有什么。")
        } else {
            format!("{label}：{path}\n请阅读这个文件，理解它的内容。")
        }
    } else {
        format!("{label}：{path}\n{rest}")
    })
}

/// Command extras for a prompt/supplement: text plus optional image paths to upload.
fn prompt_extras(text: String, images: Vec<String>) -> Vec<(&'static str, Value)> {
    let mut extras = vec![("text", Value::String(text))];
    if !images.is_empty() {
        extras.push(("images", json!(images)));
    }
    extras
}

/// Rough cell width: CJK/fullwidth codepoints take 2 cells. Good enough for scroll math and the cursor.
/// Display width in terminal columns. Delegates to unicode-width — the same
/// crate ratatui uses to lay out text — so click/selection mapping can never
/// drift from what's actually on screen (emoji counted 1 by the old hand-rolled
/// table rendered as 2 columns, shifting every mapping right of the cursor).
pub fn display_width(s: &str) -> usize {
    use unicode_width::UnicodeWidthStr;
    s.width()
}

/// A ⚙ line's call is cut to fit the chat width at draw time; this is the width
/// used before the first draw (and for the stored line text).
const BRIEF_CHARS: usize = 160;

/// The call part of a ⚙ line, fitted into `avail` columns: the first line, cut
/// with `…` when it is too wide, plus `（共 N 行）` when the call has more lines.
fn fit_call(call: &str, avail: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let n = call.lines().count();
    let first = call.lines().next().unwrap_or("").trim_end();
    let more = if n > 1 { format!("（共 {n} 行）") } else { String::new() };
    if n <= 1 && display_width(first) <= avail {
        return first.to_string();
    }
    // on a very narrow screen the command may shrink to just "…": one row still wins
    let room = avail.saturating_sub(1 + display_width(&more));
    let mut out = String::new();
    let mut w = 0;
    for c in first.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > room {
            break;
        }
        out.push(c);
        w += cw;
    }
    if out.len() < first.len() || n > 1 {
        out.push('…');
    }
    out + &more
}

/// Collapsed thinking shows this many lines / chars before the expand hint.
const THINK_PREVIEW_LINES: usize = 3;
const THINK_PREVIEW_CHARS: usize = 240;

/// The thinking icon: one robot emoji (2 cells; unicode-width agrees with the
/// renderer). The live block's motion comes from the ▍ bar's color wave.
const THINK_ICON: &str = "🤖";
/// draft text column inside the input block: the mode-colored bar, then one gap
const INPUT_X: u16 = 2;
/// ASCII mode's thinking icon.
const THINK_ICON_ASCII: &str = "*";

/// ASCII mode: emoji become plain ASCII stand-ins (known ones by meaning, any
/// other wide emoji as `*`); variation selectors and ZWJ vanish.
fn asciify(s: &str) -> String {
    use unicode_width::UnicodeWidthChar;
    s.chars()
        .filter(|c| !matches!(c, '\u{FE0F}' | '\u{200D}'))
        .map(|c| match c {
            '❓' | '❔' => '?',
            '➕' => '+',
            '📂' | '📁' => '>',
            '📋' => '#',
            '✅' | '✔' => 'v',
            '❌' => 'x',
            '⚠' => '!',
            c if (c as u32 >= 0x1F000 || (0x2600..0x27C0).contains(&(c as u32)))
                && c.width() == Some(2) =>
            {
                '*'
            }
            c => c,
        })
        .collect()
}

/// What a thinking line shows: short blocks in full; long ones as a preview with the
/// hint glued after its …, or in full plus a collapse hint when expanded. `teach`: the
/// first collapsed block of the run names the gesture; later ones only say how much
/// is hidden — the double-click is learned once, repeating it on every block is noise.
fn thinking_view(text: &str, expanded: bool, teach: bool) -> String {
    let chars = text.chars().count();
    let lines = text.lines().count();
    if lines <= THINK_PREVIEW_LINES && chars <= THINK_PREVIEW_CHARS {
        return text.to_string();
    }
    if expanded {
        return format!("{text}\n双击收起思考");
    }
    let mut preview = String::new();
    for (i, line) in text.lines().take(THINK_PREVIEW_LINES).enumerate() {
        if i > 0 {
            preview.push('\n');
        }
        preview.push_str(line);
    }
    let preview: String = preview.chars().take(THINK_PREVIEW_CHARS).collect();
    let hidden = chars.saturating_sub(preview.chars().count());
    if teach {
        format!("{preview} … 双击展开思考（还有 {hidden} 字 · 共 {lines} 行）")
    } else {
        format!("{preview} …（还有 {hidden} 字 · 共 {lines} 行）")
    }
}

/// Draws a dialog box, also keeping its hint keys as click targets (topmost dialog last).
fn dialog_frame(
    buttons: &mut Vec<HintButton>,
    f: &mut Frame,
    area: Rect,
    title: &str,
    hint: &str,
    theme: Theme,
) -> Rect {
    let (inner, mut found) = render_dialog_buttons(f, area, title, hint, theme);
    buttons.append(&mut found);
    inner
}

/// A session id as people type it: a uuid's last '-' part (12 hex digits, which
/// `/resume`, `--resume` and `/send` accept); any other id stays whole.
fn short_session(id: &str) -> &str {
    match id.rsplit_once('-') {
        Some((_, tail))
            if id.len() == 36 && tail.len() == 12 && tail.bytes().all(|b| b.is_ascii_hexdigit()) =>
        {
            tail
        }
        _ => id,
    }
}

/// Cut `s` to at most `width` columns, ending in … when anything was dropped.
fn clip_to_width(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if display_width(s) <= width {
        return format!("{s}{}", " ".repeat(width - display_width(s)));
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

/// how long a first Ctrl+C on an empty input waits for the second one
const EXIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(2);
const EXIT_HINT: &str = "再按一次 Ctrl+C 退出";

/// The language a conversation is written in — for the few phrases the TUI itself
/// writes into it (the choice ask and Tab's lead-in), so they match what the user
/// and the model are speaking instead of always being Chinese.
#[derive(Clone, Copy, PartialEq, Debug)]
enum ChatLang {
    Zh,
    Ja,
    Ko,
    En,
}

impl ChatLang {
    const ALL: [ChatLang; 4] = [ChatLang::Zh, ChatLang::Ja, ChatLang::Ko, ChatLang::En];

    /// The language of `text`, or None when it has no letters to tell by.
    fn of(text: &str) -> Option<ChatLang> {
        let mut cjk = false;
        for c in text.chars() {
            match c {
                // CJK ideographs (Chinese, or kanji — kana below decide Japanese)
                '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}' | '\u{f900}'..='\u{faff}' => cjk = true,
                '\u{3040}'..='\u{30ff}' | '\u{31f0}'..='\u{31ff}' | '\u{ff66}'..='\u{ff9d}' => {
                    return Some(ChatLang::Ja)
                }
                '\u{ac00}'..='\u{d7a3}' | '\u{1100}'..='\u{11ff}' => return Some(ChatLang::Ko),
                c if c.is_alphabetic() && !('\u{3000}'..='\u{303f}').contains(&c) => {
                    // letters of a non-CJK script (ASCII, Latin with accents, Cyrillic…):
                    // the TUI's other-language wording, English
                    if !cjk {
                        return Some(ChatLang::En);
                    }
                }
                _ => {}
            }
        }
        cjk.then_some(ChatLang::Zh)
    }

    /// "Tango is waiting for your choice"
    fn waiting(self) -> &'static str {
        match self {
            ChatLang::Zh => "Tango 在等你选择",
            ChatLang::Ja => "Tango が選択を待っています",
            ChatLang::Ko => "Tango가 선택을 기다리고 있습니다",
            ChatLang::En => "Tango is waiting for your choice",
        }
    }

    /// What opens the lead-in that quotes the question: "关于" / "About: ".
    fn about(self) -> &'static str {
        match self {
            ChatLang::Zh => "关于",
            ChatLang::Ja => "「",
            ChatLang::Ko => "",
            ChatLang::En => "About: ",
        }
    }

    /// What follows the quoted question, before the waiting phrase.
    fn about_sep(self) -> &'static str {
        match self {
            ChatLang::Zh => "，",
            ChatLang::Ja => "」、",
            ChatLang::Ko => "에 대해 ",
            ChatLang::En => " ",
        }
    }
}

impl Default for ChatLang {
    fn default() -> Self {
        ChatLang::Zh
    }
}

struct ChatLine {
    text: String,
    tone: Tone,
    /// wall time recorded when the line was pushed ([HH:MM] for /timestamps)
    ts: Option<String>,
    /// full tool output or collapsed user prompt; shown in the independent viewer
    output: Option<String>,
    prompt_state: Option<PromptState>,
    /// live tool-call id while this ⚙ line's tool is still running (drives the
    /// gear's animated color); the result line clears it by id
    tool_id: Option<String>,
    source_index: usize,
    /// pushed from a serve `notice` frame (system status, not conversation): the
    /// empty-session splash keeps showing and lists these under the wordmark
    notice: bool,
    /// a thinking block shown in full instead of its collapsed preview
    expanded: bool,
    /// a ⚙ line's full call detail (e.g. the whole bash command); the line shows
    /// it fitted to one row, the viewer and transcript show it whole
    call: Option<String>,
    /// the ⚙ line around its call — (`⚙ bash `, ` · 7ms · 已中断`) — so chat_lines
    /// can refit the call to the current width
    frame: Option<(String, String)>,
    /// the splash wordmark kept in the chat once the conversation starts: its 5 rows,
    /// drawn with the gradient instead of the wrapped text; never persisted
    wordmark: bool,
}

struct Permission {
    request: u64,
    name: String,
    detail: String,
    reason: String,
    /// plain-language summary of what the command does (serve-side briefCommand)
    brief: String,
}

#[derive(Clone, Copy, PartialEq)]
enum Pending {
    SessionPreview,
    EditLast,
    TimelineMenu,
    Model,
    Models,
    Sessions,
    /// /resume [id or prefix]: serve resolves it first, the switch happens only on a unique hit
    ResumeResolve,
    Mode,
    Compact,
    Status,
    Skill,
    History,
    NvimViewer,
    Theme,
    Mouse,
    Diff,
    Btw,
    Timeline,
    Rewind,
    Undo,
    Redo,
    Fork,
    Handoff,
    /// /send <session> <text> — a short message to another session's user
    Send,
    HandoffList,
    HandoffCancel,
    Takeover,
    Branch,
    Tree,
    Find,
    Export,
    Transcript,
    Rename,
    Recap,
    Context,
    Usage,
    Insights,
    AutocompactShow,
    AutocompactSet,
    Statusline,
    PresetSave,
    PresetList,
    PresetGet,
    PermissionsList,
    PermissionsAdd,
    /// a second+ rule of one "a" press (multi-segment command): saved silently
    PermissionsAddQuiet,
    Import,
    PermissionsRemove,
    ConfigShow,
    Pwd,
    Cd,
    AddDir,
    Init,
    Doctor,
    DebugZip,
    Plan,
    Effort,
    Memory,
    Remember,
    Skills,
    Goal,
    Loop,
    Worktree,
    Bg,
    Tasks,
    StopTask,
    AgentsShow,
    AgentsSet,
    Subtask,
    McpList,
    McpAdd,
    PluginsList,
    Trust,
    TrustStatus,
    /// bare /cd: the remembered-directory picker
    Cwds,
    /// quiet fetches for argument completion: never print, never open a picker
    ArgModels,
    ArgSessions,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PickerKind {
    Model,
    /// /mode bare: the permission-mode picker (zcode-style dropdown)
    Mode,
    Session,
    /// /handoff without an id: pick the session to send this one's brief to
    HandoffTarget,
    /// /takeover without an id: pick the session whose work this one continues
    TakeoverSource,
    /// /send without an id: pick the session to message; the message is typed after
    SendTarget,
    Command,
    History,
    Theme,
    NvimBackend,
    EditResolution,
    Timeline,
    Find,
    Preset,
    Permissions,
    Skills,
    Worktree,
    /// first start in a project whose config would run commands or widen access
    Trust,
    /// bare /cd, or the session cwd is gone: pick a directory used before, or type one
    Cwd,
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
    /// optional section header (model picker: source platform, "free" first);
    /// empty = no grouping. Headers are rendered, never selectable.
    group: String,
}

struct Picker {
    kind: PickerKind,
    items: Vec<PickItem>,
    sel: usize,
    query: String,
    /// session pickers only: (id → cwd, branch) for the project/branch filters
    scopes: Vec<(String, String, String)>,
    /// the serve cwd and its git branch (empty = no filtering)
    cwd: String,
    branch: String,
    /// Ctrl+A: show sessions from every project, not just this cwd
    all_projects: bool,
    /// Ctrl+B: only sessions recorded on this branch
    branch_only: bool,
    /// Ctrl+R in progress: the session id being renamed (query holds the name)
    renaming: Option<String>,
    /// model picker: the reasoning levels each model takes (id → levels, from serve), and the
    /// level chosen for it with ←→ (id → level; absent = off, the endpoint's default). The
    /// choice rides along with Enter: `set_model { modelId, effort }`.
    efforts: std::collections::HashMap<String, Vec<String>>,
    effort_pick: std::collections::HashMap<String, String>,
}

impl Picker {
    fn new(kind: PickerKind, items: Vec<PickItem>) -> Self {
        let sel = items.iter().position(|item| item.current).unwrap_or(0);
        Self {
            kind,
            items,
            sel,
            query: String::new(),
            scopes: Vec::new(),
            cwd: String::new(),
            branch: String::new(),
            all_projects: false,
            branch_only: false,
            renaming: None,
            efforts: std::collections::HashMap::new(),
            effort_pick: std::collections::HashMap::new(),
        }
    }

    /// `←`/`→` on a model row: off → its first level → … → its last (`→` stays there), and
    /// `←` from the first goes back to off. A model without levels does nothing.
    fn step_effort(&mut self, forward: bool) {
        let Some(id) = self.selected_item().map(|item| item.id) else { return };
        let Some(levels) = self.efforts.get(&id).filter(|l| !l.is_empty()).cloned() else { return };
        let menu: Vec<String> = std::iter::once("off".to_string()).chain(levels).collect();
        let cur = self.effort_pick.get(&id).cloned().unwrap_or_else(|| "off".to_string());
        let at = menu.iter().position(|l| *l == cur).unwrap_or(0);
        let next = if forward { (at + 1).min(menu.len() - 1) } else { at.saturating_sub(1) };
        if next == 0 {
            self.effort_pick.remove(&id);
        } else {
            self.effort_pick.insert(id, menu[next].clone());
        }
    }

    /// the level to show next to `id` in the model picker: the chosen one, else "off"
    fn effort_shown(&self, id: &str) -> Option<&str> {
        if !self.efforts.get(id).is_some_and(|l| !l.is_empty()) {
            return None;
        }
        Some(self.effort_pick.get(id).map_or("off", String::as_str))
    }

    fn session_scoped(kind: PickerKind) -> bool {
        matches!(
            kind,
            PickerKind::Session
                | PickerKind::HandoffTarget
                | PickerKind::TakeoverSource
                | PickerKind::SendTarget
        )
    }

    fn filterable(&self) -> bool {
        matches!(
            self.kind,
            PickerKind::Model
                | PickerKind::Mode
                | PickerKind::Session
                | PickerKind::HandoffTarget
                | PickerKind::TakeoverSource
                | PickerKind::Command
                | PickerKind::History
                | PickerKind::Cwd
        )
    }

    fn filtered_items(&self) -> Vec<&PickItem> {
        // a typed path is not a filter: keep every remembered directory visible
        let raw = self.query.trim();
        if self.kind == PickerKind::Cwd
            && (raw.starts_with('/') || raw.starts_with('~') || raw.starts_with('.'))
        {
            return self.items.iter().collect();
        }
        let query = raw.to_lowercase();
        // every word must appear (as an in-order subsequence, separators skippable)
        // in the id, label or note — "glm54" finds zhipu-plan/glm-5.4. Matching stays
        // inside one token (a run of alphanumerics, '-', '.', '_'): matching across a
        // whole sentence glued English words together and pulled in noise ("resu" hit
        // "driver/subagent" inside a Chinese note).
        let words: Vec<String> = query.split_whitespace().map(String::from).collect();
        self.items
            .iter()
            .filter(|item| {
                words.is_empty()
                    || words.iter().all(|w| {
                        let needle: Vec<char> = w.chars().collect();
                        [&item.id, &item.label, &item.note].into_iter().any(|text| {
                            text.to_lowercase()
                                .split(|c: char| {
                                    !(c.is_alphanumeric() || matches!(c, '-' | '.' | '_'))
                                })
                                .any(|token| {
                                    let mut i = 0;
                                    for c in token.chars() {
                                        if Some(c) == needle.get(i).copied() {
                                            i += 1;
                                            if i == needle.len() {
                                                return true;
                                            }
                                        }
                                    }
                                    false
                                })
                        })
                    })
            })
            .filter(|item| {
                if !Self::session_scoped(self.kind) {
                    return true;
                }
                let Some((_, cwd, branch)) = self.scopes.iter().find(|(id, _, _)| *id == item.id)
                else {
                    return true; // unknown scope (mock frames): don't hide
                };
                if !self.all_projects && !self.cwd_filter().is_empty() && cwd != &self.cwd_filter()
                {
                    return false;
                }
                if self.branch_only
                    && !self.branch_filter().is_empty()
                    && (branch.is_empty() || branch != &self.branch_filter())
                {
                    return false;
                }
                true
            })
            .collect()
    }

    /// the project filter is the serve cwd; tests inject it directly
    fn cwd_filter(&self) -> String {
        self.cwd.clone()
    }

    fn branch_filter(&self) -> String {
        self.branch.clone()
    }

    fn selected_item(&self) -> Option<PickItem> {
        self.filtered_items()
            .get(self.sel)
            .map(|item| (*item).clone())
    }

    fn filter_key(&mut self, code: KeyCode, ctrl: bool) -> bool {
        if !self.filterable() || ctrl {
            return false;
        }
        match code {
            KeyCode::Char(c) => self.query.push(c),
            KeyCode::Backspace => {
                self.query.pop();
            }
            _ => return false,
        }
        self.sel = 0;
        true
    }
}

#[derive(Clone, Copy, PartialEq)]
enum PopKind {
    Slash,
    File,
    /// an argument of /mode /model /send /resume /effort: Tab cycles zsh-style
    Arg,
    /// /history: the conversation's prompts, oldest first; Enter jumps to the chosen one
    History,
}

struct Popup {
    items: Vec<String>,
    descs: Vec<String>,
    sel: usize,
    kind: PopKind,
    /// Arg only: the text Tab inserts per item (a session's full id behind its short label)
    inserts: Vec<String>,
    /// Arg only: a Tab already inserted items[sel]; the next Tab moves on (menu-complete)
    cycling: bool,
    /// History only: the chat line each item is (what Enter jumps to)
    lines: Vec<usize>,
}

#[derive(Clone, Copy)]
struct PastedSpan {
    start: usize,
    end: usize,
    id: usize,
    extra_lines: usize,
    /// Some(image id) when this span is a pasted image chip rather than collapsed text
    image: Option<usize>,
}

/// A pasted image: the original local path, kept so the chip can reopen it and the
/// next prompt can upload it. Ids are session-stable (the chat click registry uses them).
#[derive(Clone)]
struct ImageAttachment {
    id: usize,
    path: String,
}

/// A prompt waiting in the local queue: text plus any image paths riding with it.
#[derive(Clone, Debug, PartialEq)]
struct QueuedPrompt {
    text: String,
    images: Vec<String>,
}

impl QueuedPrompt {
    /// Tests build queues with it; the input path always goes through the constructor below.
    #[cfg(test)]
    fn text_only(text: &str) -> Self {
        Self {
            text: text.to_string(),
            images: Vec::new(),
        }
    }
}

struct InputDraft {
    input: Vec<char>,
    cursor: usize,
    pastes: Vec<PastedSpan>,
}

struct RenderedInput {
    text: String,
    cursor: usize,
    /// (display_start, display_end, image_id) for each image chip — mouse hit testing
    image_ranges: Vec<(usize, usize, usize)>,
    /// raw input range behind each display char: itself, or a whole paste for a label
    display_raw: Vec<(usize, usize)>,
}

struct InputLayout {
    lines: Vec<String>,
    /// display char index of each line's first char (selection highlight / hit mapping)
    starts: Vec<usize>,
    cursor_row: usize,
    cursor_col: usize,
}

/// /help, Tab-completion and the Ctrl+P palette. One entry per command word (aliases too).
/// Adding, renaming or removing a run_slash arm? Update this table, the reference section of
/// docs/handbook.md and README's command list; `slash_commands_stay_in_sync_with_help_and_docs`
/// fails until they agree.
/// how long a toast stays up
const TOAST_MS: u128 = 2000;

const SLASH: &[(&str, &str)] = &[
    (
        "/branch",
        "就地分叉：保留到该回合、丢弃其后（原未来可用 /redo 找回）",
    ),
    ("/btw <q>", "临时提问当前上下文，不写入会话"),
    (
        "/todo [内容]",
        "记一条待办（/todo 买菜），并打开待办列表 · 不带内容只开关列表",
    ),
    (
        "/problems",
        "语言服务器对 tango 改过的文件报告的问题（✗ 错误数也在状态栏）",
    ),
    (
        "/history",
        "本次对话里你输入过的提示词，新的在下 · ↑↓ 选择，回车跳到那一处",
    ),
    ("/clear", "清空屏幕（上下文保留）"),
    ("/compact", "压缩较早的上下文"),
    (
        "/autocompact [N%|0.N|off]",
        "查看/设置自动压缩阈值（默认 75%，持久化）",
    ),
    ("/cost", "本会话花费与上下文条数"),
    (
        "/density",
        "工具详情密度 0紧凑/1默认/2完整 · /density [0|1|2]",
    ),
    ("/diff", "只读查看工作区 diff · /diff --staged [paths]"),
    (
        "/editlast",
        "打断并回滚上一句，原文载入输入框改后重发（alt+e）",
    ),
    (
        "/editor",
        "用 $VISUAL/$EDITOR 编辑当前草稿（nvim 专用 /nvim）",
    ),
    ("/effort [档位|off]", "查看/设置推理力度 · 档位随当前模型而定"),
    ("/exit", "退出"),
    (
        "/export [md|html]",
        "导出完整会话（含被回滚的未来）为本地文件",
    ),
    ("/find <text>", "全库搜索对话（含被放弃的分支），可跳转"),
    ("/focus", "安静模式：隐藏成功工具与思考，保留错误"),
    ("/usage", "跨会话聚合：按模型/会话的请求与花费"),
    ("/stats", "同 /usage"),
    ("/fork <name>", "从当前（或 /rewind 选中）位置复制出新会话"),
    (
        "/send [session] [消息]",
        "给另一个 tango 会话发消息；对方空闲时收到并入上下文，裸 /send 选会话",
    ),
    (
        "/handoff [session] [note]",
        "把当前会话的交接摘要发给已有会话（不切换），裸 /handoff 选会话",
    ),
    (
        "/handoff list | cancel <id>",
        "查看排队中的交接摘要 / 撤回一条（未并入的摘要 7 天后过期丢弃）",
    ),
    (
        "/takeover [session] [note]",
        "把已有会话的工作接到当前会话（摘要并入上下文），裸 /takeover 选会话",
    ),
    (
        "/goal <text>|off|max N",
        "持续目标：每轮自动判断达成与否并续跑（上限轮数可调）",
    ),
    ("/rewind", "打开时间轴：代码+对话+上下文整体回滚"),
    ("/add_dir <dir>", "添加 cwd 之外的额外可写目录（持久化）"),
    (
        "/cd <dir>",
        "切换工作目录。裸 /cd 弹出用过的目录；/cd /路径 或 ~/路径 直接切换",
    ),
    ("/pwd", "显示当前工作目录"),
    (
        "/agents [role model]",
        "角色模型路由：driver/subagent/longContext/plan（/bg 自动用 subagent，plan 模式用 plan）",
    ),
    (
        "/bg [--long] <任务>",
        "后台任务：独立 worktree + 独立会话跑完（--long 用 longContext）",
    ),
    ("/tasks", "后台任务列表：状态与会话"),
    ("/stop <id>", "停止一个后台任务（id 或名字见 /tasks）"),
    (
        "/import claude-code|grok|codex",
        "导入其他代理的允许列表为 /permissions 规则（报告无法转换的条目）",
    ),
    ("/init", "生成 AGENTS.md 项目说明脚手架（--force 追加）"),
    ("/doctor", "环境体检：git/key/目录/版本"),
    ("/config", "查看当前配置（config.json）"),
    (
        "/debug-zip",
        "打包诊断信息 zip 到工作目录（webhook 已脱敏）",
    ),
    ("/feedback", "同 /debug-zip"),
    ("/help", "命令列表"),
    ("/insights", "生成 HTML 使用报告（离线可读）"),
    (
        "/keymap",
        "显示当前按键映射（在 config.json 的 keys 里自定义）",
    ),
    ("/interrupt", "停止当前回合"),
    ("/model <id>", "切换模型（存为默认），裸 /model 打开列表"),
    (
        "/name [名字]",
        "查看/重命名当前会话（选择窗 ctrl+r 改任意会话）",
    ),
    ("/mode <m>", "ask | auto | readonly | confirm | plan"),
    ("/auto", "同 /mode auto：完全放行，先过风险确认"),
    ("/plan", "计划模式开关：禁止一切写操作，只出执行计划"),
    (
        "/permissions [add|rm <rule>]",
        "允许规则：tool:<名> · path:<前缀> · bash:<命令前缀>",
    ),
    ("/mcp [add <名字>]", "MCP 服务器与桥接工具状态 · /mcp add 添加常用远程服务器"),
    ("/plugins", "本地插件列表（~/.tango/plugins）"),
    ("/memory", "查看长期记忆（全局 AGENTS.md，ctrl+e 编辑）"),
    ("/remember <text>", "记住一条偏好 → 全局记忆，所有会话生效"),
    ("/mouse", "切换鼠标捕获 · /mouse on|off"),
    ("/animations", "动画开关 · /animations on|off（off：静态颜色，文字不变）"),
    (
        "/clipboard [auto|osc52|native]",
        "复制方式：auto（SSH 下 OSC 52）· osc52 经终端转义（SSH/tmux 可用）· native 系统命令",
    ),
    ("/ascii", "ASCII 模式 · /ascii on|off（emoji 显示成 * ? + 等，适合没有 emoji 字体的终端）"),
    (
        "/bubble",
        "用户消息样式：on 靠右气泡（默认）· off 全宽靠左（Claude Code 式）",
    ),
    (
        "/loop <interval> <prompt>",
        "定间隔重复提示（如 /loop 10m 检查构建）· /loop off 停",
    ),
    ("/multiline", "Enter 换行/发送切换（ctrl+s 始终发送）"),
    ("/new", "开新会话"),
    ("/recap", "一行总结当前对话（不写入会话）"),
    ("/rename <name>", "重命名当前会话"),
    (
        "/review [paths]",
        "对工作区 diff 做代码评审 · /security-review 安全评审",
    ),
    ("/security-review", "对工作区 diff 做安全评审"),
    (
        "/commit-push",
        "提交本次改动并推送当前分支（提交信息跟随仓库语言）",
    ),
    (
        "/commit-pr",
        "提交本次改动并开 PR（在默认分支上会先建功能分支）",
    ),
    (
        "/queue [rm <n>|clear|auto append|auto off]",
        "本地排队管理 · Alt+Enter 补充到正在处理的回合 · auto append 让忙碌时回车也并入",
    ),
    (
        "/preset <name>|save <name>",
        "提示模板：保存当前输入 / 填入输入框（不发送）",
    ),
    ("/nvim", "用外部 Neovim 编辑草稿 · ctrl+e 编辑当前草稿"),
    ("/quit", "退出"),
    (
        "/restart",
        "退出并重启 TUI 与 serve（载入新构建），续在当前会话",
    ),
    ("/resume", "切换到历史会话，打开列表"),
    ("/sessions", "打开历史会话列表（切换会话）"),
    ("/back", "回到切换前的会话（像浏览器后退）"),
    ("/forward", "撤销 /back，前进到刚离开的会话"),
    (
        "/skill [title]",
        "把本会话存成技能草稿 · /skill finish <file> 定稿",
    ),
    ("/save-as-skill", "同 /skill"),
    ("/skills", "列出全局/项目技能"),
    ("/status", "会话信息"),
    (
        "/subagent <任务>",
        "派子代理在独立 worktree 干活，完成后摘要写回本会话",
    ),
    ("/subtask", "同 /subagent（旧名）"),
    (
        "/statusline [tpl|off]",
        "自定义状态栏模板：{model} {mode} {cost} {ctx} {pct} {session}",
    ),
    ("/theme", "选择并实时应用 dark/light/auto 主题"),
    (
        "/trust [yes|no]",
        "项目配置的 hooks/permissions/mcpServers 是否生效（按仓库记住）",
    ),
    ("/timestamps", "聊天行首 [HH:MM] 时间戳开关"),
    (
        "/transcript",
        "查看完整原始 transcript（含 ⟲ 被放弃的未来）",
    ),
    (
        "/context",
        "上下文构成：system/skills/工具/历史各占多少 token",
    ),
    ("/copy [N]", "复制最近（或倒数第 N 个）助手回复"),
    ("/tree", "查看会话分叉树"),
    ("/undo", "回退一个回合（/redo 前进一步）"),
    ("/redo", "前进一个回合（撤销 /undo）"),
    ("/vim", "切换 Vim 编辑 · ctrl+s 发送 · :wq/:wqa 发送"),
    (
        "/worktree [new|rm|merge <name>]",
        "受管 git worktree：后台/子代理的隔离单元",
    ),
];

/// inline @path tokens are expanded into file contents at send time (Claude Code style)
const AT_FILE_CAP: usize = 6000;

/// `/commit-push`: a preset prompt, not git plumbing. The model does the work with its usual
/// tools, so the permission mode still gates `git push`. Rules grok's version leaves out: stage
/// by explicit path (other sessions may share the checkout), run the repo's check first, never
/// force-push, stay on the current branch, and write the message in the repository's language.
pub const COMMIT_PUSH_PROMPT: &str = "\
(/commit-push) Commit the current changes and push this branch.

1. Look first: git status, git diff (staged and unstaged), and git log --oneline -20.
2. Commit only what belongs to this piece of work. Stage by explicit path (git add <paths>), \
never `git add -A` or `git add .`: leave unrelated modified files and unrelated untracked files \
alone. Never commit secrets or credentials (.env, tokens, keys), nor build output the repository \
does not already track.
3. Run the repository's check first when it has a quick one (AGENTS.md, package.json or the \
Makefile say which). If it fails because of these changes, stop and report: do not commit or push.
4. Stay on the current branch. Write one commit that says what changed and why. Its message \
follows this repository's existing commits in language and style (an English history gets an \
English message, a 中文 history gets 中文; the same kind of prefix and length) and never mixes \
languages.
5. Push the current branch only (git push -u <remote> <branch> when it has no upstream). \
Never force-push, never push tags or other branches.
6. Nothing to commit: say so. Push anyway only if the branch is ahead of its remote.

Report briefly, in the language the user writes in: the branch, the commit (hash and message) \
and the remote it went to, or why you stopped.";

/// `/commit-pr`: the same commit, then a pull request. Unlike /commit-push it may create a
/// branch, because a pull request cannot come from the default branch.
pub const COMMIT_PR_PROMPT: &str = "\
(/commit-pr) Commit the current changes and open a pull request for this branch.

1. Look first: git status, git diff (staged and unstaged), git log --oneline -20, and the \
repository's existing branches and pull requests for their naming style.
2. Commit only what belongs to this piece of work. Stage by explicit path (git add <paths>), \
never `git add -A` or `git add .`: leave unrelated modified files and unrelated untracked files \
alone. Never commit secrets or credentials (.env, tokens, keys), nor build output the repository \
does not already track.
3. Run the repository's check first when it has a quick one (AGENTS.md, package.json or the \
Makefile say which). If it fails because of these changes, stop and report: do not commit, push \
or open anything.
4. A pull request needs its own branch. On the repository's default branch (main, master, …), \
create one with a descriptive name in the style of the existing branches and switch to it \
before committing; on any other branch, stay there. The commit message follows this \
repository's existing commits in language and style (an English history gets an English message, \
a 中文 history gets 中文) and never mixes languages.
5. Push the branch (git push -u <remote> <branch> when it has no upstream). Never force-push, \
never push tags or other branches.
6. Open the pull request with `gh pr create`: a title in the repository's style and a body with \
what changed, why, and how it was tested, following the repository's PR template when it has \
one (.github/PULL_REQUEST_TEMPLATE*), written in the same language as the commits. Link an \
issue when the work clearly belongs to one. `gh` missing or not logged in: install nothing; \
report the link the remote printed for creating the pull request and stop.
7. Nothing to commit and no commits ahead of the default branch: say so and stop.

Report briefly, in the language the user writes in: the branch, the commit and the pull request \
URL, or why you stopped. Never merge the pull request yourself.";


/// Local UTC offset in seconds, from libc's mktime/localtime (no hardcoded timezone).
fn local_utc_offset_seconds() -> i64 {
    #[cfg(unix)]
    {
        // std does not expose tm_gmtoff; use the TIMERSUB-free trick: compare mktime on
        // a UTC-normalized struct. Cheap fallback: read TZ offset via `date +%z` once.
        use std::sync::OnceLock;
        static OFFSET: OnceLock<i64> = OnceLock::new();
        *OFFSET.get_or_init(|| {
            let out = std::process::Command::new("date")
                .arg("+%z")
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .and_then(|s| parse_utc_offset(s.trim()))
                .unwrap_or(0);
            out
        })
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Query the current macOS input source via macism (no args = print current).
/// None when macism is missing — the whole feature then stays off.
fn ime_current() -> Option<String> {
    let out = std::process::Command::new("macism").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if id.is_empty() { None } else { Some(id) }
}

fn ime_switch(id: &str) -> bool {
    std::process::Command::new("macism")
        .arg(id)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Only keyboard layouts named ABC (any variant) count as "no switch needed".
fn ime_should_switch(current: &str) -> bool {
    !(current == "com.apple.keylayout.ABC" || current == "com.apple.keylayout.US")
}

/// Scan a rendered chat line for the first token that looks like and IS an existing
/// local file or directory (absolute, ./…, ../… or ~/…). Clicking a file opens
/// the read-only viewer; a directory opens the system file manager.
/// Position-aware: the click must land on the path's own characters — a path
/// elsewhere in the line is not a click target.
fn find_existing_file_path(text: &str, click_idx: usize) -> Option<String> {
    let bytes: Vec<char> = text.chars().collect();
    let starts: Vec<&[char]> = vec![&['/'], &['.', '/'], &['.', '.', '/'], &['~', '/']];
    let mut best: Option<String> = None;
    let mut i = 0;
    while i < bytes.len() {
        let matched = starts.iter().any(|s| bytes[i..].starts_with(s));
        if !matched {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < bytes.len() && (bytes[j].is_alphanumeric() || "._-/@+=~".contains(bytes[j])) {
            j += 1;
        }
        if click_idx >= i && click_idx < j {
            let raw: String = bytes[i..j].iter().collect();
            let cleaned = raw
                .trim_end_matches(|c: char| ".,;:!?\"')]".contains(c))
                .to_string();
            let no_line = cleaned.trim_end_matches(|c: char| c.is_ascii_digit() || c == ':');
            let expanded = if let Some(rest) = no_line.strip_prefix('~') {
                let home = std::env::var("HOME").unwrap_or_default();
                format!("{home}{rest}")
            } else {
                no_line.to_string()
            };
            if expanded.len() > 1
                && (std::path::Path::new(&expanded).is_file()
                    || std::path::Path::new(&expanded).is_dir())
            {
                best = Some(expanded);
            }
        }
        i = j.max(i + 1);
    }
    best
}

/// One rendered terminal row of the chat: which source line, which char range, what text.
/// Owning the wrap (instead of ratatui's Paragraph) is what makes character-exact
/// mouse selection possible.
/// A clickable action in the /queue dialog.
#[derive(Clone, Copy, Debug, PartialEq)]
enum QueueAction {
    Supplement(usize), // merge this queued item into the RUNNING turn
    Remove(usize),     // drop this queued item
    SupplementAll,     // merge every queued item into the running turn
    ClearAll,          // drop every queued item
}

/// Shortcut tips rotated on the input rule while a turn runs (20s per tip).
const TIPS: &[&str] = &[
    "ctrl+v 粘贴剪贴板图片，自动变成 [图片] chip",
    "alt+enter 把补充文字并入当前进行中的回合",
    "忙碌时回车自动排队 · /queue 管理队列",
    "/todo <内容> 记待办 · 不带内容打开列表（❓ 和 ?suggest 也收在这里）",
    "选项面板按 tab 改写自答 · esc 只收起面板",
    "ctrl+o 展开最近工具输出 · /focus 安静模式",
    "ctrl+r 历史提示 · ctrl+p 命令面板 · ctrl+e 编辑草稿",
    "alt+↑/alt+↓ 在历史提示之间逐条跳转",
    "/rewind 时间轴回滚代码+对话 · /export 导出会话",
];

/// Where a new open note goes: after the existing open notes, before done notes and the rest.
fn note_slot(todos: &[TodoItem]) -> usize {
    todos
        .iter()
        .position(|t| !matches!(&t.kind, TodoKind::Note { done: false, .. }))
        .unwrap_or(todos.len())
}

/// One line for an effort level in the /effort picker. The names are the API's and differ per
/// model (`low high max` on GLM-5.4, `low medium high xhigh max` on Claude Opus 5.5, …).
fn effort_note(level: &str) -> &'static str {
    match level {
        "none" => "不思考",
        "minimal" => "几乎不思考",
        "low" => "轻度推理",
        "medium" => "中等推理",
        "high" => "增强推理",
        "xhigh" => "更强推理",
        "max" => "最深推理",
        "ultra" => "极致推理",
        _ => "",
    }
}

/// Something the model left for the user to act on: a blocking choice (?choice) the
/// user hasn't answered, or a side suggestion (?suggest). Per-item: it leaves the
/// list only when the user explicitly handles it.
#[derive(Clone, Debug, PartialEq)]
enum TodoKind {
    Choice {
        question: String,
        preview: Vec<String>,
        options: Vec<ChoiceOption>,
    },
    Suggest {
        text: String,
    },
    /// heuristic: the reply ended on a plain question with no marker — answered by
    /// whatever the user sends next, so it auto-retires on any prompt
    Question {
        text: String,
    },
    /// the user's own note from `/todo <text>`; stays until done and dropped
    Note { text: String, done: bool },
}

impl TodoItem {
    fn note(text: impl Into<String>) -> Self {
        Self { kind: TodoKind::Note { text: text.into(), done: false } }
    }
}

/// One ?choice option: the scannable label plus an optional muted annotation
/// (the indented line under it) explaining consequences.
#[derive(Clone, Debug, PartialEq)]
struct ChoiceOption {
    label: String,
    note: Option<String>,
}

#[derive(Clone, Debug)]
struct TodoItem {
    kind: TodoKind,
}

/// A click target in the full-access acknowledgement dialog.
#[derive(Clone, Copy, Debug, PartialEq)]
enum FullAccessAction {
    Toggle, // the "我已了解风险" row
    Enable, // 启用 (treats an unchecked box as checked only on a second, explicit click)
}

/// A click target in the /todo dialog.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TodoAction {
    Expand(usize), // re-open this unanswered choice's option panel
    Run(usize),    // act on a suggestion: send "执行这条建议" as the next prompt
    Reply(usize),  // preload a heuristic question into the input box for answering
    Drop(usize),   // dismiss without acting
    Done(usize),   // a note: tick / untick
}

/// How bad a language-server problem is; the order is the list's (errors first).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Error,
    Warning,
    Info,
    Hint,
}

impl Severity {
    fn parse(v: Value) -> Self {
        match v.as_str() {
            Some("warning") => Severity::Warning,
            Some("info") => Severity::Info,
            Some("hint") => Severity::Hint,
            _ => Severity::Error, // LSP leaves the reading of a missing severity to the client
        }
    }

    /// The marker in the list and the status bar (ASCII mode: `!` `w` `i` `-`).
    fn icon(self, ascii: bool) -> &'static str {
        match (self, ascii) {
            (Severity::Error, false) => "✗",
            (Severity::Error, true) => "!",
            (Severity::Warning, false) => "⚠",
            (Severity::Warning, true) => "w",
            (Severity::Info, false) => "ℹ",
            (Severity::Info, true) => "i",
            (Severity::Hint, false) => "·",
            (Severity::Hint, true) => "-",
        }
    }

    fn tone(self) -> Tone {
        match self {
            Severity::Error => Tone::Error,
            Severity::Warning => Tone::Warning,
            Severity::Info | Severity::Hint => Tone::Muted,
        }
    }
}

/// One problem a language server reports for a file (`diagnostics` frame; positions are 1-based).
#[derive(Clone, Debug, PartialEq)]
struct Problem {
    line: u32,
    col: u16,
    severity: Severity,
    message: String,
    code: Option<String>,
    source: Option<String>,
}

impl Problem {
    fn from_json(v: &Value) -> Option<Problem> {
        let message = v["message"].as_str()?.trim();
        if message.is_empty() {
            return None;
        }
        Some(Problem {
            line: v["line"].as_u64().unwrap_or(1).max(1) as u32,
            col: v["col"].as_u64().unwrap_or(1).max(1) as u16,
            severity: Severity::parse(v["severity"].clone()),
            message: message.to_string(),
            code: v["code"].as_str().map(str::to_string),
            source: v["source"].as_str().map(str::to_string),
        })
    }
}

/// What a language server last reported for one file tango edited: the newest list
/// replaces the older one, and an empty list drops the file (`/problems` lists them).
#[derive(Clone, Debug, PartialEq)]
struct FileProblems {
    file: String,
    items: Vec<Problem>,
}

/// The model asked the user to pick; the panel renders above the input box.
#[derive(Clone)]
struct Choices {
    question: String,
    /// bar-indented preview block ("| " lines) — commands/paths the ask is about
    preview: Vec<String>,
    options: Vec<ChoiceOption>,
    sel: usize, // highlighted row (↑↓ + enter, or click)
    /// set when this panel offers 接受/忽略 for a ?suggest (its text), not a real ?choice
    suggest: Option<String>,
}

#[derive(Clone, Debug)]
struct ChatRow {
    line: usize,
    cs: usize, // char start (into the source line text)
    ce: usize, // char end, exclusive
    text: String,
    /// display column this row starts at: 0 for full-width rows, the bubble's
    /// block indent for user rows
    x: u16,
    /// user rows render as a right-positioned bg bubble (interior left-aligned)
    user: bool,
    /// thinking rows carry a left "▍ " gutter whose color scrolls while busy
    think: bool,
    /// rounded top/bottom frame row of a user bubble — styled, never selectable
    border: bool,
    /// one row of the wordmark: drawn from the gradient, never wrapped or selected
    wordmark: bool,
}

/// Right-position the rows of user lines into one bg bubble per line: all rows of
/// a line share a block indent computed from the widest row (+ interior padding),
/// so wrapped lines align inside a rectangle hugging the right margin.
/// Blank columns kept between a user bubble's right │ and the terminal edge.
const BUBBLE_MARGIN: usize = 1;

/// The thinking bar's color wave: dim → bright cyan → dim, one slot per row.
/// A phase of (frame − row) makes the bright band flow down the block.
static THINK_EPOCH: std::sync::LazyLock<std::time::Instant> =
    std::sync::LazyLock::new(std::time::Instant::now);
const THINK_WAVE: [Color; 8] = [
    Color::Rgb(18, 48, 60),
    Color::Rgb(24, 84, 104),
    Color::Rgb(32, 130, 160),
    Color::Rgb(60, 190, 220),
    Color::Rgb(120, 225, 240),
    Color::Rgb(60, 190, 220),
    Color::Rgb(32, 130, 160),
    Color::Rgb(24, 84, 104),
];
/// The same wave in xterm 256-color cube stand-ins (the rule sweep's palette).
const THINK_WAVE_256: [Color; 8] = [
    Color::Indexed(23),
    Color::Indexed(24),
    Color::Indexed(31),
    Color::Indexed(38),
    Color::Indexed(123),
    Color::Indexed(38),
    Color::Indexed(31),
    Color::Indexed(24),
];
const THINK_FRAME_MS: u128 = 120;

/// The thinking wave this terminal can show: 24-bit, or the 256-color cube.
fn think_wave() -> &'static [Color; 8] {
    if supports_truecolor() { &THINK_WAVE } else { &THINK_WAVE_256 }
}

/// Color of the thinking bar cell at global row `g` on animation frame `frame`
/// (~120ms apart). `animate` false (idle, or animations off) → flat dim gray.
fn thinking_bar_color(frame: u128, local_row: usize, animate: bool) -> Color {
    if !animate {
        return Color::DarkGray;
    }
    let wave = think_wave();
    let phase = (frame + wave.len() as u128).wrapping_sub(local_row as u128) % wave.len() as u128;
    wave[phase as usize]
}

/// How quickly output is arriving, as the rule sweep's speed in cells per
/// animation frame: streaming (output in the last 0.5s) runs, waiting on a
/// tool or the network (under 10s) drifts, past 10s of silence it stops.
fn sweep_speed(since_output: std::time::Duration) -> u64 {
    match since_output.as_millis() {
        0..500 => 2,
        500..10_000 => 1,
        _ => 0,
    }
}

/// Color of rule cell `x` when the sweep head is at `head` (wrapping over
/// `width` cells): a 7-cell cyan band with a bright core over the dim rule.
/// None = the cell keeps the plain rule style.
fn sweep_color(x: usize, head: u64, width: usize, stalled: bool) -> Option<Color> {
    if width == 0 || stalled {
        return None;
    }
    let head = (head % width as u64) as usize;
    // the band moves right: cells to the head's left are its fading trail
    let behind = (head + width - x % width) % width;
    const BAND: [Color; 7] = [
        Color::Rgb(120, 225, 240),
        Color::Rgb(90, 205, 230),
        Color::Rgb(60, 180, 215),
        Color::Rgb(45, 150, 185),
        Color::Rgb(35, 120, 150),
        Color::Rgb(28, 92, 116),
        Color::Rgb(22, 70, 88),
    ];
    // xterm 256-color cube stand-ins, bright cyan down to dark teal
    const BAND_256: [u8; 7] = [123, 87, 45, 38, 31, 24, 23];
    if supports_truecolor() {
        BAND.get(behind).copied()
    } else {
        BAND_256.get(behind).map(|&i| Color::Indexed(i))
    }
}

/// The running gear's color cycle: same cyan family as the thinking wave, one
/// step every 2 frames (~240ms). Without truecolor the same four steps come
/// from the xterm 256-color cube.
fn gear_color(frame: u128, animate: bool) -> Color {
    if !animate {
        return Color::DarkGray;
    }
    const GEAR_WAVE: [Color; 4] = [
        Color::Rgb(120, 225, 240),
        Color::Rgb(60, 190, 220),
        Color::Rgb(32, 130, 160),
        Color::Rgb(24, 84, 104),
    ];
    // the same steps in the 256-color cube (the thinking wave's stand-ins)
    const GEAR_WAVE_256: [Color; 4] =
        [Color::Indexed(123), Color::Indexed(38), Color::Indexed(31), Color::Indexed(24)];
    let wave = if supports_truecolor() { &GEAR_WAVE } else { &GEAR_WAVE_256 };
    wave[(frame / 2 % 4) as usize]
}

/// 24-bit color support: COLORTERM=truecolor/24bit or a *-direct TERM. Cached:
/// the environment does not change while we run.
fn supports_truecolor() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        let colorterm = std::env::var("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        let term = std::env::var("TERM").unwrap_or_default();
        colorterm.contains("truecolor") || colorterm.contains("24bit") || term.ends_with("direct")
    })
}

fn layout_user_bubbles(rows: &mut Vec<ChatRow>, is_user: &[bool], width: usize, bubble: bool) {
    let mut i = 0;
    while i < rows.len() {
        let line = rows[i].line;
        let user = is_user.get(line).copied().unwrap_or(false);
        let end = rows[i..]
            .iter()
            .position(|r| r.line != line)
            .map(|n| i + n)
            .unwrap_or(rows.len());
        if user && rows[i..end].iter().any(|r| !r.text.is_empty()) {
            for r in &mut rows[i..end] {
                r.user = true;
            }
            if bubble {
                let maxw = rows[i..end]
                    .iter()
                    .map(|r| display_width(&r.text))
                    .max()
                    .unwrap_or(0);
                // rounded frame: │+space each side, hugging the right edge. The renderer's
                // interior is width - x - 4, so x leaves exactly maxw columns of text plus
                // the frame's own 4 — otherwise a full row's │ lands one column off the ╮.
                let x = width.saturating_sub(maxw + 5 + BUBBLE_MARGIN) as u16;
                for r in &mut rows[i..end] {
                    r.x = x;
                }
                // wrap the text rows in top/bottom border rows of the same block
                let top = ChatRow {
                    think: false,
                    line: rows[i].line,
                    cs: rows[i].cs,
                    ce: rows[i].cs,
                    text: String::new(),
                    x,
                    user: true,
                    border: true,
                    wordmark: false,
                };
                let bottom = ChatRow {
                    think: false,
                    cs: rows[end - 1].ce,
                    ce: rows[end - 1].ce,
                    ..top.clone()
                };
                rows.insert(end, bottom);
                rows.insert(i, top);
                // the framed group now occupies [i, end+2): both inserts shifted it
                // past `end`, so skipping only `end` would re-frame the tail
                i = end + 2;
                continue;
            }
        }
        i = end;
    }
}

/// The char index a screen column lands on within a row, walking display widths
/// from the row's own start (bubble rows carry an indent + one interior pad).
/// Columns left of the row clamp to 0; past the end, to the char count.
fn row_char_at(row: &ChatRow, column: u16) -> usize {
    let start = row.x as usize
        + 2 * usize::from(row.user)   // │ + interior pad
        + 2 * usize::from(row.think); // ▍ + gutter pad
    let mut col = (column as usize).saturating_sub(start);
    let mut idx = 0usize;
    for ch in row.text.chars() {
        let w = display_width(&ch.to_string());
        if col < w {
            break;
        }
        col -= w;
        idx += 1;
    }
    idx
}

/// Wrap with per-line widths: user lines bound for a bordered bubble wrap
/// narrower (2 border/pad columns each side); everything else uses the full width.
fn wrap_chat_rows_mixed(
    texts: &[String],
    width: usize,
    is_user: &[bool],
    think: &[bool],
    wordmark: &[bool],
    bubble: bool,
) -> Vec<ChatRow> {
    let mut rows = Vec::new();
    let w = width.max(1);
    for (line, text) in texts.iter().enumerate() {
        let user = is_user.get(line).copied().unwrap_or(false);
        // thinking lines wrap 2 columns narrower via the flagged wrapper (the "▍ "
        // gutter); bubble-bound user lines reserve the frame columns
        let lw = if user && bubble {
            w.saturating_sub(4 + BUBBLE_MARGIN).max(1)
        } else {
            w
        };
        let is_think = think.get(line).copied().unwrap_or(false);
        let is_wordmark = wordmark.get(line).copied().unwrap_or(false);
        let mut part = wrap_chat_rows_flagged(&[text.clone()], lw, &[is_think]);
        for r in &mut part {
            // wrap_chat_rows indexes within the single-line slice — restore the
            // real source-line index so grouping/mapping stay correct
            r.line = line;
            r.user = user;
            r.wordmark = is_wordmark;
        }
        rows.extend(part);
    }
    rows
}

/// Word wrap with char-index tracking; long words (URLs, CJK runs) hard-split at width.
fn wrap_chat_rows(texts: &[String], width: usize) -> Vec<ChatRow> {
    wrap_chat_rows_flagged(texts, width, &[])
}

/// Wrap with per-line flags: a thinking line reserves its 2-column "▍ " gutter,
/// so it wraps 2 columns narrower to keep the text inside the chat area.
fn wrap_chat_rows_flagged(texts: &[String], width: usize, think: &[bool]) -> Vec<ChatRow> {
    let mut rows = Vec::new();
    for (line, text) in texts.iter().enumerate() {
        let think = think.get(line).copied().unwrap_or(false);
        let w = width.max(1).saturating_sub(if think { 2 } else { 0 }).max(1);
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            rows.push(ChatRow {
                line,
                cs: 0,
                ce: 0,
                text: String::new(),
                x: 0,
                user: false,
                think,
                border: false,
                wordmark: false,
            });
            continue;
        }
        // a newline is a hard row break: wrap each segment on its own, with char
        // offsets still indexing the whole line (the '\n' itself belongs to no row)
        let mut seg_start = 0usize;
        while seg_start <= chars.len() {
            let seg_end = chars[seg_start..]
                .iter()
                .position(|&c| c == '\n')
                .map(|n| seg_start + n)
                .unwrap_or(chars.len());
            wrap_segment(&mut rows, line, &chars, seg_start, seg_end, w, think);
            seg_start = seg_end + 1;
        }
    }
    rows
}

/// Wrap chars[start..end] (no newlines inside) into rows of at most `w` columns.
fn wrap_segment(
    rows: &mut Vec<ChatRow>,
    line: usize,
    chars: &[char],
    start: usize,
    end: usize,
    w: usize,
    think: bool,
) {
    if start == end {
        rows.push(ChatRow {
            line,
            cs: start,
            ce: start,
            text: String::new(),
            x: 0,
            user: false,
            think,
            border: false,
            wordmark: false,
        });
        return;
    }
    let chars = &chars[..end];
    {
        // split into tokens: runs of spaces stay attached to the preceding word end
        let mut cs = start;
        let mut idx = start;
        while idx < chars.len() {
            // find the next break candidate: end of a word + space run
            let mut end = idx;
            let mut width_so_far = 0usize;
            let mut last_space: Option<usize> = None;
            while end < chars.len() {
                let cw = display_width(&chars[end].to_string());
                if width_so_far + cw > w {
                    break;
                }
                width_so_far += cw;
                if chars[end] == ' ' {
                    last_space = Some(end);
                }
                end += 1;
            }
            let (row_end, next) = if end >= chars.len() {
                (end, end)
            } else if let Some(sp) = last_space.filter(|&s| {
                // a word that can't fit a whole row anyway (CJK run, URL) is hard-split
                // right here instead — otherwise a lone "❯ " is left on its own row
                let word_w: usize = chars[s + 1..]
                    .iter()
                    .take_while(|&&c| c != ' ')
                    .map(|c| display_width(&c.to_string()))
                    .sum();
                s > idx && word_w <= w
            }) {
                // break after the last space inside the window (word wrap)
                (sp + 1, sp + 1)
            } else {
                (end, end) // hard split: single long word
            };
            let piece: String = chars[cs..row_end].iter().collect();
            rows.push(ChatRow {
                line,
                cs,
                ce: row_end,
                text: piece,
                x: 0,
                user: false,
                think,
                border: false,
                wordmark: false,
            });
            cs = next;
            idx = next;
            // skip leading spaces of the next row (trim leading-only when the row starts with spaces)
            while idx < chars.len()
                && chars[idx] == ' '
                && display_width(&chars[idx].to_string()) == 1
            {
                // keep at most one leading space for continuity
                break;
            }
        }
    }
}

fn parse_utc_offset(s: &str) -> Option<i64> {
    // "+0800" / "-0530" — length checked BEFORE split_at (empty used to panic)
    if s.len() < 5 {
        return None;
    }
    let (sign, rest) = s.split_at(1);
    let hours: i64 = rest.get(0..2)?.parse().ok()?;
    let mins: i64 = rest.get(2..4)?.parse().ok()?;
    let mag = hours * 3600 + mins * 60;
    Some(if sign == "-" { -mag } else { mag })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum EditMode {
    Insert,
    Normal,
}

pub struct App {
    theme: Theme,
    /// transient confirmation ("已复制 …") drawn over the chat's top-right corner, never a chat line
    toast: Option<(String, std::time::Instant)>,
    /// argument completion candidates, fetched quietly on hello (models) and after each
    /// turn (sessions: new ones appear while this one runs)
    arg_models: Vec<String>,
    /// (id, label, current)
    arg_sessions: Vec<(String, String, bool)>,
    mouse: bool,
    pub mouse_capture: Option<bool>,
    keys: Keymap,
    vim: bool,
    edit_mode: EditMode,
    vim_command: Option<String>,
    nvim: Option<Option<Neovim>>,
    nvim_preference: String,
    pending_viewer: Option<Viewer>,
    edited: Option<(Viewer, String)>,
    pub nvim_request: Option<NvimRequest>,
    lines: Vec<ChatLine>,
    /// assistant text arriving right now; committed as chat lines by the `message` frame
    stream: String,
    thinking: String,
    pub input: Vec<char>,
    pub cursor: usize,
    pastes: Vec<PastedSpan>,
    next_paste: usize,
    /// every image attached this session (chat chips reopen them by id); never cleared on submit
    images: Vec<ImageAttachment>,
    next_image: usize,
    prompt_history: Vec<String>,
    stashed_draft: Option<InputDraft>,
    /// up/down history navigation: index into prompt_history of the shown
    /// entry, with the unsent draft stashed aside until sent or left
    history_nav: Option<usize>,
    history_draft: Option<InputDraft>,
    popup: Option<Popup>,
    picker: Option<Picker>,
    viewer: Option<Viewer>,
    chat_area: Rect,
    /// input box rect, captured each draw so chip clicks map back to image ids
    input_area: Rect,
    picker_area: Rect,
    /// visible picker window rows → item index (None = group header row)
    picker_rows: Vec<Option<usize>>,
    popup_area: Rect,
    jump_area: Rect,
    /// scrolled up: dim top row naming the latest prompt above the view; click → (area, scroll_back to set)
    prompt_header: Option<(Rect, usize)>,
    /// 0 = pinned to the newest line (follow mode); increases when scrolling back
    scroll_back: usize,
    last_visual_total: usize,
    unread_rows: usize,
    focus: bool,
    pub busy: bool,
    /// ids of tool calls that started but have not produced a result yet —
    /// their ⚙ lines keep the gear color cycling
    running_tools: std::collections::HashSet<String>,
    /// thinking-bar animation frame (~120ms steps); only advances while busy
    think_frame: u128,
    prompt_id: u64,
    btw_id: Option<u64>,
    btw_title: Option<String>,
    vim_override: Option<bool>,
    /// full texts of recent assistant replies, newest last (for /copy)
    assistant_replies: Vec<String>,
    /// show [HH:MM] prefixes on chat lines (/timestamps)
    timestamps: bool,
    /// custom status bar template with {model} {mode} {cost} {ctx} {pct} {session} {tokens}
    statusline: Option<String>,
    /// Enter inserts a newline instead of sending (/multiline)
    multiline_enter: bool,
    /// the terminal is tall enough (>= 20 rows) to keep the wordmark in the chat
    chat_roomy: bool,
    /// the full-access (auto) acknowledgement dialog: the "我已了解风险" checkbox state
    full_access: Option<bool>,
    /// where that dialog's checkbox row and 启用 button were drawn (click targets)
    full_access_buttons: Vec<(Rect, FullAccessAction)>,
    /// serve-side working directory (/cd moves it); @file resolves against it
    serve_cwd: Option<std::path::PathBuf>,
    /// `git ls-files` of serve_cwd for fuzzy @file search: (dir, built at, paths); rebuilt after 30 s
    file_index: Option<(std::path::PathBuf, std::time::Instant, Vec<String>)>,
    /// rendered chat rows (self-managed wrap), captured each draw for mouse mapping
    chat_rows: Vec<ChatRow>,
    chat_scroll_offset: usize,
    chat_inner_w: u16,
    /// selection anchor/end as (terminal row, column) pairs — char-exact
    sel_anchor_pos: Option<(u16, u16)>,
    sel_end_pos: Option<(u16, u16)>,
    /// input-box mouse selection as (anchor, end) display char indices; kept
    /// highlighted after release until the next key or click
    input_sel: Option<(usize, usize)>,
    input_dragging: bool,
    /// previous left press in the input box (time, row, column) — double-click detection
    input_last_click: Option<(std::time::Instant, u16, u16)>,
    /// previous plain click in the chat area (time, row): a second click on the
    /// same row within 400 ms is a double-click — the only click that opens
    /// viewers/menus/toggles, so a stray single click never launches anything
    chat_last_click: Option<(std::time::Instant, u16)>,
    /// the user prompt currently being worked on — pinned in the status bar so
    /// switching between many agents never loses track of "which ask is this"
    current_prompt: Option<String>,
    /// prompts entered while a turn runs: sent locally in order when it finishes
    local_queue: Vec<QueuedPrompt>,
    /// /queue dialog open (clickable per-item actions)
    queue_dialog: bool,
    /// keyboard-selected row in the /queue dialog
    queue_sel: usize,
    /// the /todo dialog's selected row (keyboard)
    todo_sel: usize,
    /// the IME we switched away from for a single-key dialog (macOS macism)
    ime_restore: Option<String>,
    /// clickable button rects, rebuilt every draw
    queue_buttons: Vec<(Rect, QueueAction)>,
    /// the model's pending ?choice — an option panel above the input box
    choices: Option<Choices>,
    /// Esc collapses the panel; the todo entry (and badge) keep it findable
    choices_collapsed: bool,
    /// clickable option rows of the choices panel (rect → option index), rebuilt every draw
    choice_rows: Vec<(Rect, usize)>,
    /// unanswered choices + side suggestions, handled per-item via /todo
    todos: Vec<TodoItem>,
    /// saved notes were read for this session (see load_notes)
    notes_loaded: bool,
    /// overrides the tango home for todos.json (tests; set before load_notes)
    notes_home: Option<std::path::PathBuf>,
    /// /todo dialog open
    todo_dialog: bool,
    /// clickable button rects of the /todo dialog, rebuilt every draw
    todo_buttons: Vec<(Rect, TodoAction)>,
    /// what language servers last reported per file tango edited, by file name (/problems)
    problems: Vec<FileProblems>,
    /// /problems dialog open
    problems_dialog: bool,
    /// the /problems dialog's selected row over the whole list (keyboard, wheel)
    problems_sel: usize,
    /// clickable rows of the /problems dialog (rect → row), rebuilt every draw
    problems_rows: Vec<(Rect, usize)>,
    /// the status bar's problems badge, rebuilt every draw (a click opens /problems)
    problems_area: Rect,
    /// the history emoji at the left end of the status bar, rebuilt every draw (a click
    /// opens /history)
    history_area: Rect,
    /// `tango -r` with no id (TANGO_PICK_SESSION): open the session picker once serve says hello
    start_picker: bool,
    /// when the running turn started — drives the elapsed readout and tip rotation
    turn_started: Option<std::time::Instant>,
    /// chars received this turn (text + thinking deltas) — the ↓ readout
    turn_chars: u64,
    /// when the running turn last produced anything (a delta or a tool
    /// result) — sets the rule sweep's speed and the stall dim
    last_output: Option<std::time::Instant>,
    /// the rule sweep's head position in cells, advanced per animation frame
    sweep_pos: u64,
    /// the animation frame the sweep was last advanced on
    sweep_frame: u128,
    /// full serve session id (the status bar shows the 8-char prefix; the exit
    /// note needs the whole thing for `tango --resume`)
    session_full: String,
    /// the serve cwd's current git branch (hello frame) — Ctrl+B filters by it
    serve_branch: String,
    /// the current session's display name (/name) — status bar shows it over the id
    session_name: String,
    /// browser-style session history: ids left by a switch (/back pops) and ids /back left
    /// (/forward pops); a fresh switch clears the forward list. Lives for the TUI process
    nav_back: Vec<String>,
    nav_forward: Vec<String>,
    /// the status-bar sess span's rect — clicking copies the full id
    session_id_area: Rect,
    /// the model name (+ provider) in the input block's info row: a click opens
    /// the model picker
    model_area: Rect,
    /// the mode label (`auto`, `plan` …) at the start of that row: a click opens
    /// the mode picker
    mode_area: Rect,
    /// the footer's key hints: a click on `shift+tab mode` opens the mode picker (like a click
    /// on the mode label; the key itself cycles), `ctrl+p commands` the command palette
    footer_mode_area: Rect,
    footer_palette_area: Rect,
    /// every open dialog's bottom-hint keys as buttons, drawn this frame: a click on
    /// one is handled as that key press, so every dialog key is also clickable
    hint_buttons: Vec<HintButton>,
    /// the topmost modal dialog drawn this frame (permission, picker, viewer, /queue, /todo,
    /// the message menu): everything outside it is dimmed behind it
    modal_rect: Option<Rect>,
    /// user-message style: true = right-positioned bubble (default),
    /// false = Claude Code style (same theme, left, full-width bg) — /bubble
    user_bubble: bool,
    /// motion on (default): rule sweep, thinking wave, gear, splash
    /// twinkle. Off keeps the same text with static colors — /animations
    pub animations: bool,
    /// ASCII mode: emoji and the robot icon become plain ASCII (`*`, `?`, …) for
    /// terminals without an emoji font — /ascii, config `ascii`
    pub ascii: bool,
    /// scroll offset of the permission dialog's command preview (↑↓/wheel)
    perm_scroll: usize,
    /// the preview's wrap width, captured each draw so key/wheel scrolling
    /// computes the same row count the dialog rendered
    perm_wrap_w: usize,
    /// alt+e armed: the destructive confirm (interrupt + code rollback) waits
    /// for a second press
    editlast_armed: bool,
    /// first Ctrl+C on an empty, idle input: a second one within EXIT_WINDOW quits
    exit_armed: Option<std::time::Instant>,
    /// the prompt menu (click a user bubble): anchored line index
    prompt_menu: Option<usize>,
    /// clickable rows of the prompt menu, rebuilt every draw
    prompt_menu_buttons: Vec<(Rect, u8)>,
    /// the first line of the turn targeted by 编辑重发 (timeline matching)
    edit_target_head: Option<String>,
    /// /queue auto append: busy-Enter supplements the running turn instead of
    /// queueing a new prompt
    queue_auto_append: bool,
    /// plan mode badge (/plan)
    plan_mode: bool,
    /// active /goal text (badge)
    goal_text: Option<String>,
    /// tool detail level: 0 compact / 1 default / 2 full (/density)
    density: u8,
    pending: Vec<(u64, Pending)>,
    permission: Option<Permission>,
    model: String,
    /// reasoning effort level in use ("off" = the endpoint's default), and the levels /effort may
    /// take for the current model (they differ per model; serve sends both with hello and /model)
    effort: String,
    efforts: Vec<String>,
    /// the level chosen for a model, kept when the model is switched away from and back within
    /// this run (model id → level): the model picker shows it beside the model and re-applies it
    effort_memory: std::collections::HashMap<String, String>,
    mode: String,
    session: String,
    cost: f64,
    context_tokens: u64,
    context_window: u64,
    pub quit: bool,
    /// /restart: after quitting, main re-execs the TUI resuming (session id, permission mode)
    pub restart: Option<(String, String)>,
    /// serve respawn request for the main loop: Some(Some(id)) resumes, Some(None) starts fresh
    pub respawn: Option<Option<String>>,
    /// set while a /handoff or /takeover session list is in flight: picker purpose
    session_pick: Option<PickerKind>,
    pub err: Option<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            theme: Theme::dark(),
            toast: None,
            arg_models: Vec::new(),
            arg_sessions: Vec::new(),
            mouse: true,
            mouse_capture: None,
            keys: Keymap::from_json(None),
            vim: false,
            edit_mode: EditMode::Insert,
            vim_command: None,
            nvim: None,
            nvim_preference: "ask".into(),
            pending_viewer: None,
            edited: None,
            nvim_request: None,
            lines: Vec::new(),
            stream: String::new(),
            thinking: String::new(),
            input: Vec::new(),
            cursor: 0,
            pastes: Vec::new(),
            next_paste: 1,
            images: Vec::new(),
            next_image: 1,
            prompt_history: Vec::new(),
            stashed_draft: None,
            history_nav: None,
            history_draft: None,
            popup: None,
            picker: None,
            viewer: None,
            chat_area: Rect::default(),
            input_area: Rect::default(),
            picker_area: Rect::default(),
            picker_rows: Vec::new(),
            popup_area: Rect::default(),
            jump_area: Rect::default(),
            prompt_header: None,
            scroll_back: 0,
            last_visual_total: 0,
            unread_rows: 0,
            focus: false,
            busy: false,
            running_tools: std::collections::HashSet::new(),
            think_frame: 0,
            chat_roomy: false,
            full_access: None,
            full_access_buttons: Vec::new(),
            prompt_id: 0,
            btw_id: None,
            btw_title: None,
            vim_override: None,
            assistant_replies: Vec::new(),
            timestamps: false,
            density: 1,
            statusline: None,
            multiline_enter: false,
            serve_cwd: None,
            file_index: None,
            chat_rows: Vec::new(),
            chat_scroll_offset: 0,
            chat_inner_w: 0,
            sel_anchor_pos: None,
            sel_end_pos: None,
            input_sel: None,
            input_dragging: false,
            input_last_click: None,
            chat_last_click: None,
            current_prompt: None,
            local_queue: Vec::new(),
            queue_dialog: false,
            queue_sel: 0,
            todo_sel: 0,
            choices: None,
            choices_collapsed: false,
            choice_rows: Vec::new(),
            todos: Vec::new(),
            notes_loaded: false,
            notes_home: None,
            todo_dialog: false,
            todo_buttons: Vec::new(),
            problems: Vec::new(),
            problems_dialog: false,
            problems_sel: 0,
            problems_rows: Vec::new(),
            problems_area: Rect::default(),
            history_area: Rect::default(),
            start_picker: std::env::var_os("TANGO_PICK_SESSION").is_some(),
            turn_started: None,
            turn_chars: 0,
            last_output: None,
            sweep_pos: 0,
            sweep_frame: 0,
            session_full: String::new(),
            serve_branch: String::new(),
            session_name: String::new(),
            nav_back: Vec::new(),
            nav_forward: Vec::new(),
            session_id_area: Rect::default(),
            model_area: Rect::default(),
            mode_area: Rect::default(),
            footer_mode_area: Rect::default(),
            footer_palette_area: Rect::default(),
            hint_buttons: Vec::new(),
            modal_rect: None,
            user_bubble: true,
            animations: true,
            ascii: false,
            perm_scroll: 0,
            perm_wrap_w: 76,
            editlast_armed: false,
            exit_armed: None,
            prompt_menu: None,
            prompt_menu_buttons: Vec::new(),
            edit_target_head: None,
            queue_auto_append: false,
            ime_restore: None,
            queue_buttons: Vec::new(),
            plan_mode: false,
            goal_text: None,
            pending: Vec::new(),
            permission: None,
            model: "…".into(),
            effort: "off".into(),
            // until serve says what the model takes (hello): the commonest levels
            efforts: ["low", "medium", "high"].iter().map(|s| s.to_string()).collect(),
            effort_memory: std::collections::HashMap::new(),
            mode: String::new(),
            session: String::new(),
            cost: 0.0,
            context_tokens: 0,
            context_window: 0,
            quit: false,
            respawn: None,
            restart: None,
            session_pick: None,
            err: None,
        }
    }

    pub fn mode(&self) -> &str {
        &self.mode
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn mouse_enabled(&self) -> bool {
        self.mouse
    }

    /// What to report to the terminal over OSC 7501 (see status.rs). A permission dialog
    /// opens mid-turn, so it outranks busy; a collapsed ?choice panel leaves a plain input box.
    pub fn program_status(&self) -> crate::status::ProgramStatus {
        use crate::status::ProgramStatus;
        if self.permission.is_some() {
            ProgramStatus::BlockedPermission
        } else if self.busy {
            ProgramStatus::Working
        } else if self.choices.is_some() && !self.choices_collapsed {
            ProgramStatus::BlockedQuestion
        } else {
            ProgramStatus::Idle
        }
    }

    fn context_tone(&self) -> Tone {
        if self.context_window == 0 {
            return Tone::Muted;
        }
        let percent = self.context_tokens.saturating_mul(100) / self.context_window;
        if percent >= 80 {
            Tone::Error
        } else if percent >= 60 {
            Tone::Warning
        } else {
            Tone::Success
        }
    }

    fn token_count(count: u64) -> String {
        if count >= 1000 {
            format!("{:.1}k", count as f64 / 1000.0)
        } else {
            count.to_string()
        }
    }

    /// a user-driven switch (/resume, picker, /new, /fork): remember where we came from
    fn switch_session(&mut self, target: Option<String>) {
        let cur = self.session_full.clone();
        let same = target.as_deref().is_some_and(|t| !t.is_empty() && cur.starts_with(t));
        if !cur.is_empty() && !same {
            self.nav_back.push(cur);
            self.nav_forward.clear();
        }
        self.respawn_into(target);
    }

    /// /back and /forward: move one step through the session history
    fn navigate(&mut self, forward: bool) {
        let (from, to) = if forward {
            (&mut self.nav_forward, &mut self.nav_back)
        } else {
            (&mut self.nav_back, &mut self.nav_forward)
        };
        let Some(id) = from.pop() else {
            let msg = if forward { "没有可前进的会话" } else { "没有可返回的会话" };
            self.push(msg, Tone::Muted);
            return;
        };
        if !self.session_full.is_empty() {
            to.push(self.session_full.clone());
        }
        let arrow = if forward { "→" } else { "←" };
        let short = short_session(&id).to_string();
        self.push(format!("{arrow} switching to session {short} …"), Tone::Muted);
        self.respawn_into(Some(id));
    }

    /// switching context means a new serve child; wipe the old transcript and turn state
    fn respawn_into(&mut self, target: Option<String>) {
        let keep: Vec<ChatLine> = self.lines.drain(..).last().into_iter().collect();
        self.lines = keep; // the "resuming…/new session" notice just pushed
        self.stream.clear();
        self.thinking.clear();
        self.pending.clear();
        self.btw_id = None;
        self.btw_title = None;
        self.permission = None;
        self.ime_restore_prev();
        self.viewer = None;
        self.pending_viewer = None;
        self.edited = None;
        self.nvim_request = None;
        // image chip ids are per-session: stale ids must not reopen another session's files
        self.images.clear();
        self.next_image = 1;
        self.busy = false;
        self.cost = 0.0;
        self.context_tokens = 0;
        self.context_window = 0;
        self.scroll_back = 0;
        self.last_visual_total = 0;
        self.unread_rows = 0;
        self.jump_area = Rect::default();
        // problems belong to the session that edited the files
        self.problems.clear();
        self.problems_sel = 0;
        self.problems_dialog = false;
        self.problems_rows.clear();
        self.respawn = Some(target);
    }

    /// Send /handoff (`HandoffTarget`) or /takeover (`TakeoverSource`) for `session`.
    fn send_transfer(&mut self, purpose: PickerKind, session: &str, note: &str, serve: &mut Serve) {
        let (cmd, pending, msg) = if purpose == PickerKind::HandoffTarget {
            ("handoff", Pending::Handoff, "⇢ 正在生成交接摘要…")
        } else {
            ("takeover", Pending::Takeover, "⇠ 正在读取并总结该会话…")
        };
        let mut extra = vec![("session", Value::String(session.to_string()))];
        if !note.is_empty() {
            extra.push(("note", Value::String(note.to_string())));
        }
        let id = serve.command(cmd, extra);
        self.pending.push((id, pending));
        self.push(msg, Tone::Muted);
    }

    /// Terminal row → global row index (the renderer's own row list).
    fn global_row(&self, term_row: u16, chat_y: u16) -> Option<usize> {
        let g = term_row.saturating_sub(chat_y) as usize + self.chat_scroll_offset;
        (g < self.chat_rows.len()).then_some(g)
    }

    /// Normalized selection as (row_a, col_a, row_b, col_b) in GLOBAL rows, a ≤ b.
    fn selection_range(&self, chat_y: u16) -> Option<(usize, usize, usize, usize)> {
        let (a, b) = (self.sel_anchor_pos?, self.sel_end_pos?);
        let ga = self.global_row(a.0, chat_y)?;
        let gb = self.global_row(b.0, chat_y)?;
        let (ra, ca, rb, cb) = if (ga, a.1) <= (gb, b.1) {
            (ga, a.1, gb, b.1)
        } else {
            (gb, b.1, ga, a.1)
        };
        // screen columns → char indices (bubble rows carry an indent; CJK chars
        // are 2 columns wide — walking widths keeps both exact)
        let ca = self
            .chat_rows
            .get(ra)
            .map(|r| row_char_at(r, ca))
            .unwrap_or(0);
        let cb = self
            .chat_rows
            .get(rb)
            .map(|r| row_char_at(r, cb))
            .unwrap_or(0);
        Some((ra, ca, rb, cb))
    }

    /// The exact selected text: partial rows at both ends, full rows between.
    fn selected_text(&self, chat_y: u16) -> Option<String> {
        let (ra, ca, rb, cb) = self.selection_range(chat_y)?;
        let mut out = String::new();
        let mut emitted = false;
        for g in ra..=rb.min(self.chat_rows.len().saturating_sub(1)) {
            let row = self.chat_rows.get(g)?;
            if row.border || row.wordmark {
                continue; // frame rows and the wordmark are decoration, never copied
            }
            let chars: Vec<char> = row.text.chars().collect();
            let from = if g == ra { ca.min(chars.len()) } else { 0 };
            let to = if g == rb {
                cb.min(chars.len())
            } else {
                chars.len()
            };
            if emitted {
                out.push('\n');
            }
            emitted = true;
            out.push_str(&chars[from..to].iter().collect::<String>());
        }
        (out.trim().len() > 0).then_some(out)
    }

    /// A clipboard copy that never touches the real clipboard under `cargo test`.
    fn copy_to_clipboard(&self, text: &str) -> std::io::Result<()> {
        if cfg!(test) {
            return Ok(());
        }
        crate::clipboard::copy(text)
    }

    /// Left press in the chat area always starts a character-exact selection.
    /// Click targets (image chips, file paths, line-output viewers) are only
    /// acted on at release, so a drag begun anywhere can select text.
    fn chat_press(&mut self, row: u16, column: u16) {
        self.sel_anchor_pos = Some((row, column));
        self.sel_end_pos = Some((row, column));
    }

    /// Map a chat-area position to (source line index, full line text, char index of
    /// the click within that line). Rebuilding the line from the wrap rows is what
    /// keeps the index in sync with what is on screen.
    fn chat_line_hit(&self, row: u16, column: u16, chat_y: u16) -> Option<(usize, String, usize)> {
        let g = self.global_row(row, chat_y)?;
        // the wordmark is decoration: a click or double-click on it does nothing
        if self.chat_rows.get(g).is_some_and(|r| r.wordmark) {
            return None;
        }
        let li = self.chat_rows.get(g).map(|r| r.line)?;
        let mut full = String::new();
        let mut click_idx = 0usize;
        for (i, r) in self.chat_rows.iter().enumerate() {
            if r.line != li {
                continue;
            }
            if i < g {
                click_idx += r.text.chars().count();
            } else if i == g {
                click_idx += row_char_at(r, column);
            }
            full.push_str(&r.text);
        }
        Some((li, full, click_idx))
    }

    /// A double-click (press + release + press + release, no drag) in the chat
    /// area: an image chip under the cursor opens the image; a file path under
    /// the cursor opens the read-only viewer; a line with collapsed output opens
    /// the transcript viewer. Single clicks are handled in chat_release and
    /// never reach this.
    fn chat_click(&mut self, row: u16, column: u16, chat_y: u16) {
        if let Some((li, full, click_idx)) = self.chat_line_hit(row, column, chat_y) {
            if let Some(path) = self.image_chip_at(&full, click_idx) {
                self.open_image(&path);
                return;
            }
            if let Some(path) = find_existing_file_path(&full, click_idx) {
                self.open_file_viewer(&path);
                return;
            }
            // a plain user bubble opens the prompt menu; collapsed pastes (lines
            // with output) keep their expand-to-viewer click below
            let source = self.chat_lines().get(li).map(|l| l.source_index);
            // clicking a thinking block expands / collapses it in place
            if let Some(line) = source
                .and_then(|i| self.lines.get_mut(i))
                .filter(|l| l.tone == Tone::Thinking)
            {
                line.expanded = !line.expanded;
                return;
            }
            if source
                .and_then(|i| self.lines.get(i))
                .is_some_and(|l| l.tone == Tone::User && l.output.is_none())
            {
                self.prompt_menu = source;
                return;
            }
            // only a line that actually carries collapsed output opens the transcript
            // viewer — and only the line the hit above resolved to. (An earlier version
            // re-walked the rows with a naive wrap that ignored bubble borders, drifted
            // several lines below the click, and opened whatever output-bearing line
            // happened to sit there; plain prose and notices must do nothing.)
            if let Some(l) = self.chat_lines().get(li).filter(|l| l.output.is_some() || l.call.is_some()) {
                self.open_line_viewer_quiet(l.source_index);
            }
        }
    }

    /// The original prompt behind a user bubble: strip the ❯ prefix, restore
    /// real newlines from the ⏎ flattening push_user applied.
    fn prompt_original(&self) -> Option<String> {
        let idx = self.prompt_menu?;
        let text = &self.lines.get(idx)?.text;
        let body = text.strip_prefix("❯ ").unwrap_or(text);
        Some(body.replace(" ⏎ ", "\n"))
    }

    /// 1 复制原文
    fn prompt_menu_copy(&mut self, serve: &mut Serve) {
        let _ = serve;
        if let Some(original) = self.prompt_original() {
            let head: String = original
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            match self.copy_to_clipboard(&original) {
                Ok(()) => self.toast(format!("已复制原文：{head}")),
                Err(e) => self.push(format!("复制失败：{e}"), Tone::Error),
            }
        }
        self.prompt_menu = None;
    }

    /// 2 编辑重发: locate the turn on the timeline, then editlast to it
    fn prompt_menu_edit(&mut self, serve: &mut Serve) {
        if let Some(original) = self.prompt_original() {
            let head: String = original
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            self.edit_target_head = Some(head);
            let id = serve.command("timeline", vec![]);
            self.pending.push((id, Pending::TimelineMenu));
            self.push("⟲ 定位该回合中 …", Tone::Muted);
        }
        self.prompt_menu = None;
    }

    /// 3 原样重发
    fn prompt_menu_resend(&mut self, serve: &mut Serve) {
        if let Some(original) = self.prompt_original() {
            self.push("↻ 原样重发（忙碌时自动排队）", Tone::Muted);
            self.submit_text(serve, original, vec![]);
        } else {
            self.prompt_menu = None;
        }
    }

    /// 4 引用到输入框
    fn prompt_menu_quote(&mut self) {
        if let Some(original) = self.prompt_original() {
            let head: String = original
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            self.input = format!("关于「{head}」：").chars().collect();
            self.cursor = self.input.len();
        }
        self.prompt_menu = None;
    }

    /// The attached image whose `[图片 #N]` chip covers char index `click_idx` of a line.
    fn image_chip_at(&self, full: &str, click_idx: usize) -> Option<String> {
        let chars: Vec<char> = full.chars().collect();
        for img in &self.images {
            let label: Vec<char> = format!("[图片 #{}]", img.id).chars().collect();
            let mut start = 0;
            while start + label.len() <= chars.len() {
                if chars[start..start + label.len()] == label[..]
                    && click_idx >= start
                    && click_idx < start + label.len()
                {
                    return Some(img.path.clone());
                }
                start += 1;
            }
        }
        None
    }

    /// Map a click in the input box to an image chip, if it lands on one. Uses the same
    /// wrap/scroll math as the draw pass so hits line up with what is on screen.
    fn input_image_at(&self, row: u16, column: u16) -> Option<String> {
        if self.images.is_empty() || self.input_area.width == 0 {
            return None;
        }
        let width = Self::input_text_w(self.input_area.width) as usize;
        let layout = self.input_layout(Self::input_text_w(self.input_area.width));
        let visible = self.input_visible();
        let scroll = layout.cursor_row.saturating_sub(visible - 1);
        let text_top = self.input_text_top();
        if row < text_top || column < INPUT_X {
            return None;
        }
        let column = column - INPUT_X;
        let target_row = (row - text_top) as usize + scroll;
        let rendered = self.rendered_input();
        let mut r = 0usize;
        let mut c = 0usize;
        let mut hit = None;
        for (i, ch) in rendered.text.chars().enumerate() {
            if ch == '\n' {
                r += 1;
                c = 0;
                continue;
            }
            let w = display_width(&ch.to_string());
            if c >= width || (c > 0 && c + w > width) {
                r += 1;
                c = 0;
            }
            if r == target_row && (column as usize) >= c && (column as usize) < c + w {
                hit = Some(i);
                break;
            }
            c += w;
        }
        let idx = hit?;
        let (_, _, id) = rendered
            .image_ranges
            .iter()
            .find(|(s, e, _)| idx >= *s && idx < *e)?;
        self.images
            .iter()
            .find(|img| img.id == *id)
            .map(|img| img.path.clone())
    }

    fn chat_drag(&mut self, row: u16, column: u16) {
        if self.sel_anchor_pos.is_some() {
            self.sel_end_pos = Some((row, column));
        }
    }

    /// Release: a press+release without movement is a click — but only a
    /// double-click acts (opens chips/paths/viewers, toggles, menus); a single
    /// click just anchors/clears the selection. Anything dragged over is copied
    /// exactly to the system clipboard.
    fn chat_release(&mut self, chat_y: u16, row: u16, column: u16) {
        let clicked = self.sel_anchor_pos == Some((row, column));
        let text = self.selected_text(chat_y);
        self.sel_anchor_pos = None;
        self.sel_end_pos = None;
        if clicked {
            let now = std::time::Instant::now();
            let double = self.chat_last_click.is_some_and(|(t, r)| {
                r == row && now.duration_since(t).as_millis() < 400
            });
            self.chat_last_click = Some((now, row));
            if double {
                self.chat_click(row, column, chat_y);
            }
        } else if let Some(text) = text
            && !text.trim().is_empty()
        {
            match self.copy_to_clipboard(&text) {
                Ok(()) => {
                    let n = text.chars().filter(|c| *c != '\n').count();
                    self.toast(format!("已复制 {n} 个字符到剪贴板"));
                }
                Err(e) => self.push(format!("复制失败：{e}"), Tone::Error),
            }
        }
    }

    /// The display char index under a screen position in the input box, using the
    /// draw pass's wrap and scroll. Rows above/below clamp to the first/last line;
    /// columns past a line's end land after its last char.
    fn input_index_at(&self, row: u16, column: u16) -> usize {
        let layout = self.input_layout(Self::input_text_w(self.input_area.width));
        let total = self.rendered_input().text.chars().count();
        let visible = self.input_visible();
        let scroll = layout.cursor_row.saturating_sub(visible - 1);
        let text_top = self.input_text_top();
        if row < text_top {
            return 0;
        }
        let column = column.saturating_sub(INPUT_X);
        let r = (row - text_top) as usize + scroll;
        let (Some(line), Some(&start)) = (layout.lines.get(r), layout.starts.get(r)) else {
            return total;
        };
        if start > total {
            return total; // the vim ":command" line
        }
        let mut col = 0usize;
        for (i, ch) in line.chars().enumerate() {
            let w = display_width(&ch.to_string());
            if (column as usize) < col + w {
                return start + i;
            }
            col += w;
        }
        (start + line.chars().count()).min(total)
    }

    /// The raw input behind display chars [a, b): paste labels expand to the
    /// full pasted text, so what lands on the clipboard is what would be sent.
    fn input_raw_text(&self, a: usize, b: usize) -> String {
        let map = self.rendered_input().display_raw;
        let (a, b) = (a.min(map.len()), b.min(map.len()));
        if a >= b {
            return String::new();
        }
        let (from, to) = (map[a].0, map[b - 1].1);
        self.input[from..to].iter().collect()
    }

    /// Show `text` as a toast for TOAST_MS (replaces any current one). The 1s idle tick redraws,
    /// so it disappears within a second after expiring.
    fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((text.into(), std::time::Instant::now()));
    }

    /// The toast still on screen, if any (expired ones are dropped).
    fn live_toast(&mut self) -> Option<&str> {
        if self.toast.as_ref().is_some_and(|(_, at)| at.elapsed().as_millis() >= TOAST_MS) {
            self.toast = None;
        }
        self.toast.as_ref().map(|(t, _)| t.as_str())
    }

    fn input_copy(&mut self, text: &str) {
        match self.copy_to_clipboard(text) {
            Ok(()) => {
                let n = text.chars().filter(|c| *c != '\n').count();
                self.toast(format!("已复制 {n} 个字符到剪贴板"));
            }
            Err(e) => self.push(format!("复制失败：{e}"), Tone::Error),
        }
    }

    /// Left press in the input box: a second press within 400 ms selects the whole
    /// input and copies it; otherwise it anchors a drag selection.
    fn input_press(&mut self, row: u16, column: u16) {
        let now = std::time::Instant::now();
        let double = self
            .input_last_click
            .is_some_and(|(t, _, _)| now.duration_since(t).as_millis() < 400);
        if double && !self.input.is_empty() {
            self.input_last_click = None;
            self.input_dragging = false;
            let total = self.rendered_input().text.chars().count();
            self.input_sel = Some((0, total));
            let text: String = self.input.iter().collect();
            self.input_copy(&text);
            return;
        }
        self.input_last_click = Some((now, row, column));
        let idx = self.input_index_at(row, column);
        self.input_sel = Some((idx, idx));
        self.input_dragging = true;
    }

    fn input_drag(&mut self, row: u16, column: u16) {
        let idx = self.input_index_at(row, column);
        if let Some((_, end)) = self.input_sel.as_mut() {
            *end = idx;
        }
    }

    /// Release after a drag copies the covered text; a plain click moves the
    /// cursor there (a click on a paste label lands just before it).
    fn input_release(&mut self) {
        self.input_dragging = false;
        let Some((a, b)) = self.input_sel else {
            return;
        };
        let (a, b) = (a.min(b), a.max(b));
        if a == b {
            self.input_sel = None;
            self.cursor = self
                .rendered_input()
                .display_raw
                .get(a)
                .map(|r| r.0)
                .unwrap_or(self.input.len());
            return;
        }
        let text = self.input_raw_text(a, b);
        if !text.trim().is_empty() {
            self.input_copy(&text);
        }
    }

    /// Clickable file paths: show the file's content in the shared read-only viewer.
    /// The picked option goes out as the next prompt; the matching todo entry retires.
    fn pick_choice(&mut self, serve: &mut Serve, idx: usize) {
        let Some(c) = self.choices.take() else { return };
        // the built-in explain row (one past the model's options): ask for a
        // detailed walkthrough of every option ending in the same ?choice
        // block again — the ask itself is not answered
        if idx == c.options.len() && c.suggest.is_none() {
            let numbered: Vec<String> = c
                .options
                .iter()
                .enumerate()
                .map(|(i, o)| format!("{}. {}", i + 1, o.label))
                .collect();
            let prompt = format!(
                "请逐项详细解释这组选项：\n{}\n对每一项说明：它具体会做什么、做了之后发生什么（含风险与代价）、什么情况下该选它。解释完成后，把同一组选项再用 ?choice 块原样给出（若解释让你发现某项表述有误导，可修正措辞后给出完整选项组），不要引入新选项。",
                numbered.join("\n")
            );
            self.submit_text(serve, prompt, vec![]);
            return;
        }
        let Some(pick) = c.options.get(idx).cloned() else {
            self.choices = Some(c);
            return;
        };
        // a suggest panel's options are fixed: 接受 executes, 忽略 just retires it
        if let Some(text) = &c.suggest {
            self.todos
                .retain(|t| !matches!(&t.kind, TodoKind::Suggest { text: s } if s == text));
            if pick.label == "接受" {
                self.submit_text(serve, format!("请执行这条建议：{text}"), vec![]);
            } else {
                self.arm_pending_panel();
            }
            return;
        }
        self.todos.retain(|t| !matches!(&t.kind,
            TodoKind::Choice { question, options, .. } if *question == c.question && *options == c.options));
        self.submit_text(serve, pick.label, vec![]);
    }

    /// At turn end, the first un-handled suggestion takes the panel with a
    /// 接受/忽略 pair — suggestions get the same immediate affordance as choices.
    fn arm_pending_panel(&mut self) {
        if self.busy || self.choices.is_some() {
            return;
        }
        if let Some(text) = self.todos.iter().find_map(|t| match &t.kind {
            TodoKind::Suggest { text } => Some(text.clone()),
            _ => None,
        }) {
            self.choices = Some(Choices {
                question: format!("建议：{text}"),
                preview: Vec::new(),
                options: vec![
                    ChoiceOption {
                        label: "接受".into(),
                        note: None,
                    },
                    ChoiceOption {
                        label: "忽略".into(),
                        note: None,
                    },
                ],
                sel: 0,
                suggest: Some(text),
            });
            self.choices_collapsed = false;
        }
    }

    /// Re-open the option panel for an unanswered choice parked in todo.
    fn expand_choice(&mut self, idx: usize) {
        if let Some(TodoKind::Choice {
            question,
            preview,
            options,
        }) = self.todos.get(idx).map(|t| t.kind.clone())
        {
            self.choices = Some(Choices {
                question,
                preview,
                options,
                sel: 0,
                suggest: None,
            });
            self.choices_collapsed = false;
            self.todo_dialog = false;
            self.todo_buttons.clear();
        }
    }

    /// Act on a suggestion from todo: send it as the next prompt, drop the entry.
    fn run_suggestion(&mut self, serve: &mut Serve, idx: usize) {
        if let Some(TodoKind::Suggest { text }) = self.todos.get(idx).map(|t| t.kind.clone()) {
            self.todos.remove(idx);
            self.submit_text(serve, format!("请执行这条建议：{text}"), vec![]);
        }
    }

    /// The status bar's first span: the history emoji (`H` in ASCII mode), a click target
    /// for /history.
    fn history_label(&self) -> String {
        format!(" {} ", if self.ascii { "H" } else { "🕘" })
    }

    /// `/history`: every prompt of this conversation on its own line, oldest first — the
    /// newest at the bottom and selected — in the completion popup's place above the input.
    fn open_history(&mut self) {
        let mut items = Vec::new();
        let mut lines = Vec::new();
        for (i, l) in self.chat_lines().iter().enumerate() {
            if l.tone != Tone::User || l.notice || l.wordmark {
                continue;
            }
            // a long prompt is shown collapsed; its full text is what the list names
            let text = l.output.clone().unwrap_or_else(|| l.text.clone());
            let text = text.strip_prefix("❯ ").unwrap_or(&text);
            items.push(text.split_whitespace().collect::<Vec<_>>().join(" "));
            lines.push(i);
        }
        if items.is_empty() {
            self.push("还没有输入记录", Tone::Muted);
            return;
        }
        let n = items.len();
        self.popup = Some(Popup {
            items,
            descs: vec![String::new(); n],
            sel: n - 1,
            kind: PopKind::History,
            inserts: Vec::new(),
            cycling: false,
            lines,
        });
    }

    /// Scroll so that chat line `line` starts at the top of the view, as far back as the view
    /// allows; a line that is in the last screenful already stays where it is. Once the chat
    /// is scrolled back the ↓ pill takes a row of it, and that row is accounted for.
    fn jump_to_line(&mut self, line: usize) {
        let total = self.chat_rows.len();
        let Some(first) = self.chat_rows.iter().position(|r| r.line >= line) else {
            return;
        };
        let pill_now = usize::from(self.scroll_back > 0);
        let after = (self.chat_area.height as usize + pill_now).saturating_sub(1);
        if after == 0 {
            return;
        }
        self.scroll_back = total.saturating_sub(after + first);
    }

    /// Every problem on one line: `file:line:col  ✗ message`, files by name, errors first.
    fn problem_rows(&self) -> Vec<(String, Severity)> {
        self.problems
            .iter()
            .flat_map(|f| {
                f.items.iter().map(move |p| {
                    (
                        format!(
                            "{}:{}:{}  {} {}",
                            f.file,
                            p.line,
                            p.col,
                            p.severity.icon(self.ascii),
                            p.message
                        ),
                        p.severity,
                    )
                })
            })
            .collect()
    }

    fn problem_count(&self) -> usize {
        self.problems.iter().map(|f| f.items.len()).sum()
    }

    /// (errors, warnings) over every file
    fn problem_counts(&self) -> (usize, usize) {
        self.problems
            .iter()
            .flat_map(|f| f.items.iter())
            .fold((0, 0), |(e, w), p| match p.severity {
                Severity::Error => (e + 1, w),
                Severity::Warning => (e, w + 1),
                Severity::Info | Severity::Hint => (e, w),
            })
    }

    /// The file and problem at row `idx` of the flattened list.
    fn problem_at(&self, idx: usize) -> Option<(&FileProblems, &Problem)> {
        let mut i = idx;
        for f in &self.problems {
            if i < f.items.len() {
                return Some((f, &f.items[i]));
            }
            i -= f.items.len();
        }
        None
    }

    fn close_problems(&mut self) {
        self.problems_dialog = false;
        self.problems_rows.clear();
    }

    /// /problems dialog keys: ↑↓/j k and 1-9 pick a row, enter puts a fix request for it in
    /// the input (nothing is sent), y copies the list, x clears it, esc/q closes.
    fn problems_key(&mut self, k: crossterm::event::KeyEvent) {
        let n = self.problem_count();
        let sel = self.problems_sel.min(n.saturating_sub(1));
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_problems(),
            KeyCode::Up | KeyCode::Char('k') => self.problems_sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.problems_sel = (sel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < n {
                    self.problems_sel = i;
                }
            }
            KeyCode::Enter if n > 0 => {
                let text = self
                    .problem_at(sel)
                    .map(|(f, p)| format!("请修复 @{}#{}：{}", f.file, p.line, p.message));
                if let Some(text) = text {
                    self.close_problems();
                    self.input = text.chars().collect();
                    self.cursor = self.input.len();
                }
            }
            KeyCode::Char('y') => {
                let text = self
                    .problem_rows()
                    .iter()
                    .map(|(t, _)| t.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                match self.copy_to_clipboard(&text) {
                    Ok(()) => self.toast(format!("已复制 {n} 条诊断")),
                    Err(e) => self.push(format!("复制失败：{e}"), Tone::Error),
                }
            }
            KeyCode::Char('x') => {
                self.problems.clear();
                self.problems_sel = 0;
                self.problems_rows.clear();
            }
            _ => {}
        }
    }

    /// Answer a heuristic question: preload its text into the input box for editing.
    fn reply_question(&mut self, idx: usize) {
        if let Some(TodoKind::Question { text }) = self.todos.get(idx).map(|t| t.kind.clone()) {
            self.todos.remove(idx);
            self.input = text.chars().collect();
            self.cursor = self.input.len();
            self.todo_dialog = false;
            self.todo_buttons.clear();
        }
    }

    /// The user's notes live in `<tango home>/todos.json`, keyed by working directory, so they
    /// survive `/restart` and `/new`. Other todo kinds belong to the conversation.
    fn notes_file(&self) -> Option<std::path::PathBuf> {
        Some(self.tango_home()?.join("todos.json"))
    }

    fn tango_home(&self) -> Option<std::path::PathBuf> {
        if let Some(d) = &self.notes_home {
            return Some(d.clone());
        }
        match std::env::var_os("TANGO_HOME") {
            Some(d) => Some(std::path::PathBuf::from(d)),
            None => Some(std::path::PathBuf::from(std::env::var_os("HOME")?).join(".tango")),
        }
    }

    fn notes_key(&self) -> String {
        self.serve_cwd
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| ".".to_string())
    }

    fn save_notes(&self) {
        let Some(file) = self.notes_file() else { return };
        let notes: Vec<(String, bool)> = self
            .todos
            .iter()
            .filter_map(|t| match &t.kind {
                TodoKind::Note { text, done } => Some((text.clone(), *done)),
                _ => None,
            })
            .collect();
        let mut all: serde_json::Map<String, Value> = fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if notes.is_empty() {
            all.remove(&self.notes_key());
        } else {
            all.insert(
                self.notes_key(),
                Value::Array(notes.into_iter().map(|(t, d)| json!({ "text": t, "done": d })).collect()),
            );
        }
        let _ = fs::write(&file, serde_json::to_string_pretty(&Value::Object(all)).unwrap_or_default());
    }

    /// Load this directory's saved notes (once, when the serve cwd is first known).
    fn load_notes(&mut self) {
        let Some(file) = self.notes_file() else { return };
        if self.notes_loaded {
            return;
        }
        self.notes_loaded = true;
        let all: serde_json::Map<String, Value> = fs::read_to_string(&file)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let Some(list) = all.get(&self.notes_key()).and_then(|v| v.as_array()) else { return };
        let mut at = note_slot(&self.todos);
        for n in list {
            if let (Some(text), Some(done)) = (n["text"].as_str(), n["done"].as_bool()) {
                self.todos.insert(
                    at,
                    TodoItem { kind: TodoKind::Note { text: text.to_string(), done } },
                );
                at += 1;
            }
        }
    }

    /// A /todo row button (click or key): expand / run / reply, or drop.
    fn todo_action(&mut self, serve: &mut Serve, action: TodoAction) {
        match action {
            TodoAction::Expand(i) => self.expand_choice(i),
            TodoAction::Run(i) => self.run_suggestion(serve, i),
            TodoAction::Reply(i) => self.reply_question(i),
            TodoAction::Done(i) => {
                if let Some(TodoKind::Note { done, .. }) = self.todos.get_mut(i).map(|t| &mut t.kind) {
                    *done = !*done;
                    self.save_notes();
                }
            }
            TodoAction::Drop(i) => {
                if i < self.todos.len() {
                    let was_note = matches!(self.todos[i].kind, TodoKind::Note { .. });
                    self.todos.remove(i);
                    if was_note {
                        self.save_notes();
                    }
                }
            }
        }
        self.todo_sel = self.todo_sel.min(self.todos.len().saturating_sub(1));
    }

    /// /todo dialog keys: ↑↓/j k pick a row, 1-9 jump to one, enter acts on it (the
    /// row's first button: ✓ 完成 / 展开 / 执行 / 答复), d/delete drops it, esc/q close.
    fn todo_key(&mut self, serve: &mut Serve, k: crossterm::event::KeyEvent) {
        let n = self.todos.len();
        let sel = self.todo_sel.min(n.saturating_sub(1));
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.todo_dialog = false;
                self.todo_buttons.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => self.todo_sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.todo_sel = (sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < n {
                    self.todo_sel = i;
                }
            }
            KeyCode::Enter if n > 0 => {
                let act = match self.todos[sel].kind {
                    TodoKind::Choice { .. } => TodoAction::Expand(sel),
                    TodoKind::Suggest { .. } => TodoAction::Run(sel),
                    TodoKind::Question { .. } => TodoAction::Reply(sel),
                    TodoKind::Note { .. } => TodoAction::Done(sel),
                };
                self.todo_action(serve, act);
            }
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace if n > 0 => {
                self.todo_action(serve, TodoAction::Drop(sel))
            }
            _ => {}
        }
    }

    /// /mcp and /mcp add: configured servers (with their address and last error), then the catalog.
    fn render_mcp_list(&mut self, d: &Value) {
        let mut text = String::from("MCP 服务器：\n\n");
        if let Some(list) = d["servers"].as_array() {
            for s in list {
                let connected = s["connected"].as_bool().unwrap_or(false);
                let target = s["target"].as_str().unwrap_or("").chars().take(90).collect::<String>();
                text.push_str(&format!(
                    "  {} {}\n",
                    s["name"].as_str().unwrap_or("?"),
                    if connected { "· 已连接" } else { "· 未连接" }
                ));
                if !target.is_empty() {
                    text.push_str(&format!("      {target}\n"));
                }
                if let Some(e) = s["error"].as_str() {
                    let e: String = e.chars().take(160).collect();
                    text.push_str(&format!("      {e}\n"));
                }
            }
        }
        if d["servers"].as_array().is_none_or(|a| a.is_empty()) {
            text.push_str("  （无）\n");
        }
        text.push_str("\n工具以 mcp_<server>_<tool> 形式出现在模型工具表里\n");
        if let Some(list) = d["catalog"].as_array()
            && !list.is_empty()
        {
            text.push_str("\n常用远程服务器 · /mcp add <名字> 添加到 ~/.tango/config.json（然后 /restart）：\n\n");
            for c in list {
                let added = d["servers"]
                    .as_array()
                    .is_some_and(|ss| ss.iter().any(|s| s["name"].as_str() == c["id"].as_str()));
                text.push_str(&format!(
                    "  {:<10} {}{}\n",
                    format!("/mcp add {}", c["id"].as_str().unwrap_or("?")),
                    c["description"].as_str().unwrap_or(""),
                    if added { "  ✓ 已添加" } else { "" }
                ));
                let auth = c["auth"].as_str().unwrap_or("none");
                if auth != "none" {
                    text.push_str(&format!("              需要：{auth}\n"));
                }
            }
        }
        self.viewer = Some(Viewer::read_only("MCP", text));
    }

    /// merge one queued item into the RUNNING turn (clickable [并入当前])
    fn queue_supplement_one(&mut self, serve: &mut Serve, idx: usize) {
        if !self.busy {
            self.push(
                "当前没有进行中的回合 — 回车直接发送或等待排队轮到它",
                Tone::Muted,
            );
            return;
        }
        if let Some(q) = self.local_queue.get(idx).cloned() {
            self.local_queue.remove(idx);
            let head: String = q
                .text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            serve.command("supplement", prompt_extras(q.text, q.images));
            self.push(
                format!("➕ 排队 #{} 已并入当前回合：{head}", idx + 1),
                Tone::Success,
            );
        }
    }

    /// merge the WHOLE queue into the running turn (clickable [全部并入当前])
    fn queue_action(&mut self, serve: &mut Serve, action: QueueAction) {
        match action {
            QueueAction::Supplement(i) => self.queue_supplement_one(serve, i),
            QueueAction::Remove(i) => {
                if let Some(q) = self.local_queue.get(i).cloned() {
                    self.local_queue.remove(i);
                    let head: String =
                        q.text.lines().next().unwrap_or("").chars().take(40).collect();
                    self.push(format!("已移除排队 #{}：{head}", i + 1), Tone::Muted);
                }
            }
            QueueAction::SupplementAll => self.queue_supplement_all(serve),
            QueueAction::ClearAll => {
                let n = self.local_queue.len();
                self.local_queue.clear();
                self.push(format!("已清空 {n} 条排队提示"), Tone::Muted);
            }
        }
        self.queue_sel = self.queue_sel.min(self.local_queue.len().saturating_sub(1));
        // nothing left to manage: the dialog closes itself (merge all, or the last one merged/removed)
        if self.local_queue.is_empty() {
            self.queue_dialog = false;
            self.queue_buttons.clear();
        }
    }

    /// /queue dialog keys: ↑↓/j k pick a row, enter/m merges it, d/delete drops
    /// it, a merges all, c clears, 1-9 jump to a row, esc/q close.
    fn queue_key(&mut self, serve: &mut Serve, k: crossterm::event::KeyEvent) {
        let n = self.local_queue.len();
        let sel = self.queue_sel.min(n.saturating_sub(1));
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.queue_dialog = false;
                self.queue_buttons.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => self.queue_sel = sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.queue_sel = (sel + 1).min(n.saturating_sub(1))
            }
            KeyCode::Char(c @ '1'..='9') => {
                let i = c as usize - '1' as usize;
                if i < n {
                    self.queue_sel = i;
                }
            }
            KeyCode::Enter | KeyCode::Char('m') if n > 0 => {
                self.queue_action(serve, QueueAction::Supplement(sel))
            }
            KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace if n > 0 => {
                self.queue_action(serve, QueueAction::Remove(sel))
            }
            KeyCode::Char('a') if n > 0 => self.queue_action(serve, QueueAction::SupplementAll),
            KeyCode::Char('c') if n > 0 => self.queue_action(serve, QueueAction::ClearAll),
            _ => {}
        }
    }

    fn queue_supplement_all(&mut self, serve: &mut Serve) {
        if !self.busy {
            self.push(
                "当前没有进行中的回合 — 排队会在回合结束后依次发送",
                Tone::Muted,
            );
            return;
        }
        let n = self.local_queue.len();
        if n == 0 {
            return;
        }
        for q in std::mem::take(&mut self.local_queue) {
            serve.command("supplement", prompt_extras(q.text, q.images));
        }
        self.push(format!("➕ 已把 {n} 条排队全部并入当前回合"), Tone::Success);
    }

    /// Open an image in the system viewer (macOS Preview / xdg-open). The TUI shows a
    /// chip; clicking it is the deliberate "let me see it" gesture.
    fn open_image(&mut self, path: &str) {
        if cfg!(test) {
            self.push(format!("🖼 打开图片 {path}"), Tone::Muted);
            return;
        }
        #[cfg(target_os = "macos")]
        let program = "open";
        #[cfg(not(target_os = "macos"))]
        let program = "xdg-open";
        match Command::new(program)
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => self.push(format!("🖼 已用系统查看器打开图片：{path}"), Tone::Muted),
            Err(e) => self.push(format!("无法打开图片 {path}：{e}"), Tone::Error),
        }
    }

    /// Open a directory in the system file manager: Finder on macOS, Explorer on
    /// Windows, xdg-open elsewhere.
    fn open_dir(&mut self, path: &str) {
        if cfg!(test) {
            self.push(format!("📂 打开目录 {path}"), Tone::Muted);
            return;
        }
        #[cfg(target_os = "macos")]
        let program = "open";
        #[cfg(target_os = "windows")]
        let program = "explorer";
        #[cfg(all(unix, not(target_os = "macos")))]
        let program = "xdg-open";
        match Command::new(program)
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(_) => self.push(format!("📂 已在文件管理器打开：{path}"), Tone::Muted),
            Err(e) => self.push(format!("无法打开目录 {path}：{e}"), Tone::Error),
        }
    }

    fn open_file_viewer(&mut self, path: &str) {
        // directories open in the system file manager, never as text
        if std::path::Path::new(path).is_dir() {
            self.open_dir(path);
            return;
        }
        let trimmed = path.trim_end_matches(|c: char| ".,;:!?\"')]".contains(c));
        let line_free = trimmed.trim_end_matches(|c: char| c.is_ascii_digit() || c == ':');
        let candidate = std::path::Path::new(line_free);
        let target = if candidate.is_file() {
            line_free
        } else {
            trimmed
        };
        // images open in the system viewer, never as binary "text" in the viewer
        if image_path_from_paste(target).is_some() {
            self.open_image(target);
            return;
        }
        match std::fs::read_to_string(target) {
            Ok(body) => {
                let capped = if body.len() > 200_000 {
                    format!(
                        "{}\n…[文件过大，仅显示前 200KB]",
                        body.chars().take(200_000).collect::<String>()
                    )
                } else {
                    body
                };
                self.viewer = Some(Viewer::read_only(format!("文件 · {target}"), capped));
            }
            Err(e) => self.push(format!("无法读取 {target}：{e}"), Tone::Error),
        }
    }

    /// Single-key dialogs (y/n/a) are unreachable through a CJK IME. While macism
    /// is installed, switch to ABC for the dialog and restore afterwards.
    fn ime_for_dialog(&mut self) {
        if cfg!(test) {
            return;
        }
        let Some(cur) = ime_current() else { return };
        if !ime_should_switch(&cur) {
            self.ime_restore = None; // already ABC — nothing to do
            return;
        }
        if ime_switch("com.apple.keylayout.ABC") {
            self.ime_restore = Some(cur);
        }
    }

    fn ime_restore_prev(&mut self) {
        if let Some(prev) = self.ime_restore.take()
            && !cfg!(test)
        {
            ime_switch(&prev);
        }
    }

    fn jump_to_bottom(&mut self) {
        self.scroll_back = 0;
        self.unread_rows = 0;
    }

    fn scroll_chat_down(&mut self, rows: usize) {
        self.scroll_back = self.scroll_back.saturating_sub(rows);
        if self.scroll_back == 0 {
            self.unread_rows = 0;
        }
    }

    fn hhmm(&self) -> String {
        // local time (the old hardcoded UTC+8 broke for any other timezone)
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        let offset = local_utc_offset_seconds();
        let local = (secs + offset).rem_euclid(86_400);
        format!("[{:02}:{:02}]", local / 3600, (local / 60) % 60)
    }

    fn push(&mut self, text: impl Into<String>, tone: Tone) {
        let ts = self.hhmm();
        for line in text.into().split('\n') {
            self.lines.push(ChatLine {
                text: line.to_string(),
                tone,
                ts: Some(ts.clone()),
                output: None,
                prompt_state: None,
                tool_id: None,
                source_index: usize::MAX,
                notice: false,
                expanded: false,
                call: None,
                frame: None,
                wordmark: false,
            });
        }
    }

    fn push_tool(&mut self, text: impl Into<String>, tone: Tone, output: Option<String>) {
        self.lines.push(ChatLine {
            text: text.into(),
            tone,
            ts: Some(self.hhmm()),
            output,
            prompt_state: None,
            tool_id: None,
            source_index: usize::MAX,
            notice: false,
            expanded: false,
            call: None,
            frame: None,
            wordmark: false,
        });
    }

    fn push_user(&mut self, text: &str) {
        let chars = text.chars().count();
        let line_count = text.lines().count();
        if chars > 600 || line_count > 8 {
            let preview = text.lines().take(2).collect::<Vec<_>>().join(" ⏎ ");
            let preview = preview.chars().take(160).collect::<String>();
            self.push_tool(
                format!("❯ [粘贴 {chars} 字 · {line_count} 行 · ctrl+o/双击展开]\n  {preview} …"),
                Tone::User,
                Some(text.to_string()),
            );
        } else {
            // one ChatLine (one bubble) whose newlines the wrapper turns into real rows
            self.push_tool(format!("❯ {text}"), Tone::User, None);
        }
        if let Some(line) = self.lines.last_mut() {
            line.prompt_state = Some(PromptState::Draft);
        }
    }

    fn viewer_title(line: &ChatLine) -> String {
        let first = line.text.lines().next().unwrap_or("content");
        if first.starts_with("❯ [粘贴") {
            "粘贴上下文".into()
        } else if let Some(tool) = first.strip_prefix("⚙ ") {
            format!("工具输出 · {}", tool.chars().take(48).collect::<String>())
        } else {
            first.chars().take(64).collect()
        }
    }

    fn detect_nvim(&mut self) -> Option<Neovim> {
        self.nvim.get_or_insert_with(crate::nvim::detect).clone()
    }

    fn request_viewer(&mut self, viewer: Viewer) {
        let preference = self.nvim_preference.clone();
        match (preference.as_str(), self.detect_nvim()) {
            ("readonly", Some(editor)) => self.launch_nvim(editor, viewer, true),
            ("internal", _) => self.viewer = Some(viewer),
            ("ask", Some(_)) => {
                self.pending_viewer = Some(viewer);
                let item = |id: &str, label: &str, note: &str| PickItem {
                    group: String::new(),
                    id: id.into(),
                    label: label.into(),
                    note: note.into(),
                    warn: false,
                    current: false,
                };
                self.picker = Some(Picker::new(
                    PickerKind::NvimBackend,
                    vec![
                        item("once", "仅此次使用 Neovim 只读", "本次打开，下次仍询问"),
                        item("always", "以后都使用 Neovim 只读", "保存到配置"),
                        item("edit", "本次使用 Neovim 编辑", "保存后选择如何处理修改"),
                        item("internal", "以后都用内置查看器", "存到配置，不启动外部编辑器"),
                    ],
                ));
            }
            _ => self.viewer = Some(viewer),
        }
    }

    fn launch_nvim(&mut self, editor: Neovim, viewer: Viewer, read_only: bool) {
        self.picker = None;
        self.pending_viewer = None;
        self.viewer = None;
        self.nvim_request = Some(NvimRequest {
            editor,
            viewer,
            read_only,
            target: NvimTarget::Viewer,
            plain: false,
        });
    }

    /// /editor: generic $VISUAL/$EDITOR round-trip (no nvim-specific flags)
    fn edit_draft_generic(&mut self) {
        let program = std::env::var("VISUAL")
            .or_else(|_| std::env::var("EDITOR"))
            .ok()
            .filter(|p| !p.trim().is_empty());
        let Some(program) = program else {
            self.push(
                "未设置 $EDITOR/$VISUAL · 设置后重试（/nvim 始终可用）",
                Tone::Muted,
            );
            return;
        };
        let draft: String = self.input.iter().collect();
        self.popup = None;
        self.vim_command = None;
        self.nvim_request = Some(NvimRequest {
            editor: Neovim {
                executable: program,
            },
            viewer: Viewer::new("输入草稿", draft),
            read_only: false,
            target: NvimTarget::Draft,
            plain: true,
        });
    }

    fn edit_draft(&mut self) {
        let Some(editor) = self.detect_nvim() else {
            self.push("未检测到 Neovim，草稿已保留", Tone::Muted);
            return;
        };
        let draft: String = self.input.iter().collect();
        self.popup = None;
        self.vim_command = None;
        self.nvim_request = Some(NvimRequest {
            editor,
            viewer: Viewer::new("输入草稿", draft),
            read_only: false,
            target: NvimTarget::Draft,
            plain: false,
        });
    }

    pub fn finish_draft_nvim(&mut self, original: &str, edited: impl Into<String>) {
        let edited = edited.into();
        let replacement: Vec<char> = edited.chars().collect();
        let end = self.input.len();
        self.replace_input_range(0, end, &replacement);
        self.cursor = self.input.len();
        self.popup = None;
        self.viewer = None;
        self.push(
            if original == edited {
                "草稿未修改"
            } else {
                "Neovim 草稿已更新 · 尚未发送"
            },
            Tone::Muted,
        );
    }

    pub fn nvim_draft_error(&mut self, original: &str, error: String) {
        let replacement: Vec<char> = original.chars().collect();
        let end = self.input.len();
        self.replace_input_range(0, end, &replacement);
        self.cursor = self.input.len();
        self.push(format!("Neovim 启动失败：{error}；草稿已保留"), Tone::Error);
    }

    pub fn finish_nvim(&mut self, viewer: Viewer, edited: String, read_only: bool) {
        self.viewer = None;
        if read_only {
            return;
        }
        if viewer.text == edited {
            self.push("内容未修改", Tone::Muted);
            return;
        }
        let notice = match viewer.prompt_state {
            Some(PromptState::Running) => "原内容已提交且仍在处理；选择后将打断当前回合",
            Some(PromptState::Completed) => "原内容已经处理完成；请选择修改后的处理方式",
            _ => "内容已修改；请选择后续处理方式",
        };
        self.push(notice, Tone::Warning);
        self.edited = Some((viewer, edited));
        let item = |id: &str, label: &str, note: &str| PickItem {
            group: String::new(),
            id: id.into(),
            label: label.into(),
            note: note.into(),
            warn: false,
            current: false,
        };
        self.picker = Some(Picker::new(
            PickerKind::EditResolution,
            vec![
                item("new", "作为全新处理", "发送修改后的完整内容"),
                item("reprocess", "重新处理", "用完整修改稿重做原请求"),
                item("correct", "修正前面的处理", "只发送原始内容与修改稿的 diff"),
            ],
        ));
    }

    pub fn nvim_error(&mut self, viewer: Viewer, error: String) {
        self.viewer = Some(viewer);
        self.push(
            format!("Neovim 启动失败：{error}；已回退内置查看器"),
            Tone::Error,
        );
    }

    fn open_latest_viewer(&mut self) {
        let found = self
            .lines
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, line)| {
                Self::line_viewer_text(line).map(|output| {
                    (
                        Self::viewer_title(line),
                        output,
                        index,
                        line.prompt_state,
                    )
                })
            });
        if let Some((title, text, index, state)) = found {
            self.viewer = Some(Viewer::with_source(title, text, index, state));
        }
    }

    fn open_transcript(&mut self) {
        let mut text = String::new();
        let mut prompt_rows = Vec::new();
        let mut row = 0;
        for line in self.chat_lines() {
            if line.wordmark {
                continue; // decoration, not transcript
            }
            // the transcript is for reading/searching: thinking always in full
            let mut full = (line.tone == Tone::Thinking)
                .then(|| self.lines.get(line.source_index).map(|l| l.text.clone()))
                .flatten()
                .unwrap_or_else(|| line.text.clone());
            // a ⚙ line cut to a brief: the transcript carries the whole call
            if let Some(call) = line.call.as_ref().filter(|c| !full.contains(c.as_str())) {
                for part in call.lines() {
                    full.push_str("\n  $ ");
                    full.push_str(part);
                }
            }
            for (index, part) in full.split('\n').enumerate() {
                if index == 0 && line.prompt_state.is_some() {
                    prompt_rows.push(row);
                }
                text.push_str(part);
                text.push('\n');
                row += 1;
            }
        }
        if !self.stream.is_empty() {
            text.push_str(&self.stream);
        }
        self.viewer = Some(Viewer::transcript(
            "Transcript",
            text.trim_end(),
            prompt_rows,
        ));
    }

    /// Opens a chat line's full output in the viewer. Reports success, so a plain-text
    /// click can fall through to path-click / selection instead of silently doing nothing.
    fn open_line_viewer_quiet(&mut self, index: usize) -> bool {
        let found = self.lines.get(index).and_then(|line| {
            Self::line_viewer_text(line)
                .map(|output| (Self::viewer_title(line), output, line.prompt_state))
        });
        match found {
            // a chat line's output is for a quick look (and maybe a copy): always the
            // built-in dialog, never Neovim — ctrl+e still hands it to nvim on demand
            Some((title, text, state)) => {
                self.viewer = Some(Viewer::with_source(title, text, index, state));
                true
            }
            None => false,
        }
    }

    /// One-dimension summary of a tool call for its status line.
    /// The ⚙ line's text for a call, plus the full detail when the line had to cut
    /// it: a bash command longer than one line or BRIEF_CHARS shows its first line
    /// and how much was left out; the rest lives in the viewer and transcript.
    fn call_brief(input: &Value) -> (String, String) {
        let full = Self::call_summary(input);
        (fit_call(&full, BRIEF_CHARS), full)
    }

    /// What a ⚙ line's viewer shows: the full call (as `$ command`) above its output.
    fn line_viewer_text(line: &ChatLine) -> Option<String> {
        match (&line.call, &line.output) {
            (Some(call), Some(out)) if !out.trim().is_empty() => Some(format!("$ {call}\n\n{out}")),
            (Some(call), _) => Some(format!("$ {call}")),
            (None, out) => out.clone(),
        }
    }

    /// A tool result that only says the user's interrupt killed it — shown as a
    /// warning, not an error (`[exit null]` is what older sessions recorded).
    fn interrupted_output(output: Option<&str>) -> bool {
        output.is_some_and(|o| {
            let last = o.trim_end().lines().last().unwrap_or("");
            last == "[interrupted]" || last == "[exit null]"
        })
    }

    fn string_array(v: &Value) -> Vec<String> {
        v.as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }

    fn call_summary(input: &Value) -> String {
        let m = input.as_object();
        // task sub-agents: `explore · find the auth middleware` (+ ` · 后台`)
        if let Some(desc) = m.and_then(|o| o.get("description")).and_then(|v| v.as_str())
            && m.is_some_and(|o| o.contains_key("prompt"))
        {
            let agent = input["agent"].as_str().unwrap_or("explore");
            let bg = if input["background"].as_bool() == Some(true) { " · 后台" } else { "" };
            return format!("{agent} · {}{bg}", desc.chars().take(200).collect::<String>());
        }
        // lsp: `definition · src/x.ts:12 add`
        if let (Some(op), Some(path)) = (input["op"].as_str(), input["path"].as_str()) {
            let mut out = format!("{op} · {path}");
            if let Some(line) = input["line"].as_u64() {
                out.push_str(&format!(":{line}"));
            }
            if let Some(sym) = input["symbol"].as_str().or_else(|| input["query"].as_str()) {
                out.push_str(&format!(" {sym}"));
            }
            return out;
        }
        let detail = ["command", "path", "url", "query", "pattern"]
            .iter()
            .find_map(|k| m.and_then(|o| o.get(*k)).and_then(|v| v.as_str()))
            .map(String::from)
            .unwrap_or_else(|| input.to_string());
        let d: String = detail.chars().take(4000).collect();
        d
    }

    /// Lightweight markdown commit: fences become indented gray blocks (markers stripped),
    /// headings turn bold, ** and ` markers vanish, `|` tables are re-flowed into aligned
    /// columns. No inline spans — one style per line.
    /// The language the conversation is written in, from the newest user prompt
    /// that has letters: CJK ideographs alone → Chinese, kana → Japanese, hangul →
    /// Korean, otherwise English. Chat lines the TUI itself wrote (tools, notices,
    /// recaps) are skipped, and so is the tab lead-in itself (it would otherwise
    /// pin the language after one press).
    fn chat_language(&self) -> ChatLang {
        for line in self.lines.iter().rev() {
            if line.tone != Tone::User || line.wordmark || line.notice {
                continue;
            }
            let text = line.text.strip_prefix("❯ ").unwrap_or(&line.text);
            let text = Self::strip_tab_lead(text);
            if let Some(l) = ChatLang::of(text) {
                return l;
            }
        }
        ChatLang::default()
    }

    /// "Tango 在等你选择" — the panel title and the stand-in question when the model
    /// wrote nothing before `?choice`.
    fn waiting_label(lang: ChatLang) -> String {
        lang.waiting().to_string()
    }

    /// The choice panel's title, in the conversation's language.
    fn choice_title(&self) -> String {
        let label = Self::waiting_label(self.chat_language());
        if self.ascii {
            format!("? {label}")
        } else {
            format!("❓ {label}")
        }
    }

    /// What Tab puts in the input box so the user can write their own answer
    /// instead of picking an option: the ask as a lead-in that ends in ": ", in the
    /// conversation's language — `关于要改吗，Tango 在等你选择: `, `About: ship it now? The
    /// model is waiting for your choice: `. A closing ? / ？ of the question is
    /// dropped; a question that is only the stand-in label gets no "关于".
    fn amend_lead(question: &str, lang: ChatLang) -> String {
        let q = question.trim();
        let waiting = lang.waiting();
        if q == waiting || q.is_empty() {
            return format!("{waiting}: ");
        }
        let q = q.trim_end_matches(['?', '？']).trim_end();
        format!("{}{q}{}{waiting}: ", lang.about(), lang.about_sep())
    }

    /// `(answer, true)` when `text` is Tab's lead-in followed by the user's answer:
    /// the lead-in is `<about>…<waiting>: ` — cut at the FIRST `<waiting>: ` after
    /// the `<about>` opener, so an answer that repeats the phrase stays whole.
    fn split_tab_lead(text: &str) -> (&str, bool) {
        for lang in ChatLang::ALL {
            let Some(head) = text.strip_prefix(lang.about()) else {
                continue;
            };
            let mark = format!("{}: ", lang.waiting());
            if let Some(n) = head.find(&mark) {
                return (&head[n + mark.len()..], true);
            }
        }
        (text, false)
    }

    /// Undo `amend_lead` for a text that starts with it (so a language guess or a
    /// resend never reads the lead-in as the user's words).
    fn strip_tab_lead(text: &str) -> &str {
        Self::split_tab_lead(text).0
    }

    fn commit_stream(&mut self) {
        if !self.thinking.is_empty() {
            // the whole block is kept; chat_lines shows a collapsed preview until
            // a double-click expands it
            let t = std::mem::take(&mut self.thinking);
            self.push_tool(format!("{THINK_ICON} {}", t.trim_end()), Tone::Thinking, None);
        }
        if self.stream.is_empty() {
            return;
        }
        let s = std::mem::take(&mut self.stream);
        self.assistant_replies.push(s.trim_end().to_string());
        if self.assistant_replies.len() > 20 {
            self.assistant_replies.remove(0);
        }
        let mut in_fence = false;
        let lines: Vec<&str> = s.trim_end().split('\n').collect();
        // ?choice block: lines after the marker up to the first non-option line become
        // the option panel instead of transcript text
        let mut in_choice = false;
        let mut choice_options: Vec<ChoiceOption> = Vec::new();
        let mut choice_preview: Vec<String> = Vec::new();
        let mut last_text = String::new();
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
            // the ?choice marker opens the block wherever it sits in the line
            if !in_choice && let Some(pos) = line.find("?choice") {
                let before = line[..pos].trim_end();
                if !before.is_empty() {
                    last_text = before.chars().take(60).collect();
                    self.push(before.to_string(), Tone::Text);
                }
                in_choice = true;
                i += 1;
                continue;
            }
            // an open ?choice block eats options, "| " preview lines and indented
            // annotations for the option above
            if in_choice {
                if let Some(rest) = Self::option_label(line) {
                    choice_options.push(ChoiceOption {
                        label: rest.to_string(),
                        note: None,
                    });
                    i += 1;
                    continue;
                }
                if let Some(rest) = line.strip_prefix("| ") {
                    choice_preview.push(rest.to_string());
                    i += 1;
                    continue;
                }
                let indented = line.starts_with("   ") || line.starts_with('\t');
                if indented && let Some(last) = choice_options.last_mut() {
                    last.note = Some(line.trim().to_string());
                    i += 1;
                    continue;
                }
                if choice_options.is_empty() {
                    // marker seen but no options yet — keep scanning
                    i += 1;
                    continue;
                }
                // first non-option line closes the block; fall through to render it
                in_choice = false;
            }
            // side suggestion: strip the marker (wherever it sits in the line), keep the
            // text in context, park it in todo
            if let Some(pos) = line.find("?suggest") {
                let before = line[..pos].trim_end();
                let rest = line[pos + "?suggest".len()..].trim();
                if !before.is_empty() {
                    self.push(before.to_string(), Tone::Text);
                }
                if !rest.is_empty() {
                    let suggestion = rest.to_string();
                    self.push(suggestion.clone(), Tone::Muted);
                    if !self.todos.iter().any(
                        |t| matches!(&t.kind, TodoKind::Suggest { text } if *text == suggestion),
                    ) {
                        self.todos.push(TodoItem {
                            kind: TodoKind::Suggest { text: suggestion },
                        });
                    }
                }
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
            last_text = t.chars().take(60).collect();
            self.push(t, Tone::Text);
            i += 1;
        }
        if !choice_options.is_empty() {
            let question = if last_text.is_empty() {
                // no question before ?choice: a label in the conversation's language
                Self::waiting_label(self.chat_language())
            } else {
                last_text
            };
            self.choices = Some(Choices {
                question: question.clone(),
                preview: choice_preview.clone(),
                options: choice_options.clone(),
                sel: 0,
                suggest: None,
            });
            self.choices_collapsed = false;
            // unanswered choices are findable forever: park one per ask in todo
            let already = self.todos.iter().any(|t| matches!(&t.kind,
                TodoKind::Choice { question: q, options: o, .. } if *q == question && *o == choice_options));
            if !already {
                self.todos.push(TodoItem {
                    kind: TodoKind::Choice {
                        question,
                        preview: choice_preview,
                        options: choice_options,
                    },
                });
            }
        } else if s.trim_end().ends_with('？') || s.trim_end().ends_with('?') {
            // plain question, no marker — heuristic: park it as an answerable todo row
            // (any next prompt counts as the answer, so it can never go stale)
            let text = s.trim_end().lines().last().unwrap_or("").trim().to_string();
            if !text.is_empty()
                && !self
                    .todos
                    .iter()
                    .any(|t| matches!(&t.kind, TodoKind::Question { text: q } if *q == text))
            {
                self.todos.push(TodoItem {
                    kind: TodoKind::Question { text },
                });
            }
        }
    }

    /// "1. text" / "1、text" / "1) text" → Some("text")
    fn option_label(line: &str) -> Option<&str> {
        let t = line.trim_start();
        let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let after = &t[digits..];
        after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix("、"))
            .or_else(|| after.strip_prefix(") "))
            .map(str::trim)
            .filter(|rest| !rest.is_empty())
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
        // step past the whole whitespace char: a full-width space (U+3000) is 3 bytes
        let start = s[..self.cursor_byte()]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
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
        // slash palette: input is the command word only. Busy too: Enter runs slash
        // commands mid-turn (see run_slash), so completing them then is just as useful
        if self.pastes.is_empty() && text.starts_with('/') && !text.contains(' ') {
            let items: Vec<(&str, &str)> = SLASH
                .iter()
                .copied()
                // renamed commands keep working but are not offered (desc ends "（旧名）")
                .filter(|(c, d)| c.starts_with(text.as_str()) && !d.ends_with("（旧名）"))
                .collect();
            if items.is_empty() || (items.len() == 1 && items[0].0 == text) {
                self.popup = None;
            } else {
                self.popup = Some(Popup {
                    items: items.iter().map(|(c, _)| c.to_string()).collect(),
                    descs: items.iter().map(|(_, d)| d.to_string()).collect(),
                    sel: 0,
                    kind: PopKind::Slash,
                    inserts: Vec::new(),
                    cycling: false,
                    lines: Vec::new(),
                });
            }
            return;
        }
        // argument palette: "/mode a", "/model gl" … (one argument, cursor at the end)
        if self.pastes.is_empty() && self.cursor == self.input.len() {
            if let Some((cmd, arg)) = text.split_once(' ')
                && !arg.contains(char::is_whitespace)
                && let Some(pop) = self.arg_popup(cmd, arg)
            {
                self.popup = Some(pop);
                return;
            }
        }
        // @file palette: cursor sits inside an @token
        let (tok, _, _) = self.token_at_cursor();
        if tok.starts_with('@') && tok.len() > 1 {
            let mut items = Self::file_candidates(&tok, self.serve_cwd.as_deref());
            // a bare name (no '/') also matches anywhere in the repo, fuzzily, after the
            // directory's own prefix matches
            let body = &tok[1..];
            if !body.contains('/') && !body.contains('#') && items.len() < 12 {
                for hit in fuzzy_files(body, self.repo_files()) {
                    let hit = format!("@{hit}");
                    if !items.contains(&hit) {
                        items.push(hit);
                    }
                    if items.len() >= 12 {
                        break;
                    }
                }
            }
            if items.is_empty() {
                self.popup = None;
            } else {
                self.popup = Some(Popup {
                    items: items.clone(),
                    descs: vec![String::new(); items.len()],
                    sel: 0,
                    kind: PopKind::File,
                    inserts: Vec::new(),
                    cycling: false,
                    lines: Vec::new(),
                });
            }
            return;
        }
        self.popup = None;
    }

    /// candidates for the argument of `cmd`, filtered by the typed prefix `arg`:
    /// prefix matches first, then in-order subsequence ("glm54" → zhipu-plan/glm-5.4)
    fn arg_popup(&self, cmd: &str, arg: &str) -> Option<Popup> {
        // (shown, desc, inserted)
        let all: Vec<(String, String, String)> = match cmd {
            "/mode" => [
                ("plan", "只出计划 · 只研究、写计划，不改文件也不跑命令"),
                ("confirm", "每步确认 · 改文件、跑命令前都先问"),
                ("ask", "风险确认（默认） · 项目内的改动直接做，高风险操作先问"),
                ("auto", "完全放行 · 不再确认，全部直接执行"),
                ("readonly", "只读 · 只看不改，写入和命令都拒绝"),
            ]
            .iter()
            .map(|(m, d)| (m.to_string(), d.to_string(), m.to_string()))
            .collect(),
            // the levels differ per model (serve sends them with hello and /model); off is always there
            "/effort" => {
                let cur = self.effort.as_str();
                let mut all: Vec<(String, String, String)> = self
                    .efforts
                    .iter()
                    .map(|e| {
                        let shown = if e == cur { format!("{e} (当前)") } else { e.clone() };
                        (shown, effort_note(e).to_string(), e.clone())
                    })
                    .collect();
                let off = if cur == "off" { "off (当前)" } else { "off" };
                all.push((off.to_string(), "不发参数 · 用服务商的默认".to_string(), "off".to_string()));
                all
            }
            "/model" => self
                .arg_models
                .iter()
                .map(|m| (m.clone(), String::new(), m.clone()))
                .collect(),
            "/send" | "/resume" => self
                .arg_sessions
                .iter()
                .filter(|(_, _, current)| !current)
                .map(|(id, label, _)| {
                    let short = short_session(id).to_string();
                    // /send's message follows the id
                    let ins = if cmd == "/send" { format!("{short} ") } else { short.clone() };
                    (short, label.clone(), ins)
                })
                .collect(),
            _ => return None,
        };
        let needle = arg.to_lowercase();
        let subseq = |hay: &str| {
            let mut it = needle.chars().peekable();
            for c in hay.chars() {
                if it.peek() == Some(&c) {
                    it.next();
                }
            }
            it.peek().is_none()
        };
        let mut hits: Vec<&(String, String, String)> =
            all.iter().filter(|(s, _, _)| s.to_lowercase().starts_with(&needle)).collect();
        if hits.is_empty() {
            hits = all
                .iter()
                .filter(|(s, d, _)| subseq(&s.to_lowercase()) || subseq(&d.to_lowercase()))
                .collect();
        }
        // nothing to offer, or the argument is already exactly the only candidate
        if hits.is_empty() || (hits.len() == 1 && hits[0].2.trim_end() == arg) {
            return None;
        }
        Some(Popup {
            items: hits.iter().map(|h| h.0.clone()).collect(),
            descs: hits.iter().map(|h| h.1.clone()).collect(),
            sel: 0,
            kind: PopKind::Arg,
            inserts: hits.iter().map(|h| h.2.clone()).collect(),
            cycling: false,
            lines: Vec::new(),
        })
    }

    /// Tab on an argument popup, zsh menu-complete: a single candidate is inserted and
    /// the popup closes; several: the first Tab inserts the selected one and keeps the
    /// menu open, each further Tab (Shift+Tab backwards) swaps in the next
    fn cycle_arg(&mut self, step: isize) {
        let Some(p) = self.popup.as_mut() else { return };
        let n = p.items.len();
        if n == 0 {
            return;
        }
        if p.cycling {
            p.sel = ((p.sel as isize + step).rem_euclid(n as isize)) as usize;
        }
        p.cycling = true;
        let ins: Vec<char> = p.inserts[p.sel].chars().collect();
        let single = n == 1;
        let text: String = self.input.iter().collect();
        let start = text.split_once(' ').map_or(0, |(c, _)| c.chars().count() + 1);
        let end = self.input.len();
        self.replace_input_range(start, end, &ins);
        self.cursor = self.input.len();
        if single {
            self.popup = None;
        }
    }

    /// tracked + untracked-but-not-ignored files of serve_cwd, cached for 30 s; empty outside git
    fn repo_files(&mut self) -> &[String] {
        let dir = self.serve_cwd.clone().unwrap_or_else(|| std::path::PathBuf::from("."));
        let fresh = matches!(&self.file_index, Some((d, t, _)) if *d == dir && t.elapsed().as_secs() < 30);
        if !fresh {
            let files = Command::new("git")
                .args(["ls-files", "-co", "--exclude-standard"])
                .current_dir(&dir)
                .stderr(Stdio::null())
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .take(50_000)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            self.file_index = Some((dir, std::time::Instant::now(), files));
        }
        &self.file_index.as_ref().unwrap().2
    }

    fn file_candidates(token: &str, base: Option<&Path>) -> Vec<String> {
        let body = &token[1..];
        let (dir, frag) = match body.rfind('/') {
            Some(i) => (&body[..=i], &body[i + 1..]),
            None => ("", body),
        };
        let base = match (base, dir.is_empty()) {
            (Some(b), true) => b.to_path_buf(),
            (Some(b), false) => {
                let d = Path::new(dir);
                if d.is_absolute() {
                    d.to_path_buf()
                } else {
                    b.join(d)
                }
            }
            (None, true) => Path::new(".").to_path_buf(),
            (None, false) => Path::new(dir).to_path_buf(),
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
    /// Enter (or a click) on a row of the command completion: run that row's command as if
    /// typed in full — "/mo" ⏎ runs /model, the first match; ↑↓ picks another row first.
    /// Tab still only completes the word into the input. Other popups accept as before.
    fn run_popup_row(&mut self, serve: &mut Serve) {
        let Some(word) = self
            .popup
            .as_ref()
            .filter(|p| p.kind == PopKind::Slash)
            .and_then(|p| p.items.get(p.sel))
            .and_then(|item| item.split(' ').next())
            .map(str::to_string)
        else {
            self.accept_popup();
            return;
        };
        self.popup = None;
        // the same sequence as a submitted slash command: the draft survives
        let draft = self.take_history_draft();
        self.clear_input();
        self.run_slash(&word, serve);
        self.restore_draft(draft);
    }

    /// the argument menus whose Enter picks the highlighted row: one argument, nothing after
    fn arg_row_runs(&self) -> bool {
        let text: String = self.input.iter().collect();
        matches!(text.split(' ').next(), Some("/effort" | "/mode"))
    }

    /// Enter on a row of the /effort or /mode argument menu: insert that row, run the command
    fn run_arg_row(&mut self, serve: &mut Serve) {
        if let Some(pp) = self.popup.as_mut() {
            pp.cycling = false; // insert the row highlighted, don't step
        }
        self.cycle_arg(1);
        self.popup = None;
        let text: String = self.input.iter().collect();
        // the same sequence as a submitted slash command: the draft survives
        let draft = self.take_history_draft();
        self.clear_input();
        self.run_slash(&text, serve);
        self.restore_draft(draft);
    }

    fn accept_popup(&mut self) {
        let Some(p) = &self.popup else { return };
        let Some(item) = p.items.get(p.sel).cloned() else {
            return;
        };
        match p.kind {
            PopKind::Slash => {
                // never insert the "<id>"-style placeholder; commands with args get a trailing space
                let word = item.split(' ').next().unwrap_or(&item);
                let mut replacement: Vec<char> = word.chars().collect();
                if item.contains(' ') {
                    replacement.push(' ');
                }
                let end = self.input.len();
                self.replace_input_range(0, end, &replacement);
            }
            PopKind::File => {
                let (_tok, start, end) = self.token_at_cursor();
                let s: String = self.input.iter().collect();
                let cb = self.cursor_byte();
                let sb: usize = s[..min(start, cb)].chars().count();
                let eb: usize = s[..min(end, s.len())].chars().count();
                let replacement: Vec<char> = item.chars().collect();
                self.replace_input_range(sb, eb, &replacement);
            }
            PopKind::Arg => {
                // a click: insert that one and close (Tab cycles instead, see cycle_arg)
                if let Some(pp) = self.popup.as_mut() {
                    pp.cycling = false;
                }
                self.cycle_arg(1);
            }
            PopKind::History => {
                // each item knows its chat line: scroll that line to the top of the view
                let line = p.lines.get(p.sel).copied();
                if let Some(line) = line {
                    self.jump_to_line(line);
                }
            }
        }
        self.popup = None;
    }

    // ---------- slash commands ----------

    fn set_vim(&mut self, enabled: bool) {
        let was_vim = self.vim;
        self.vim = enabled;
        self.vim_command = None;
        self.edit_mode = EditMode::Insert;
        self.popup = None;
        if enabled {
            self.push(
                "Vim 已开启 · INSERT 输入 · Esc 转 NORMAL · Ctrl+S 发送 · Enter 换行",
                Tone::Success,
            );
        } else if was_vim {
            self.push("Vim 已关闭 · Enter 发送", Tone::Muted);
        }
    }

    fn open_command_palette(&mut self) {
        let items = SLASH
            .iter()
            .map(|(command, description)| PickItem {
                group: String::new(),
                id: command.split(' ').next().unwrap_or(command).into(),
                label: (*command).into(),
                note: (*description).into(),
                warn: false,
                current: false,
            })
            .collect();
        self.picker = Some(Picker::new(PickerKind::Command, items));
    }

    fn open_prompt_history(&mut self) {
        if self.prompt_history.is_empty() {
            self.push("暂无 prompt 历史", Tone::Muted);
            return;
        }
        let items = self
            .prompt_history
            .iter()
            .enumerate()
            .rev()
            .map(|(index, text)| PickItem {
                group: String::new(),
                id: index.to_string(),
                label: text.lines().next().unwrap_or("").chars().take(80).collect(),
                note: text.clone(),
                warn: false,
                current: false,
            })
            .collect();
        self.picker = Some(Picker::new(PickerKind::History, items));
    }

    fn restore_prompt(&mut self, text: String) {
        let replacement: Vec<char> = text.chars().collect();
        let end = self.input.len();
        self.replace_input_range(0, end, &replacement);
        self.cursor = self.input.len();
        self.popup = None;
    }

    /// (line the cursor sits on, total lines) of the input box
    fn cursor_line(&self) -> (usize, usize) {
        let mut current = 0usize;
        let mut total = 1usize;
        for (i, c) in self.input.iter().enumerate() {
            if *c == '\n' {
                total += 1;
                if self.cursor > i {
                    current += 1;
                }
            }
        }
        (current, total)
    }

    /// Show the history entry under `history_nav`, cursor at its end.
    fn show_history_entry(&mut self) {
        if let Some(i) = self.history_nav
            && let Some(text) = self.prompt_history.get(i)
        {
            self.restore_prompt(text.clone());
        }
    }

    fn take_history_draft(&mut self) -> Option<InputDraft> {
        self.history_nav = None;
        self.history_draft.take()
    }

    fn restore_draft(&mut self, draft: Option<InputDraft>) {
        if let Some(d) = draft {
            self.input = d.input;
            self.cursor = d.cursor;
            self.pastes = d.pastes;
            self.popup = None;
        }
    }

    /// Leave history navigation, putting the unsent draft back in the box.
    fn exit_history_nav(&mut self) {
        let draft = self.take_history_draft();
        self.restore_draft(draft);
    }

    /// Up in the input box: inside a multi-line entry the cursor climbs a line
    /// first; at the top edge it walks back through sent prompts. Returns false
    /// (key falls through to chat scroll) when there is nothing to recall.
    fn history_up(&mut self) -> bool {
        if self.cursor_line().0 > 0 {
            self.move_input_vertical(false);
            return true;
        }
        match self.history_nav {
            None if self.prompt_history.is_empty() => false,
            None => {
                self.history_draft = Some(InputDraft {
                    input: std::mem::take(&mut self.input),
                    cursor: self.cursor,
                    pastes: std::mem::take(&mut self.pastes),
                });
                self.history_nav = Some(self.prompt_history.len() - 1);
                self.show_history_entry();
                true
            }
            Some(0) => true, // already the oldest
            Some(i) => {
                self.history_nav = Some(i - 1);
                self.show_history_entry();
                true
            }
        }
    }

    /// Down is the mirror: last line of a multi-line entry moves the cursor;
    /// past the newest entry the unsent draft comes back to the box.
    fn history_down(&mut self) -> bool {
        let (current, total) = self.cursor_line();
        if current + 1 < total {
            self.move_input_vertical(true);
            return true;
        }
        match self.history_nav {
            None => false, // not navigating → let the key scroll the chat
            Some(i) if i + 1 < self.prompt_history.len() => {
                self.history_nav = Some(i + 1);
                self.show_history_entry();
                true
            }
            Some(_) => {
                self.exit_history_nav();
                true
            }
        }
    }

    fn toggle_draft_stash(&mut self) {
        if self.history_nav.is_some() {
            self.exit_history_nav(); // never stash a recalled entry as the draft
        }
        if !self.input.is_empty() {
            self.stashed_draft = Some(InputDraft {
                input: std::mem::take(&mut self.input),
                cursor: self.cursor,
                pastes: std::mem::take(&mut self.pastes),
            });
            self.cursor = 0;
            self.popup = None;
            self.push("草稿已暂存 · alt+s 恢复", Tone::Muted);
        } else if let Some(draft) = self.stashed_draft.take() {
            self.input = draft.input;
            self.cursor = draft.cursor;
            self.pastes = draft.pastes;
            self.popup = None;
            self.push("草稿已恢复", Tone::Muted);
        }
    }

    /// Explain what a project's config wants to switch on, then ask yes/no.
    fn ask_trust(&mut self, root: &str, keys: &[String], files: &[String]) {
        self.push(format!("项目配置 · {root}"), Tone::Selected);
        for f in files {
            self.push(format!("  {f}"), Tone::Muted);
        }
        self.push(
            format!(
                "  含 {}：会执行命令或放宽权限，信任前不生效（其他配置已生效）",
                keys.join(", ")
            ),
            Tone::Muted,
        );
        let item = |id: &str, label: &str, note: &str| PickItem {
            group: String::new(),
            id: id.into(),
            label: label.into(),
            note: note.into(),
            warn: false,
            current: false,
        };
        let items = vec![
            item("yes", "信任", "启用这些配置并重启 serve（会话保留）"),
            item("no", "不信任", "记住选择，以后不再询问"),
        ];
        self.picker = Some(Picker::new(PickerKind::Trust, items));
    }

    fn run_slash(&mut self, text: &str, serve: &mut Serve) {
        let trimmed = text.trim();
        let (head, rest) = match trimmed.split_once(' ') {
            Some((h, r)) => (h.to_lowercase(), r.trim()),
            None => (trimmed.to_lowercase(), ""),
        };
        match head.as_str() {
            "/exit" | "/quit" | ":q" => self.quit = true,
            "/restart" => {
                if self.busy {
                    self.push("回合进行中 · 先 esc 中断再 /restart", Tone::Error);
                } else if self.session_full.is_empty() {
                    self.push("serve 还没就绪，没有可续的会话", Tone::Error);
                } else {
                    self.restart = Some((self.session_full.clone(), self.mode.clone()));
                    self.quit = true;
                }
            }
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
                    "  tab / ↑↓      complete · ctrl+o tools · ctrl+s send · esc interrupt/clear/mode",
                    Tone::Muted,
                );
            }
            "/clear" => self.lines.clear(),
            "/focus" => {
                self.focus = !self.focus;
                self.last_visual_total = 0;
                self.jump_to_bottom();
                self.push(
                    if self.focus {
                        "focus 已开启 · 仅保留对话与错误"
                    } else {
                        "focus 已关闭 · 显示全部工具活动"
                    },
                    Tone::Muted,
                );
            }
            "/new" => {
                self.push("↻ new session …", Tone::Muted);
                self.switch_session(None);
            }
            "/sessions" => self.run_slash("/resume", serve),
            "/back" => self.navigate(false),
            "/forward" => self.navigate(true),
            "/rewind" => {
                let id = serve.command("timeline", vec![]);
                self.pending.push((id, Pending::Timeline));
            }
            "/undo" | "/redo" => {
                let id = serve.command(head.trim_start_matches('/'), vec![]);
                self.pending.push((
                    id,
                    if head == "/undo" {
                        Pending::Undo
                    } else {
                        Pending::Redo
                    },
                ));
            }
            "/fork" => {
                let name = rest.trim().to_string();
                let mut extra = vec![];
                if !name.is_empty() {
                    extra.push(("name", Value::String(name)));
                }
                let id = serve.command("fork", extra);
                self.pending.push((id, Pending::Fork));
            }
            "/send" => {
                let rest = rest.trim();
                if rest.is_empty() {
                    // pick the target first; the message is typed after the id
                    let id = serve.command("sessions", vec![]);
                    self.pending.push((id, Pending::Sessions));
                    self.session_pick = Some(PickerKind::SendTarget);
                } else {
                    let (session, text) = rest
                        .split_once(char::is_whitespace)
                        .unwrap_or((rest, ""));
                    if text.trim().is_empty() {
                        self.push("用法：/send <会话> <消息>", Tone::Muted);
                    } else {
                        let id = serve.command(
                            "send",
                            vec![
                                ("session", Value::String(session.into())),
                                ("text", Value::String(text.trim().into())),
                            ],
                        );
                        self.pending.push((id, Pending::Send));
                        self.push("✉ 发送中 …", Tone::Muted);
                    }
                }
            }
            "/handoff" | "/takeover" => {
                let purpose = if head == "/handoff" {
                    PickerKind::HandoffTarget
                } else {
                    PickerKind::TakeoverSource
                };
                let rest = rest.trim();
                // session ids are hex UUIDs, so "list"/"cancel" never collide with a prefix
                let (sub, sub_arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                if head == "/handoff" && sub == "list" {
                    let id = serve.command("handoff_list", vec![]);
                    self.pending.push((id, Pending::HandoffList));
                } else if head == "/handoff" && sub == "cancel" {
                    let q = sub_arg.trim();
                    if q.is_empty() {
                        self.push(
                            "用法：/handoff cancel <queue id> · /handoff list 查看",
                            Tone::Warning,
                        );
                    } else {
                        let id = serve
                            .command("handoff_cancel", vec![("queueId", Value::String(q.into()))]);
                        self.pending.push((id, Pending::HandoffCancel));
                    }
                } else if rest.is_empty() {
                    // no id: list sessions, the picker sends the command
                    let id = serve.command("sessions", vec![]);
                    self.pending.push((id, Pending::Sessions));
                    self.session_pick = Some(purpose);
                } else {
                    let (session, note) =
                        rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                    self.send_transfer(purpose, session, note.trim(), serve);
                }
            }
            "/branch" => {
                let id = serve.command("branch", vec![]);
                self.pending.push((id, Pending::Branch));
            }
            "/tree" => {
                let id = serve.command("tree", vec![]);
                self.pending.push((id, Pending::Tree));
            }
            "/context" => {
                let id = serve.command("context", vec![]);
                self.pending.push((id, Pending::Context));
            }
            "/history" => self.open_history(),
            "/problems" => {
                // the list of what language servers report for the files tango edited
                self.problems_dialog = !self.problems_dialog;
                self.problems_sel = 0;
                if !self.problems_dialog {
                    self.problems_rows.clear();
                }
            }
            "/todo" => {
                // `/todo <text>` adds a note and shows the whole list; bare toggles it
                let note = rest.trim();
                if !note.is_empty() {
                    if let Some(i) = self.todos.iter().position(|t| matches!(&t.kind, TodoKind::Note { text, done: false } if text == note)) {
                        self.push(format!("已在待办里：{note}"), Tone::Muted);
                        self.todo_sel = i;
                    } else {
                        self.todos.insert(note_slot(&self.todos), TodoItem::note(note));
                        self.todo_sel = note_slot(&self.todos) - 1;
                        self.save_notes();
                        self.push(format!("已记下：{note}"), Tone::Success);
                    }
                    self.todo_dialog = true;
                } else {
                    self.todo_dialog = !self.todo_dialog;
                    self.todo_sel = 0;
                    if !self.todo_dialog {
                        self.todo_buttons.clear();
                    }
                }
            }
            "/queue" => {
                let arg = rest.trim();
                if arg == "clear" {
                    let n = self.local_queue.len();
                    self.local_queue.clear();
                    self.push(format!("已清空 {n} 条排队提示"), Tone::Muted);
                } else if let Some(num) = arg.strip_prefix("rm ") {
                    match num.trim().parse::<usize>() {
                        Ok(idx) if idx >= 1 && idx <= self.local_queue.len() => {
                            let removed = self.local_queue.remove(idx - 1);
                            let head: String = removed
                                .text
                                .lines()
                                .next()
                                .unwrap_or("")
                                .chars()
                                .take(40)
                                .collect();
                            self.push(format!("已移除排队 #{idx}：{head}"), Tone::Muted);
                        }
                        _ => self.push("usage: /queue rm <编号>（/queue 里看编号）", Tone::Muted),
                    }
                } else if arg.starts_with("auto") {
                    match rest.trim().strip_prefix("auto").unwrap_or("").trim() {
                        "append" => {
                            self.queue_auto_append = true;
                            self.push(
                                "queue auto append 已开启：忙碌时回车直接并入当前回合",
                                Tone::Success,
                            );
                        }
                        "off" | "queue" => {
                            self.queue_auto_append = false;
                            self.push("queue auto append 已关闭：忙碌时回车恢复排队", Tone::Muted);
                        }
                        "" => {
                            let state = if self.queue_auto_append {
                                "append（并入当前回合）"
                            } else {
                                "queue（排队）"
                            };
                            self.push(
                                format!(
                                    "忙碌时回车当前行为：{state} · /queue auto append 开启 · /queue auto off 关闭"
                                ),
                                Tone::Muted,
                            );
                        }
                        other => self.push(
                            format!("usage: /queue auto append|off（收到 {other}）"),
                            Tone::Muted,
                        ),
                    }
                } else if arg == "all" {
                    self.queue_supplement_all(serve);
                } else if arg.is_empty() {
                    self.queue_dialog = !self.queue_dialog;
                    self.queue_sel = 0;
                    if !self.queue_dialog {
                        self.queue_buttons.clear();
                    }
                } else {
                    self.push(
                        "usage: /queue · /queue rm <n> · /queue all（全部并入当前） · /queue clear · /queue auto append|off",
                        Tone::Muted,
                    );
                }
            }
            "/config" => {
                let id = serve.command("config_show", vec![]);
                self.pending.push((id, Pending::ConfigShow));
            }
            "/keymap" => {
                let mut text = String::from("当前按键映射（config.json 的 keys 可自定义）：\n\n");
                for (name, key) in self.keys.summary() {
                    text.push_str(&format!("  {name:<14} {key}\n"));
                }
                self.viewer = Some(Viewer::read_only("按键映射", text));
            }
            "/init" => {
                let force = rest.split_whitespace().any(|p| p == "--force");
                let mut extra = vec![];
                if force {
                    extra.push(("force", Value::Bool(true)));
                }
                let id = serve.command("init", extra);
                self.pending.push((id, Pending::Init));
            }
            "/doctor" => {
                let id = serve.command("doctor", vec![]);
                self.pending.push((id, Pending::Doctor));
            }
            "/debug-zip" | "/feedback" => {
                let id = serve.command("debug_zip", vec![]);
                self.pending.push((id, Pending::DebugZip));
            }
            "/usage" | "/stats" => {
                let id = serve.command("usage", vec![]);
                self.pending.push((id, Pending::Usage));
            }
            "/insights" => {
                let id = serve.command("insights", vec![]);
                self.pending.push((id, Pending::Insights));
            }
            "/pwd" => {
                let id = serve.command("pwd", vec![]);
                self.pending.push((id, Pending::Pwd));
            }
            "/cd" => {
                if rest.is_empty() {
                    let id = serve.command("cwds", vec![]);
                    self.pending.push((id, Pending::Cwds));
                } else {
                    self.cd_to(rest.trim(), serve);
                }
            }
            "/add_dir" => {
                if rest.is_empty() {
                    self.push("usage: /add_dir <dir>", Tone::Muted);
                } else {
                    let id =
                        serve.command("add_dir", vec![("dir", Value::String(rest.trim().into()))]);
                    self.pending.push((id, Pending::AddDir));
                }
            }
            "/effort" => {
                let arg = rest.trim();
                if arg.is_empty() {
                    let id = serve.command("status", vec![]);
                    self.pending.push((id, Pending::Effort));
                } else {
                    let id =
                        serve.command("set_effort", vec![("level", Value::String(arg.into()))]);
                    self.pending.push((id, Pending::Effort));
                }
            }
            "/bg" => {
                if rest.is_empty() {
                    self.push("usage: /bg <任务描述>", Tone::Muted);
                } else {
                    let long = rest.starts_with("--long");
                    let prompt = if long {
                        rest.trim_start_matches("--long").trim()
                    } else {
                        rest.trim()
                    };
                    let mut extra = vec![("prompt", Value::String(prompt.into()))];
                    if long {
                        extra.push(("long", Value::Bool(true)));
                    }
                    let id = serve.command("bg", extra);
                    self.pending.push((id, Pending::Bg));
                }
            }
            "/agents" => {
                if rest.is_empty() {
                    let id = serve.command("agents_show", vec![]);
                    self.pending.push((id, Pending::AgentsShow));
                } else if let Some((role, model)) = rest.split_once(' ') {
                    let id = serve.command(
                        "agents_set",
                        vec![
                            ("role", Value::String(role.trim().into())),
                            ("model", Value::String(model.trim().into())),
                        ],
                    );
                    self.pending.push((id, Pending::AgentsSet));
                } else {
                    self.push(
                        "usage: /agents [driver|subagent|longContext|plan] <model>",
                        Tone::Muted,
                    );
                }
            }
            "/subagent" | "/subtask" => {
                if rest.is_empty() {
                    self.push(
                        "usage: /subagent <小任务描述> · 完成后摘要自动写回",
                        Tone::Muted,
                    );
                } else {
                    let id = serve.command("subtask", vec![("prompt", Value::String(rest.into()))]);
                    self.pending.push((id, Pending::Subtask));
                }
            }
            "/plugins" => {
                let id = serve.command("plugins_list", vec![]);
                self.pending.push((id, Pending::PluginsList));
            }
            "/mcp" => {
                // `/mcp add <name>`: a catalog server into the global config
                if let Some(name) = rest.trim().strip_prefix("add ") {
                    let id = serve.command("mcp_add", vec![("name", Value::String(name.trim().into()))]);
                    self.pending.push((id, Pending::McpAdd));
                } else if rest.trim() == "add" {
                    self.push("usage: /mcp add <名字>（/mcp 里列出可添加的）", Tone::Muted);
                } else {
                    let id = serve.command("mcp_list", vec![]);
                    self.pending.push((id, Pending::McpList));
                }
            }
            "/tasks" => {
                let id = serve.command("tasks", vec![]);
                self.pending.push((id, Pending::Tasks));
            }
            "/stop" => {
                if rest.is_empty() {
                    self.push("usage: /stop <任务id或名字>（/tasks 里看）", Tone::Muted);
                } else {
                    let id =
                        serve.command("stop_task", vec![("id", Value::String(rest.trim().into()))]);
                    self.pending.push((id, Pending::StopTask));
                }
            }
            "/worktree" => {
                let arg = rest.trim();
                // a bare subcommand is a typo in progress, not "list everything":
                // say what's missing instead of popping the worktree picker
                if matches!(arg, "new" | "rm" | "remove" | "merge") {
                    let what = match arg {
                        "new" => "new <名字>",
                        "merge" => "merge <名字>（主工作区需干净）",
                        _ => "rm <名字>",
                    };
                    self.push(
                        format!("usage: /worktree {what} · /worktree 看列表"),
                        Tone::Muted,
                    );
                } else if let Some(name) = arg.strip_prefix("new ") {
                    let id = serve.command(
                        "worktree_create",
                        vec![("name", Value::String(name.trim().into()))],
                    );
                    self.pending.push((id, Pending::Worktree));
                } else if let Some(name) = arg
                    .strip_prefix("rm ")
                    .or_else(|| arg.strip_prefix("remove "))
                {
                    let id = serve.command(
                        "worktree_remove",
                        vec![("name", Value::String(name.trim().into()))],
                    );
                    self.pending.push((id, Pending::Worktree));
                } else if let Some(name) = arg.strip_prefix("merge ") {
                    let id = serve.command(
                        "worktree_merge",
                        vec![("name", Value::String(name.trim().into()))],
                    );
                    self.pending.push((id, Pending::Worktree));
                } else {
                    let id = serve.command("worktree_list", vec![]);
                    self.pending.push((id, Pending::Worktree));
                }
            }
            "/loop" => {
                if rest.trim() == "off" {
                    let id = serve.command("loop", vec![("off", Value::Bool(true))]);
                    self.pending.push((id, Pending::Loop));
                } else if let Some((interval, prompt)) = rest.split_once(' ') {
                    let secs = match interval.trim().chars().last() {
                        Some('m') => interval
                            .trim()
                            .trim_end_matches('m')
                            .parse::<f64>()
                            .map(|n| n * 60.0)
                            .ok(),
                        Some('h') => interval
                            .trim()
                            .trim_end_matches('h')
                            .parse::<f64>()
                            .map(|n| n * 3600.0)
                            .ok(),
                        Some('s') => interval.trim().trim_end_matches('s').parse::<f64>().ok(),
                        _ => interval.trim().parse::<f64>().ok(),
                    };
                    match secs {
                        Some(secs) if secs >= 10.0 && !prompt.trim().is_empty() => {
                            let id = serve.command(
                                "loop",
                                vec![
                                    ("everySec", Value::from(secs as u64)),
                                    ("prompt", Value::String(prompt.trim().into())),
                                ],
                            );
                            self.pending.push((id, Pending::Loop));
                        }
                        _ => self.push(
                            "usage: /loop <10s|5m|1h> <prompt> · 间隔下限 10 秒",
                            Tone::Muted,
                        ),
                    }
                } else {
                    self.push("usage: /loop <10s|5m|1h> <prompt> · /loop off", Tone::Muted);
                }
            }
            "/goal" => {
                let id = serve.command("goal", vec![("text", Value::String(rest.trim().into()))]);
                self.pending.push((id, Pending::Goal));
            }
            "/skills" => {
                let id = serve.command("skills", vec![]);
                self.pending.push((id, Pending::Skills));
            }
            "/memory" => {
                let id = serve.command("memory", vec![]);
                self.pending.push((id, Pending::Memory));
            }
            "/remember" => {
                if rest.is_empty() {
                    self.push("usage: /remember <一句话>", Tone::Muted);
                } else {
                    let id = serve.command("remember", vec![("text", Value::String(rest.into()))]);
                    self.pending.push((id, Pending::Remember));
                }
            }
            // preset prompts (grok's /commit-push, /commit-pr): the model runs git with its
            // usual tools, so the permission mode still gates `git push`
            "/commit-push" => {
                self.submit_text(serve, COMMIT_PUSH_PROMPT.to_string(), vec![]);
            }
            "/commit-pr" => {
                self.submit_text(serve, COMMIT_PR_PROMPT.to_string(), vec![]);
            }
            "/review" | "/security-review" => {
                let cmd = if head == "/review" {
                    "review"
                } else {
                    "security_review"
                };
                let id = serve.command(cmd, vec![]);
                // the result lands like a prompt result: same busy lifecycle
                self.prompt_id = id;
                self.busy = true;
                self.push("评审进行中 …", Tone::Muted);
            }
            "/plan" => {
                let on = !self.plan_mode;
                let id = serve.command("set_plan", vec![("on", Value::Bool(on))]);
                self.pending.push((id, Pending::Plan));
            }
            "/permissions" => {
                let arg = rest.trim();
                if let Some(rule) = arg.strip_prefix("add ") {
                    let id = serve.command(
                        "permissions_add",
                        vec![("rule", Value::String(rule.trim().into()))],
                    );
                    self.pending.push((id, Pending::PermissionsAdd));
                } else if let Some(rule) = arg
                    .strip_prefix("rm ")
                    .or_else(|| arg.strip_prefix("remove "))
                {
                    let id = serve.command(
                        "permissions_remove",
                        vec![("rule", Value::String(rule.trim().into()))],
                    );
                    self.pending.push((id, Pending::PermissionsRemove));
                } else {
                    let id = serve.command("permissions_list", vec![]);
                    self.pending.push((id, Pending::PermissionsList));
                }
            }
            "/import" => {
                let mut parts = rest.split_whitespace();
                match parts.next() {
                    Some(source @ ("claude-code" | "grok" | "codex")) => {
                        let mut extra = vec![("source", Value::String(source.into()))];
                        // codex only: optional config.toml path (default ~/.codex/config.toml)
                        if let (Some(path), "codex") = (parts.next(), source) {
                            extra.push(("path", Value::String(path.into())));
                        }
                        let id = serve.command("import", extra);
                        self.pending.push((id, Pending::Import));
                    }
                    _ => self.push(
                        "usage: /import claude-code|grok|codex [config.toml]",
                        Tone::Muted,
                    ),
                }
            }
            "/preset" => {
                let arg = rest.trim();
                if rest.trim() == "save" || rest.trim() == "save " {
                    self.push(
                        "usage: /preset save <name>（保存当前输入框内容）",
                        Tone::Muted,
                    );
                } else if let Some(name) = arg.strip_prefix("save ") {
                    let text: String = self.input.iter().collect();
                    if text.trim().is_empty() {
                        self.push("输入框为空 — 先写好再 /preset save", Tone::Muted);
                    } else {
                        let id = serve.command(
                            "preset_save",
                            vec![
                                ("name", Value::String(name.trim().into())),
                                ("text", Value::String(text)),
                            ],
                        );
                        self.pending.push((id, Pending::PresetSave));
                        self.input.clear();
                        self.cursor = 0;
                    }
                } else if arg.is_empty() {
                    let id = serve.command("preset_list", vec![]);
                    self.pending.push((id, Pending::PresetList));
                } else {
                    let id = serve.command("preset_get", vec![("name", Value::String(arg.into()))]);
                    self.pending.push((id, Pending::PresetGet));
                }
            }
            "/statusline" => {
                if rest.is_empty() {
                    let cur = self.statusline.clone().unwrap_or_else(|| "（默认）".into());
                    self.push(
                        format!(
                            "当前状态栏模板：{cur}\n可用占位符：{{model}} {{mode}} {{cost}} {{session}} {{tokens}} {{ctx}} {{pct}} {{work}} · /statusline off 恢复默认"
                        ),
                        Tone::Muted,
                    );
                } else if rest.trim() == "off" {
                    let id = serve.command("set_statusline", vec![("template", Value::Null)]);
                    self.pending.push((id, Pending::Statusline));
                } else {
                    let id = serve.command(
                        "set_statusline",
                        vec![("template", Value::String(rest.trim().into()))],
                    );
                    self.pending.push((id, Pending::Statusline));
                }
            }
            "/autocompact" => {
                if rest.is_empty() {
                    let id = serve.command("status", vec![]);
                    self.pending.push((id, Pending::AutocompactShow));
                } else {
                    let id = serve.command(
                        "set_autocompact",
                        vec![("value", Value::String(rest.trim().into()))],
                    );
                    self.pending.push((id, Pending::AutocompactSet));
                }
            }
            "/timestamps" => {
                self.timestamps = !self.timestamps;
                self.push(
                    format!("时间戳 {}", if self.timestamps { "开" } else { "关" }),
                    Tone::Muted,
                );
            }
            "/density" => {
                let arg = rest.trim();
                self.density = if arg == "0" || arg == "1" || arg == "2" {
                    arg.parse().unwrap_or(1)
                } else {
                    (self.density + 1) % 3
                };
                let name = match self.density {
                    0 => "0 紧凑（仅工具名）",
                    2 => "2 完整（成功工具也显示输出）",
                    _ => "1 默认（每次调用一行，出错的也是）",
                };
                self.push(format!("工具详情密度 → {name}"), Tone::Muted);
            }
            "/find" => {
                if rest.is_empty() {
                    self.push("usage: /find <text>", Tone::Muted);
                } else {
                    let id = serve.command("find", vec![("text", Value::String(rest.into()))]);
                    self.pending.push((id, Pending::Find));
                }
            }
            "/export" => {
                let format = if rest.trim() == "html" { "html" } else { "md" };
                let id = serve.command("export", vec![("format", Value::String(format.into()))]);
                self.pending.push((id, Pending::Export));
            }
            "/transcript" => {
                let id = serve.command("transcript", vec![]);
                self.pending.push((id, Pending::Transcript));
            }
            "/editlast" => {
                // same two-press rule as alt+e: the rewind is code-destructive
                if self.editlast_armed {
                    self.editlast_armed = false;
                    let id = serve.command("editlast", vec![]);
                    self.pending.push((id, Pending::EditLast));
                    self.push("⟲ 打断并回滚上一句中 …", Tone::Muted);
                } else {
                    self.editlast_armed = true;
                    self.push(
                        "⚠ 将打断当前回合并回滚对话与代码到上一句之前 · 再按 alt+e 或 /editlast 执行 · esc 取消"
                            .to_string(),
                        Tone::Warning,
                    );
                }
            }
            "/rename" | "/name" => {
                if rest.is_empty() {
                    let shown = if self.session_name.is_empty() {
                        self.session.clone()
                    } else {
                        self.session_name.clone()
                    };
                    self.push(
                        format!("当前会话：{shown} · /name <新名字> 重命名（状态栏即显示）"),
                        Tone::Muted,
                    );
                } else {
                    let id = serve.command("rename", vec![("name", Value::String(rest.into()))]);
                    self.session_name = rest.trim().chars().take(32).collect();
                    self.push(
                        format!("✓ 会话已重命名：{}", self.session_name),
                        Tone::Success,
                    );
                    self.pending.push((id, Pending::Rename));
                }
            }
            "/recap" => {
                let id = serve.command("recap", vec![]);
                self.pending.push((id, Pending::Recap));
                self.btw_id = Some(id);
                let title = "一行总结".to_string();
                self.btw_title = Some(title.clone());
                self.viewer = Some(Viewer::read_only(title, ""));
            }
            "/copy" => {
                let n: usize = rest.trim().parse().unwrap_or(1);
                let want = n.saturating_sub(1);
                let pick = self
                    .assistant_replies
                    .len()
                    .checked_sub(want)
                    .filter(|left| *left > 0)
                    .and_then(|left| self.assistant_replies.get(left - 1));
                if let Some(text) = pick {
                    let text = text.clone();
                    match self.copy_to_clipboard(&text) {
                        Ok(()) => self.toast(format!("已复制助手回复（{n}）")),
                        Err(error) => self.push(format!("复制失败：{error}"), Tone::Error),
                    }
                } else {
                    self.push("没有可复制的助手回复", Tone::Muted);
                }
            }
            "/cost" => {
                let id = serve.command("status", vec![]);
                self.pending.push((id, Pending::Status));
            }
            "/diff" => {
                let staged = rest.split_whitespace().any(|part| part == "--staged");
                let mut paths: Vec<Value> = rest
                    .split_whitespace()
                    .filter(|part| *part != "--staged")
                    .map(|part| Value::String(part.into()))
                    .collect();
                if paths.is_empty() {
                    paths.push(Value::String(".".into()));
                }
                let id = serve.command(
                    "diff",
                    vec![
                        ("staged", Value::Bool(staged)),
                        ("paths", Value::Array(paths)),
                    ],
                );
                self.pending.push((id, Pending::Diff));
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
            "/trust" => match rest {
                "" => {
                    let id = serve.command("trust_status", vec![]);
                    self.pending.push((id, Pending::TrustStatus));
                }
                "yes" | "no" => {
                    let id =
                        serve.command("set_trust", vec![("trusted", Value::Bool(rest == "yes"))]);
                    self.pending.push((id, Pending::Trust));
                }
                _ => self.push("usage: /trust [yes|no]", Tone::Muted),
            },
            "/theme" => {
                if rest.is_empty() {
                    let items = ["dark", "light", "auto"]
                        .into_iter()
                        .map(|theme| PickItem {
                            group: String::new(),
                            id: theme.into(),
                            label: theme.into(),
                            note: match theme {
                                "dark" => "深色背景",
                                "light" => "浅色背景",
                                _ => "跟随终端背景色（OSC 11，退回 COLORFGBG）",
                            }
                            .into(),
                            warn: false,
                            current: false,
                        })
                        .collect();
                    self.picker = Some(Picker::new(PickerKind::Theme, items));
                } else if matches!(rest, "dark" | "light" | "auto") {
                    let id =
                        serve.command("set_theme", vec![("theme", Value::String(rest.into()))]);
                    self.pending.push((id, Pending::Theme));
                } else {
                    self.push("usage: /theme [dark|light|auto]", Tone::Muted);
                }
            }
            "/mouse" => {
                let enabled = match rest {
                    "" => !self.mouse,
                    "on" => true,
                    "off" => false,
                    _ => {
                        self.push("usage: /mouse [on|off]", Tone::Muted);
                        return;
                    }
                };
                let id = serve.command("set_mouse", vec![("mouse", Value::Bool(enabled))]);
                self.pending.push((id, Pending::Mouse));
                self.mouse_capture = Some(enabled);
            }
            "/bubble" => {
                let enabled = match rest {
                    "" => !self.user_bubble,
                    "on" => true,
                    "off" => false,
                    _ => {
                        self.push(
                            "usage: /bubble [on|off]（on=靠右气泡 off=全宽靠左）",
                            Tone::Muted,
                        );
                        return;
                    }
                };
                self.user_bubble = enabled;
                // serve persists it; the chat line below is the only feedback needed
                let _ = serve.command("set_bubble", vec![("bubble", Value::Bool(enabled))]);
                self.push(
                    if enabled {
                        "用户消息：靠右气泡"
                    } else {
                        "用户消息：全宽靠左（Claude Code 式）"
                    }
                    .to_string(),
                    Tone::Muted,
                );
            }
            "/animations" => {
                let enabled = match rest {
                    "" => !self.animations,
                    "on" => true,
                    "off" => false,
                    _ => {
                        self.push("usage: /animations [on|off]", Tone::Muted);
                        return;
                    }
                };
                self.animations = enabled;
                // serve persists it, like /bubble
                let _ = serve.command("set_animations", vec![("animations", Value::Bool(enabled))]);
                self.push(
                    if enabled { "动画：开" } else { "动画：关（静态颜色）" }.to_string(),
                    Tone::Muted,
                );
            }
            "/clipboard" => {
                use crate::clipboard::{Mode, mode, set_mode};
                if rest.is_empty() {
                    self.push(
                        format!(
                            "剪贴板：{}（auto：SSH 下走 OSC 52，本机用系统命令，失败再退到 OSC 52）· /clipboard auto|osc52|native",
                            mode().name()
                        ),
                        Tone::Muted,
                    );
                    return;
                }
                let Some(m) = Mode::parse(rest) else {
                    self.push("usage: /clipboard [auto|osc52|native]", Tone::Muted);
                    return;
                };
                set_mode(m);
                let _ = serve.command("set_clipboard", vec![("clipboard", Value::String(m.name().into()))]);
                self.push(format!("剪贴板：{}", m.name()), Tone::Muted);
            }
            "/ascii" => {
                let enabled = match rest {
                    "" => !self.ascii,
                    "on" => true,
                    "off" => false,
                    _ => {
                        self.push("usage: /ascii [on|off]", Tone::Muted);
                        return;
                    }
                };
                self.ascii = enabled;
                // serve persists it, like /animations
                let _ = serve.command("set_ascii", vec![("ascii", Value::Bool(enabled))]);
                self.push(
                    if enabled {
                        "ASCII 模式：开（emoji 与思考图标用 * ? + 等代替）"
                    } else {
                        "ASCII 模式：关"
                    }
                    .to_string(),
                    Tone::Muted,
                );
            }
            "/vim" => {
                let enabled = match rest {
                    "" => !self.vim,
                    "on" => true,
                    "off" => false,
                    _ => {
                        self.push("usage: /vim [on|off]", Tone::Muted);
                        return;
                    }
                };
                self.set_vim(enabled);
                // the toggle lasts for the whole TUI process; a serve respawn must not
                // re-apply the persisted config's editor over the user's choice
                self.vim_override = Some(enabled);
            }
            "/nvim" => self.edit_draft(),
            "/editor" => self.edit_draft_generic(),
            "/multiline" => {
                self.multiline_enter = !self.multiline_enter;
                self.push(
                    format!(
                        "Enter {} · ctrl+s 始终发送",
                        if self.multiline_enter {
                            "换行（多行模式）"
                        } else {
                            "发送（默认）"
                        }
                    ),
                    Tone::Muted,
                );
            }
            // /auto is short for /mode auto (same acknowledgement dialog)
            "/auto" => {
                if rest.is_empty() {
                    if self.mode == "auto" {
                        self.push("已经是完全放行（auto）· /mode ask 恢复确认", Tone::Muted);
                    } else {
                        self.full_access = Some(false);
                    }
                } else {
                    self.push("usage: /auto（不带参数）", Tone::Muted);
                }
            }
            "/mode" => {
                if rest.is_empty() {
                    self.open_mode_picker();
                } else if rest == "auto" {
                    // typed or picked: full access always goes through the acknowledgement
                    self.full_access = Some(false);
                } else {
                    let id = serve.command("set_mode", vec![("mode", Value::String(rest.into()))]);
                    self.pending.push((id, Pending::Mode));
                }
            }
            "/btw" => {
                if rest.is_empty() {
                    self.push("usage: /btw <question>", Tone::Muted);
                } else {
                    let id = serve.command("btw", vec![("text", Value::String(rest.into()))]);
                    self.pending.push((id, Pending::Btw));
                    self.btw_id = Some(id);
                    let title = format!("临时提问 · {}", rest.chars().take(40).collect::<String>());
                    self.btw_title = Some(title.clone());
                    self.viewer = Some(Viewer::read_only(title, ""));
                    self.busy = true; // ctrl+c must be able to abort a slow side question
                }
            }
            "/compact" => {
                let id = serve.command("compact", vec![]);
                self.pending.push((id, Pending::Compact));
            }
            "/resume" => {
                let target = rest.trim();
                if target.is_empty() {
                    let id = serve.command("sessions", vec![]);
                    self.pending.push((id, Pending::Sessions));
                } else {
                    // a typed id/prefix: resolve it before tearing down this serve, so a
                    // miss or an ambiguous prefix is an error line instead of a dead child
                    let id = serve.command(
                        "session_resolve",
                        vec![("session", Value::String(target.to_string()))],
                    );
                    self.pending.push((id, Pending::ResumeResolve));
                }
            }
            "/status" => {
                let id = serve.command("status", vec![]);
                self.pending.push((id, Pending::Status));
            }
            other if other.starts_with('/') && other.chars().skip(1).any(|c| c == '/') => {
                // looks like a path, but nothing exists there
                self.push(
                    format!("找不到文件或目录：{} · 命令见 /help", trimmed),
                    Tone::Error,
                );
            }
            other => {
                self.push(format!("unknown command {other} · /help"), Tone::Error);
            }
        }
    }

    // ---------- @file expansion ----------

    fn at_line_range(raw: &str) -> Option<(String, (usize, usize))> {
        let (path, spec) = raw.rsplit_once('#')?;
        if path.is_empty() {
            return None;
        }
        let spec = spec.strip_prefix('L').unwrap_or(spec);
        let (start, end) = spec.split_once('-').unwrap_or((spec, spec));
        let end = end.strip_prefix('L').unwrap_or(end);
        if start.is_empty()
            || end.is_empty()
            || !start.bytes().all(|b| b.is_ascii_digit())
            || !end.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let start = start.parse::<usize>().ok()?;
        let end = end.parse::<usize>().ok()?;
        (start > 0 && end >= start).then(|| (path.to_string(), (start, end)))
    }

    fn expand_at(text: &str, base: Option<&Path>) -> (String, Vec<String>) {
        let chars: Vec<char> = text.chars().collect();
        let mut hits = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let boundary = i == 0 || chars[i - 1].is_whitespace();
            if boundary && chars[i] == '@' {
                let mut j = i + 1;
                while j < chars.len() && !chars[j].is_whitespace() {
                    j += 1;
                }
                let raw: String = chars[i + 1..j].iter().collect();
                let token = raw.trim_end_matches(|c: char| ")]}.,;:!?\"'".contains(c));
                let resolve = |p: &str| -> String {
                    let pp = Path::new(p);
                    if pp.is_absolute() || base.is_none() {
                        p.to_string()
                    } else {
                        base.unwrap().join(pp).to_string_lossy().into_owned()
                    }
                };
                let (path, range) = if Path::new(token).is_file() || Path::new(token).is_dir() {
                    (resolve(token), None)
                } else if let Some((path, range)) = Self::at_line_range(token) {
                    (resolve(&path), Some(range))
                } else {
                    (resolve(token), None)
                };
                let p = Path::new(&path);
                if !path.is_empty() && (p.is_file() || (p.is_dir() && range.is_none())) {
                    hits.push((i, j, token.to_string(), path, range));
                }
                i = j.max(i + 1);
                continue;
            }
            i += 1;
        }
        let mut warns = Vec::new();
        let mut out = String::new();
        let mut pos = 0;
        for (start, end, token, path, range) in hits {
            out.extend(chars[pos..start].iter());
            pos = end;
            if Path::new(&path).is_dir() {
                out.push_str(&format!("{path}/:\n{}", Self::dir_listing(&path)));
                continue;
            }
            match fs::read_to_string(&path) {
                Ok(body) => {
                    let selected = if let Some((first, last)) = range {
                        let lines: Vec<&str> = body.lines().collect();
                        if first > lines.len() {
                            warns.push(format!(
                                "@{token}: line {first} exceeds file length {}",
                                lines.len()
                            ));
                            out.extend(chars[start..end].iter());
                            continue;
                        }
                        lines[first - 1..last.min(lines.len())].join("\n")
                    } else {
                        body
                    };
                    let clipped: String = selected.chars().take(AT_FILE_CAP).collect();
                    let tail = if selected.chars().count() > AT_FILE_CAP {
                        "\n…[truncated]"
                    } else {
                        ""
                    };
                    out.push_str(&format!("{token}:\n{clipped}{tail}"));
                }
                Err(e) => {
                    warns.push(format!("@{token}: {e}"));
                    out.extend(chars[start..end].iter());
                }
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

    fn replace_input_range(&mut self, start: usize, end: usize, replacement: &[char]) {
        // an edit that touches part of a collapsed paste consumes the whole block,
        // so pasted spans stay atomic instead of silently expanding into raw lines
        let mut start = start;
        let mut end = end;
        while let Some(index) = self
            .pastes
            .iter()
            .position(|paste| paste.start < end && paste.end > start)
        {
            let paste = self.pastes.remove(index);
            start = start.min(paste.start);
            end = end.max(paste.end);
        }
        let old_len = end - start;
        let new_len = replacement.len();
        self.pastes.retain_mut(|paste| {
            if paste.end <= start {
                return true;
            }
            if paste.start >= end {
                if new_len >= old_len {
                    let delta = new_len - old_len;
                    paste.start += delta;
                    paste.end += delta;
                } else {
                    let delta = old_len - new_len;
                    paste.start -= delta;
                    paste.end -= delta;
                }
                return true;
            }
            false
        });
        self.input.splice(start..end, replacement.iter().copied());
        if self.cursor >= end {
            if new_len >= old_len {
                self.cursor += new_len - old_len;
            } else {
                self.cursor -= old_len - new_len;
            }
        } else if self.cursor > start {
            self.cursor = start + new_len;
        }
    }

    fn insert_char(&mut self, c: char) {
        // a separator (space/newline) after a just-typed existing-image path collapses
        // that token into an attachment chip — covers terminals without bracketed paste
        if (c == ' ' || c == '\n')
            && let Some(cut) = self.typed_image_token_start()
        {
            let path: String = self.input[cut..self.cursor].iter().collect();
            self.insert_image_paste_at(path, cut);
            self.insert_char(c);
            return;
        }
        let cursor = self.cursor;
        self.replace_input_range(cursor, cursor, &[c]);
    }

    fn insert_paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        // a pasted image (the terminal drops clipboard images to a temp file and pastes
        // its path) becomes a clickable chip, not a wall of path text
        if let Some(path) = image_path_from_paste(&text) {
            self.insert_image_paste(path);
            return;
        }
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            return;
        }
        if !text.contains('\n') && chars.len() <= 200 {
            let cursor = self.cursor;
            self.replace_input_range(cursor, cursor, &chars);
            return;
        }
        let start = self.cursor;
        let end = start + chars.len();
        let extra_lines = text.lines().count().saturating_sub(1);
        self.replace_input_range(start, start, &chars);
        if chars.last() != Some(&'\n') {
            self.insert_char('\n');
        }
        let hidden_end = if chars.last() == Some(&'\n') {
            end - 1
        } else {
            end
        };
        self.pastes.push(PastedSpan {
            start,
            end: hidden_end,
            id: self.next_paste,
            extra_lines,
            image: None,
        });
        self.next_paste += 1;
    }

    /// Attach a pasted image: the raw input keeps the path (so copy/edit see real text),
    /// but the span renders as a `[图片 #N]` chip and rides the next prompt as an upload.
    /// Char index where an existing-image-path token ends at the cursor begins,
    /// or None when the text right before the cursor isn't such a token.
    fn typed_image_token_start(&self) -> Option<usize> {
        // right after a chip span the "token" is already collapsed text — recursing
        // here would re-chip the span forever (stack overflow)
        if self.pastes.iter().any(|p| p.end == self.cursor) {
            return None;
        }
        let mut start = self.cursor;
        while start > 0 && !self.input[start - 1].is_whitespace() {
            start -= 1;
        }
        if start == self.cursor {
            return None;
        }
        let token: String = self.input[start..self.cursor].iter().collect();
        image_path_from_paste(&token).map(|_| start)
    }

    /// insert_image_paste, but inserting at an arbitrary position (typed-token collapse)
    fn insert_image_paste_at(&mut self, path: String, at: usize) {
        let id = self.next_image;
        self.next_image += 1;
        self.images.push(ImageAttachment {
            id,
            path: path.clone(),
        });
        let chars: Vec<char> = path.chars().collect();
        let end = at + chars.len();
        self.replace_input_range(at, end, &chars);
        self.pastes.push(PastedSpan {
            start: at,
            end,
            id: self.next_paste,
            extra_lines: 0,
            image: Some(id),
        });
        self.next_paste += 1;
        self.cursor = end;
        self.push(
            format!("🖼 已附加图片 #{id} · 双击查看 · 随下一条消息发送"),
            Tone::Muted,
        );
    }

    fn insert_image_paste(&mut self, path: String) {
        let id = self.next_image;
        self.next_image += 1;
        self.images.push(ImageAttachment {
            id,
            path: path.clone(),
        });
        let chars: Vec<char> = path.chars().collect();
        let start = self.cursor;
        self.replace_input_range(start, start, &chars);
        let end = start + chars.len();
        self.pastes.push(PastedSpan {
            start,
            end,
            id: self.next_paste,
            extra_lines: 0,
            image: Some(id),
        });
        self.next_paste += 1;
        self.push(
            format!("🖼 已附加图片 #{id} · 双击查看 · 随下一条消息发送"),
            Tone::Muted,
        );
    }

    /// Build the outgoing prompt from the input: image chips become `[图片 #N]` markers
    /// in the text and their paths collect into the upload list (in chip order).
    fn take_submission(&self) -> (String, Vec<String>) {
        let mut pastes = self.pastes.clone();
        pastes.sort_by_key(|p| p.start);
        let mut text = String::new();
        let mut images = Vec::new();
        let mut raw = 0;
        for p in pastes {
            while raw < p.start {
                text.push(self.input[raw]);
                raw += 1;
            }
            if let Some(id) = p.image {
                text.push_str(&format!("[图片 #{id}]"));
                if let Some(img) = self.images.iter().find(|i| i.id == id) {
                    images.push(img.path.clone());
                }
            } else {
                while raw < p.end {
                    text.push(self.input[raw]);
                    raw += 1;
                }
            }
            raw = p.end;
        }
        while raw < self.input.len() {
            text.push(self.input[raw]);
            raw += 1;
        }
        // last-resort sweep: bare existing-image tokens that never hit a chip path
        // (typed without a trailing separator, odd terminals) still upload — the model
        // should never see a raw local path for an image that exists
        let mut swept = String::new();
        for token in text.split_inclusive(|c: char| c == ' ' || c == '\n') {
            let trimmed = token.trim_end();
            if let Some(path) = image_path_from_paste(trimmed)
                && !images.contains(&path)
            {
                let sep = &token[trimmed.len()..];
                swept.push_str(&format!("[图片 {}]{sep}", images.len() + 1));
                images.push(path);
            } else {
                swept.push_str(token);
            }
        }
        // a leading file path is a prompt about that file, never a slash command
        if let Some(prompt) = file_prompt(&swept) {
            swept = prompt;
        }
        (swept, images)
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.pastes.clear();
        self.cursor = 0;
        self.popup = None;
        self.history_nav = None;
        self.history_draft = None;
    }

    fn backspace_input(&mut self) {
        if let Some(paste) = self
            .pastes
            .iter()
            .find(|paste| paste.end == self.cursor)
            .copied()
        {
            self.replace_input_range(paste.start, paste.end, &[]);
        } else if self.cursor > 0 {
            let cursor = self.cursor;
            self.replace_input_range(cursor - 1, cursor, &[]);
        }
    }

    fn delete_input(&mut self) {
        if let Some(paste) = self
            .pastes
            .iter()
            .find(|paste| paste.start == self.cursor)
            .copied()
        {
            self.replace_input_range(paste.start, paste.end, &[]);
        } else if self.cursor < self.input.len() {
            let cursor = self.cursor;
            self.replace_input_range(cursor, cursor + 1, &[]);
        }
    }

    fn move_input_cursor(&mut self, left: bool) {
        self.cursor = if left {
            self.pastes
                .iter()
                .find(|paste| paste.end == self.cursor)
                .map_or(self.cursor.saturating_sub(1), |paste| paste.start)
        } else {
            self.pastes
                .iter()
                .find(|paste| paste.start == self.cursor)
                .map_or(min(self.cursor + 1, self.input.len()), |paste| paste.end)
        };
    }

    fn rendered_input(&self) -> RenderedInput {
        let mut pastes = self.pastes.clone();
        pastes.sort_by_key(|paste| paste.start);
        let mut text = String::new();
        let mut positions = vec![0; self.input.len() + 1];
        let mut image_ranges = Vec::new();
        let mut display_raw = Vec::new();
        let mut raw = 0;
        let mut display = 0;
        for paste in pastes {
            while raw < paste.start {
                positions[raw] = display;
                display_raw.push((raw, raw + 1));
                text.push(self.input[raw]);
                raw += 1;
                display += 1;
            }
            positions[raw] = display;
            let label = if let Some(id) = paste.image {
                format!("[图片 #{id}]")
            } else if paste.extra_lines > 0 {
                format!("[Pasted text #{} +{} lines]", paste.id, paste.extra_lines)
            } else {
                format!("[Pasted text #{}]", paste.id)
            };
            let label_start = display;
            display += label.chars().count();
            for _ in label.chars() {
                display_raw.push((paste.start, paste.end));
            }
            text.push_str(&label);
            if let Some(id) = paste.image {
                image_ranges.push((label_start, display, id));
            }
            while raw < paste.end {
                positions[raw] = display;
                raw += 1;
            }
            positions[raw] = display;
        }
        while raw < self.input.len() {
            positions[raw] = display;
            display_raw.push((raw, raw + 1));
            text.push(self.input[raw]);
            raw += 1;
            display += 1;
        }
        positions[self.input.len()] = display;
        RenderedInput {
            text,
            cursor: positions[self.cursor],
            image_ranges,
            display_raw,
        }
    }

    fn input_layout(&self, width: u16) -> InputLayout {
        let rendered = self.rendered_input();
        let width = width.max(1) as usize;
        let mut lines = vec![String::new()];
        let mut starts = vec![0usize];
        let mut row = 0;
        let mut col = 0;
        let mut cursor = None;
        for (index, c) in rendered.text.chars().enumerate() {
            if c == '\n' {
                starts.push(index + 1);
                if index == rendered.cursor {
                    cursor = Some(if col >= width {
                        (row + 1, 0)
                    } else {
                        (row, col)
                    });
                }
                lines.push(String::new());
                row += 1;
                col = 0;
                continue;
            }
            let char_width = display_width(&c.to_string());
            if col >= width || (col > 0 && col + char_width > width) {
                lines.push(String::new());
                starts.push(index);
                row += 1;
                col = 0;
            }
            if index == rendered.cursor {
                cursor = Some((row, col));
            }
            lines[row].push(c);
            col += char_width;
        }
        let total = rendered.text.chars().count();
        if rendered.cursor == total {
            if col >= width {
                lines.push(String::new());
                starts.push(total);
                row += 1;
                col = 0;
            }
            cursor = Some((row, col));
        }
        let (mut cursor_row, mut cursor_col) = cursor.unwrap_or((row, col));
        if let Some(command) = &self.vim_command {
            let command_line = format!(":{command}");
            cursor_col = display_width(&command_line);
            lines.push(command_line);
            starts.push(usize::MAX); // never part of the input text
            cursor_row = lines.len() - 1;
        }
        InputLayout {
            lines,
            starts,
            cursor_row,
            cursor_col,
        }
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
            // 1s heartbeat: nothing to handle, the main loop just redraws
            Event::Tick => {}
            Event::Resized(_, _) => self.last_visual_total = 0,
            Event::Paste(s) => {
                if self.permission.is_none()
                    && self.picker.is_none()
                    && self.viewer.is_none()
                    && (!self.vim || self.edit_mode == EditMode::Insert)
                {
                    self.insert_paste(&s);
                    self.refresh_popup();
                }
            }
            Event::Mouse(m) => {
                // a dialog's hint key ("y 允许", "esc 关闭" …) is a button: the click is that
                // key press, routed exactly as typing it (the topmost dialog is drawn last)
                if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                    let pos = Position::new(m.column, m.row);
                    if let Some((_, key)) =
                        self.hint_buttons.iter().rev().find(|(r, _)| r.contains(pos)).copied()
                    {
                        self.key(key, serve);
                        return false;
                    }
                }
                // the full-access dialog's checkbox row and 启用 button
                if self.full_access.is_some()
                    && let MouseEventKind::Down(MouseButton::Left) = m.kind
                {
                    let pos = Position::new(m.column, m.row);
                    if let Some((_, action)) = self.full_access_buttons.iter().find(|(r, _)| r.contains(pos)).copied() {
                        match action {
                            FullAccessAction::Toggle => self.full_access = self.full_access.map(|v| !v),
                            FullAccessAction::Enable => self.key(KeyCode::Enter.into(), serve),
                        }
                        return false;
                    }
                }
                // the /queue dialog's clickable buttons come first
                if self.queue_dialog
                    && let MouseEventKind::Down(MouseButton::Left) = m.kind
                {
                    let pos = Position::new(m.column, m.row);
                    if let Some((_, action)) = self
                        .queue_buttons
                        .iter()
                        .find(|(rect, _)| rect.contains(pos))
                        .cloned()
                    {
                        self.queue_action(serve, action);
                        return false;
                    }
                    // click outside the dialog closes it
                    self.queue_dialog = false;
                    self.queue_buttons.clear();
                    return false;
                }
                // the prompt menu's clickable rows
                if self.prompt_menu.is_some()
                    && let MouseEventKind::Down(MouseButton::Left) = m.kind
                {
                    let pos = Position::new(m.column, m.row);
                    if let Some((_, n)) = self
                        .prompt_menu_buttons
                        .iter()
                        .find(|(rect, _)| rect.contains(pos))
                        .cloned()
                    {
                        match n {
                            1 => self.prompt_menu_copy(serve),
                            2 => self.prompt_menu_edit(serve),
                            3 => self.prompt_menu_resend(serve),
                            _ => self.prompt_menu_quote(),
                        }
                        return false;
                    }
                    self.prompt_menu = None;
                    self.prompt_menu_buttons.clear();
                    return false;
                }
                // the /todo dialog's clickable buttons
                if self.todo_dialog
                    && let MouseEventKind::Down(MouseButton::Left) = m.kind
                {
                    let pos = Position::new(m.column, m.row);
                    if let Some((_, action)) = self
                        .todo_buttons
                        .iter()
                        .find(|(rect, _)| rect.contains(pos))
                        .cloned()
                    {
                        self.todo_action(serve, action);
                        return false;
                    }
                    self.todo_dialog = false;
                    self.todo_buttons.clear();
                    return false;
                }
                // a click on a choices-panel row picks that option
                if !self.choice_rows.is_empty()
                    && let MouseEventKind::Down(MouseButton::Left) = m.kind
                {
                    let pos = Position::new(m.column, m.row);
                    if let Some((_, idx)) = self
                        .choice_rows
                        .iter()
                        .find(|(rect, _)| rect.contains(pos))
                        .cloned()
                    {
                        self.pick_choice(serve, idx);
                        return false;
                    }
                }
                if let Some(viewer) = self.viewer.as_mut() {
                    match m.kind {
                        MouseEventKind::ScrollUp => viewer.scroll_by(-3, viewer.max_scroll),
                        MouseEventKind::ScrollDown => viewer.scroll_by(3, viewer.max_scroll),
                        _ => {}
                    }
                } else if self.permission.is_some() {
                    // answers stay deliberate keyboard choices; the wheel only scrolls
                    match m.kind {
                        MouseEventKind::ScrollUp => {
                            if let Some(p) = self.permission.as_ref() {
                                let total =
                                    wrap_chat_rows(&[p.detail.clone()], self.perm_wrap_w).len();
                                let max_scroll = total.saturating_sub(8);
                                self.perm_scroll =
                                    self.perm_scroll.saturating_sub(1).min(max_scroll);
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            if let Some(p) = self.permission.as_ref() {
                                let total =
                                    wrap_chat_rows(&[p.detail.clone()], self.perm_wrap_w).len();
                                let max_scroll = total.saturating_sub(8);
                                self.perm_scroll = (self.perm_scroll + 1).min(max_scroll);
                            }
                        }
                        _ => {}
                    }
                } else if self.picker.is_some() {
                    let pos = Position::new(m.column, m.row);
                    let kind = self.picker.as_ref().unwrap().kind;
                    // the info row's mode / model labels toggle their picker; the other
                    // label swaps to its own picker (only one is ever open)
                    if let MouseEventKind::Down(MouseButton::Left) = m.kind
                        && !self.picker_area.contains(pos)
                    {
                        let on_label = self.mode_area.width > 0 && self.mode_area.contains(pos);
                        let on_hint = self.footer_mode_area.width > 0 && self.footer_mode_area.contains(pos);
                        if on_label || on_hint {
                            self.picker = None;
                            if kind != PickerKind::Mode {
                                self.open_mode_picker();
                            }
                            return false;
                        }
                        if self.footer_palette_area.width > 0
                            && self.footer_palette_area.contains(pos)
                        {
                            self.picker = None;
                            if kind != PickerKind::Command {
                                self.open_command_palette();
                            }
                            return false;
                        }
                        if self.model_area.width > 0 && self.model_area.contains(pos) {
                            self.picker = None;
                            if kind != PickerKind::Model {
                                self.run_slash("/model", serve);
                            }
                            return false;
                        }
                    }
                    match m.kind {
                        MouseEventKind::ScrollUp => {
                            let p = self.picker.as_mut().unwrap();
                            p.sel = p.sel.saturating_sub(1);
                        }
                        MouseEventKind::ScrollDown => {
                            let p = self.picker.as_mut().unwrap();
                            let len = p.filtered_items().len();
                            p.sel = (p.sel + 1).min(len.saturating_sub(1));
                        }
                        MouseEventKind::Down(MouseButton::Left)
                            if self.picker_area.contains(Position::new(m.column, m.row)) =>
                        {
                            let row = m.row.saturating_sub(self.picker_area.y + 1) as usize;
                            let p = self.picker.as_ref().unwrap();
                            let query_rows = usize::from(p.filterable());
                            // map the clicked display row back to an item; header rows do nothing
                            let index = row
                                .checked_sub(query_rows)
                                .and_then(|r| self.picker_rows.get(r))
                                .copied()
                                .flatten();
                            if let Some(index) = index {
                                if index < p.filtered_items().len() {
                                    self.picker.as_mut().unwrap().sel = index;
                                    self.accept_picker(serve);
                                }
                            }
                        }
                        _ => {}
                    }
                } else if self.popup.is_some() {
                    match m.kind {
                        MouseEventKind::ScrollUp => {
                            let p = self.popup.as_mut().unwrap();
                            p.sel = p.sel.saturating_sub(1);
                        }
                        MouseEventKind::ScrollDown => {
                            let p = self.popup.as_mut().unwrap();
                            p.sel = (p.sel + 1).min(p.items.len().saturating_sub(1));
                        }
                        MouseEventKind::Down(MouseButton::Left)
                            if self.popup_area.contains(Position::new(m.column, m.row)) =>
                        {
                            // + the scroll offset the draw applied (selection kept in view)
                            let skip = self.popup.as_ref().unwrap().sel.saturating_sub(5);
                            let shown = m.row.saturating_sub(self.popup_area.y + 1) as usize;
                            let row = shown + skip;
                            // only the 6 drawn rows pick; the blank and hint rows below do not
                            if shown < 6 && row < self.popup.as_ref().unwrap().items.len() {
                                self.popup.as_mut().unwrap().sel = row;
                                // a command row runs on click, as it does on Enter
                                if self
                                    .popup
                                    .as_ref()
                                    .is_some_and(|p| p.kind == PopKind::Slash)
                                {
                                    self.run_popup_row(serve);
                                } else {
                                    self.accept_popup();
                                }
                            }
                        }
                        _ => {}
                    }
                } else if self.problems_dialog {
                    // the wheel scrolls the /problems list, never the chat behind it
                    let n = self.problem_count();
                    match m.kind {
                        MouseEventKind::ScrollUp => {
                            self.problems_sel = self.problems_sel.saturating_sub(3)
                        }
                        MouseEventKind::ScrollDown => {
                            self.problems_sel = (self.problems_sel + 3).min(n.saturating_sub(1))
                        }
                        MouseEventKind::Down(MouseButton::Left) => {
                            // a click picks a row; one outside the dialog closes it
                            let pos = Position::new(m.column, m.row);
                            let hit = self
                                .problems_rows
                                .iter()
                                .find(|(r, _)| r.contains(pos))
                                .map(|(_, i)| *i);
                            if let Some(i) = hit {
                                self.problems_sel = i;
                            } else if self.modal_rect.map(|r| !r.contains(pos)).unwrap_or(true) {
                                self.close_problems();
                            }
                        }
                        _ => {}
                    }
                } else {
                    match m.kind {
                        MouseEventKind::ScrollUp => self.scroll_back += 3,
                        MouseEventKind::ScrollDown => self.scroll_chat_down(3),
                        // one ordered click handler: input-box image chip → jump badge →
                        // chat press (selection anchor; chips/paths/viewers wait for
                        // release so a drag from anywhere can select text)
                        MouseEventKind::Down(MouseButton::Left) => {
                            let pos = Position::new(m.column, m.row);
                            // clicking the history emoji at the left of the status bar opens /history
                            if self.history_area.width > 0 && self.history_area.contains(pos) {
                                self.run_slash("/history", serve);
                                return false;
                            }
                            // clicking the status-bar problems badge opens /problems
                            if self.problems_area.width > 0 && self.problems_area.contains(pos) {
                                self.run_slash("/problems", serve);
                                return false;
                            }
                            // clicking the status-bar sess span copies the full id
                            if self.session_id_area.width > 0 && self.session_id_area.contains(pos)
                            {
                                let id = self.session_full.clone();
                                match self.copy_to_clipboard(&id) {
                                    Ok(()) => {
                                        self.toast(format!("已复制会话 id：{id}"))
                                    }
                                    Err(e) => self.push(format!("复制失败：{e}"), Tone::Error),
                                }
                                return false;
                            }
                            // clicking the model in the input info row opens the model
                            // picker, the current model preselected
                            if self.model_area.width > 0 && self.model_area.contains(pos) {
                                self.run_slash("/model", serve);
                                return false;
                            }
                            // …the mode label (or the footer's shift+tab hint) opens the mode
                            // picker on the current mode; the Shift+Tab key itself cycles
                            if self.mode_area.width > 0 && self.mode_area.contains(pos)
                                || self.footer_mode_area.width > 0
                                    && self.footer_mode_area.contains(pos)
                            {
                                self.open_mode_picker();
                                return false;
                            }
                            // the footer's ctrl+p hint opens the command palette
                            if self.footer_palette_area.width > 0
                                && self.footer_palette_area.contains(pos)
                                && self.viewer.is_none()
                            {
                                self.open_command_palette();
                                return false;
                            }
                            // a click on an input-box image chip opens the image
                            self.input_sel = None;
                            if self.input_area.contains(pos) {
                                if let Some(path) = self.input_image_at(m.row, m.column) {
                                    self.open_image(&path);
                                    return false;
                                }
                                // the rule row is decoration; text rows start a selection
                                if m.row > self.input_area.y {
                                    self.input_press(m.row, m.column);
                                    return false;
                                }
                            }
                            if let Some((area, back)) = self.prompt_header
                                && area.contains(pos)
                            {
                                self.scroll_back = back;
                            } else if self.jump_area.contains(pos) {
                                self.jump_to_bottom();
                            } else if self.chat_area.contains(pos) {
                                self.chat_press(m.row, m.column);
                            }
                        }
                        MouseEventKind::Drag(MouseButton::Left) if self.input_dragging => {
                            self.input_drag(m.row, m.column)
                        }
                        MouseEventKind::Drag(MouseButton::Left) => self.chat_drag(m.row, m.column),
                        MouseEventKind::Up(MouseButton::Left) if self.input_dragging => {
                            self.input_release()
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            self.chat_release(self.chat_area.y, m.row, m.column)
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
                    let id = serve.command("models", vec![]);
                    self.pending.push((id, Pending::ArgModels));
                    let id = serve.command("sessions", vec![]);
                    self.pending.push((id, Pending::ArgSessions));
                }
                self.frame(v, serve)
            }
        }
        false
    }

    fn cd_to(&mut self, dir: &str, serve: &mut Serve) {
        let id = serve.command("cd", vec![("dir", Value::String(dir.trim().into()))]);
        self.pending.push((id, Pending::Cd));
    }

    fn open_cwd_picker(&mut self, dirs: &[String]) {
        let mut items: Vec<PickItem> = dirs
            .iter()
            .map(|dir| PickItem {
                group: String::new(),
                id: dir.clone(),
                label: dir.clone(),
                note: String::new(),
                warn: false,
                current: self
                    .serve_cwd
                    .as_ref()
                    .is_some_and(|c| c.to_string_lossy() == dir.as_str()),
            })
            .collect();
        items.push(PickItem {
            group: String::new(),
            id: "__input__".into(),
            label: "自己输入路径".into(),
            note: "回车后在输入框里写 /路径 或 ~/路径".into(),
            warn: false,
            current: false,
        });
        self.picker = Some(Picker::new(PickerKind::Cwd, items));
    }

    fn accept_picker(&mut self, serve: &mut Serve) {
        let query = self
            .picker
            .as_ref()
            .map(|p| p.query.trim().to_string())
            .unwrap_or_default();
        let cwd_pick = self.picker.as_ref().map(|p| p.kind) == Some(PickerKind::Cwd);
        let picked = self
            .picker
            .as_ref()
            .and_then(|p| p.selected_item().map(|item| (p.kind, item)));
        // the model picker: the level set with ←→ on the row Enter lands on, if any
        let model_effort = self
            .picker
            .as_ref()
            .filter(|p| p.kind == PickerKind::Model)
            .and_then(|p| p.selected_item())
            .and_then(|item| self.picker.as_ref().and_then(|p| p.effort_pick.get(&item.id).cloned()));
        self.picker = None;
        // a path typed in the cwd picker is the directory, not a filter
        if cwd_pick
            && (query.starts_with('/') || query.starts_with('~') || query.starts_with('.'))
        {
            self.cd_to(&query, serve);
            return;
        }
        if let Some((kind, item)) = picked {
            match kind {
                PickerKind::Model => {
                    if item.warn {
                        self.push(
                            format!("! {} has no api key configured", item.id),
                            Tone::Muted,
                        );
                    }
                    // with the level picked beside it: off sends none
                    let id = serve.command(
                        "set_model",
                        match &model_effort {
                            Some(level) => vec![
                                ("modelId", Value::String(item.id.clone())),
                                ("effort", Value::String(level.clone())),
                            ],
                            None => vec![("modelId", Value::String(item.id.clone()))],
                        },
                    );
                    self.pending.push((id, Pending::Model));
                }
                // zcode's execution-authorization dropdown: one line per mode, the
                // current one marked. Full access goes through its own acknowledgement.
                PickerKind::Mode => match item.id.as_str() {
                    "plan" => {
                        if !self.plan_mode {
                            let id = serve.command("set_plan", vec![("on", Value::Bool(true))]);
                            self.pending.push((id, Pending::Plan));
                        }
                    }
                    "auto" => self.full_access = Some(false),
                    id => {
                        if self.plan_mode {
                            let pid = serve.command("set_plan", vec![("on", Value::Bool(false))]);
                            self.pending.push((pid, Pending::Plan));
                        }
                        let mid = serve.command("set_mode", vec![("mode", Value::String(id.into()))]);
                        self.pending.push((mid, Pending::Mode));
                    }
                },
                PickerKind::HandoffTarget | PickerKind::TakeoverSource => {
                    self.send_transfer(kind, &item.id, "", serve);
                }
                PickerKind::SendTarget => {
                    // the target is fixed; the message still needs typing
                    let lead = format!("/send {} ", item.id);
                    self.input = lead.chars().collect();
                    self.cursor = self.input.len();
                }
                PickerKind::Session => {
                    self.push(format!("↻ resuming session {} …", item.id), Tone::Muted);
                    self.switch_session(Some(item.id));
                }
                PickerKind::Timeline => {
                    if let Ok(entry) = item.id.parse::<u64>() {
                        let id = serve.command("rewind", vec![("entryIndex", Value::from(entry))]);
                        self.pending.push((id, Pending::Rewind));
                        self.push("⟲ 回滚中 …", Tone::Muted);
                    }
                }
                PickerKind::Preset => {
                    let id = serve.command("preset_get", vec![("name", Value::String(item.id))]);
                    self.pending.push((id, Pending::PresetGet));
                }
                PickerKind::Skills => {
                    self.push(
                        format!("技能位于 {} · 打开即可阅读/编辑", item.id),
                        Tone::Muted,
                    );
                }
                PickerKind::Worktree => {
                    self.push(
                        format!(
                            "/worktree merge {} 合并 · /worktree rm {} 删除",
                            item.id, item.id
                        ),
                        Tone::Muted,
                    );
                }
                PickerKind::Permissions => {
                    // selecting a rule shows how to remove it; no accidental action
                    self.push(
                        format!("如需移除：/permissions rm {}", item.id),
                        Tone::Muted,
                    );
                }
                PickerKind::Find => {
                    // item.id carries "sessionId:entryIndex:current-orphan"
                    let parts: Vec<&str> = item.id.split(':').collect();
                    if parts.len() == 3 {
                        let (sid, entry, cur) = (parts[0], parts[1], parts[2] == "1");
                        if cur {
                            if let Ok(e) = entry.parse::<u64>() {
                                let id = serve.command(
                                    "rewind",
                                    vec![
                                        ("entryIndex", Value::from(e)),
                                        ("include", Value::Bool(true)),
                                    ],
                                );
                                self.pending.push((id, Pending::Rewind));
                                self.push("⟲ 恢复到命中位置 …", Tone::Muted);
                            }
                        } else {
                            self.push(format!("↻ resuming session {sid} …"), Tone::Muted);
                            self.switch_session(Some(sid.to_string()));
                        }
                    }
                }
                PickerKind::Cwd => {
                    if item.id == "__input__" {
                        let prefill = if query.is_empty() {
                            "/cd ".to_string()
                        } else {
                            format!("/cd {query}")
                        };
                        self.input = prefill.chars().collect();
                        self.cursor = self.input.len();
                    } else {
                        self.cd_to(&item.id, serve);
                    }
                }
                PickerKind::Command => self.run_slash(&item.id, serve),
                PickerKind::Theme => self.run_slash(&format!("/theme {}", item.id), serve),
                PickerKind::Trust => self.run_slash(&format!("/trust {}", item.id), serve),
                PickerKind::History => {
                    if let Ok(index) = item.id.parse::<usize>()
                        && let Some(text) = self.prompt_history.get(index).cloned()
                    {
                        self.restore_prompt(text);
                    }
                }
                PickerKind::NvimBackend => {
                    if let Some(viewer) = self.pending_viewer.take() {
                        if item.id == "internal" {
                            let id = serve.command(
                                "set_nvim_viewer",
                                vec![("mode", Value::String("internal".into()))],
                            );
                            self.pending.push((id, Pending::NvimViewer));
                            self.viewer = Some(viewer);
                        } else if let Some(editor) = self.detect_nvim() {
                            if item.id == "always" {
                                let id = serve.command(
                                    "set_nvim_viewer",
                                    vec![("mode", Value::String("readonly".into()))],
                                );
                                self.pending.push((id, Pending::NvimViewer));
                            }
                            self.launch_nvim(editor, viewer, item.id != "edit");
                        }
                    }
                }
                PickerKind::EditResolution => {
                    if let Some((viewer, edited)) = self.edited.take() {
                        let resolution = match item.id.as_str() {
                            "reprocess" => EditResolution::Reprocess,
                            "correct" => EditResolution::Correct,
                            _ => EditResolution::New,
                        };
                        if let Some(text) = edit_submission(resolution, &viewer.text, &edited) {
                            if self.busy {
                                serve.notify(json!({ "cmd": "interrupt" }));
                            }
                            self.submit_text(serve, text, Vec::new());
                        }
                    }
                }
            }
        }
    }

    fn submit_text(&mut self, serve: &mut Serve, text: String, images: Vec<String>) {
        self.turn_started = Some(std::time::Instant::now());
        self.turn_chars = 0;
        self.last_output = self.turn_started;
        // a new prompt answers whatever was on screen; an unanswered choice stays
        // parked in todo for explicit handling, but heuristic trailing questions
        // count any prompt as their answer
        self.choices = None;
        self.todos
            .retain(|t| !matches!(t.kind, TodoKind::Question { .. }));
        // pin what we're working on: first line, capped for the status bar
        let first = text.lines().next().unwrap_or("").to_string();
        let shown: String = first.chars().take(48).collect();
        self.current_prompt = Some(shown);
        self.push_user(&text);
        self.jump_to_bottom();
        self.busy = true;
        self.prompt_id = serve.command("prompt", prompt_extras(text, images));
        if let Some(line) = self.lines.last_mut() {
            line.prompt_state = Some(PromptState::Running);
        }
    }

    fn move_input_vertical(&mut self, down: bool) {
        let mut lines = Vec::new();
        let mut start = 0;
        for (index, c) in self.input.iter().enumerate() {
            if *c == '\n' {
                lines.push((start, index));
                start = index + 1;
            }
        }
        lines.push((start, self.input.len()));
        let Some(current) = lines
            .iter()
            .position(|(start, end)| self.cursor >= *start && self.cursor <= *end)
        else {
            return;
        };
        let target = if down {
            if current + 1 >= lines.len() {
                return;
            }
            current + 1
        } else {
            if current == 0 {
                return;
            }
            current - 1
        };
        let (start, _) = lines[current];
        let before_cursor = self.input[start..self.cursor].iter().collect::<String>();
        let column = display_width(&before_cursor);
        let (target_start, target_end) = lines[target];
        let mut target_cursor = target_end;
        let mut width = 0;
        for index in target_start..target_end {
            let char_width = display_width(&self.input[index].to_string());
            if width + char_width > column {
                target_cursor = index;
                break;
            }
            width += char_width;
            target_cursor = index + 1;
        }
        self.cursor = target_cursor;
        self.snap_cursor_to_paste_boundary();
    }

    /// Vertical movement lands on a raw column, which can fall inside a collapsed
    /// paste; pastes are atomic, so rest on the leading edge.
    fn snap_cursor_to_paste_boundary(&mut self) {
        if let Some(paste) = self
            .pastes
            .iter()
            .find(|paste| self.cursor > paste.start && self.cursor < paste.end)
        {
            self.cursor = paste.start;
        }
    }

    fn normal_edit(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('h') => self.move_input_cursor(true),
            KeyCode::Char('l') => self.move_input_cursor(false),
            KeyCode::Char('0') => self.cursor = 0,
            KeyCode::Char('$') => self.cursor = self.input.len(),
            KeyCode::Char('x') if self.cursor < self.input.len() => {
                self.delete_input();
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
            KeyCode::Char('j') => self.move_input_vertical(true),
            KeyCode::Char('k') => self.move_input_vertical(false),
            KeyCode::Char(':') => self.vim_command = Some(String::new()),
            KeyCode::Char('/') | KeyCode::Char('@') => {
                self.edit_mode = EditMode::Insert;
                return false;
            }
            KeyCode::Enter => return false,
            _ => {}
        }
        true
    }

    fn vim_command_key(&mut self, code: KeyCode) -> Option<bool> {
        self.vim_command.as_ref()?;
        Some(match code {
            KeyCode::Char(c) => {
                self.vim_command.as_mut().unwrap().push(c);
                false
            }
            KeyCode::Backspace => {
                let command = self.vim_command.as_mut().unwrap();
                if command.is_empty() {
                    self.vim_command = None;
                } else {
                    command.pop();
                }
                false
            }
            KeyCode::Esc => {
                self.vim_command = None;
                false
            }
            KeyCode::Enter => {
                let command = self.vim_command.take().unwrap();
                match command.as_str() {
                    "wq" | "wqa" => true,
                    _ => {
                        self.push(
                            format!("不支持的 Vim 命令 :{command} · 使用 :wq、:wqa 或 ctrl+s"),
                            Tone::Error,
                        );
                        false
                    }
                }
            }
            _ => false,
        })
    }

    fn key(&mut self, k: crossterm::event::KeyEvent, serve: &mut Serve) {
        // any key may edit the input, which would make the display indices stale
        self.input_sel = None;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // any other key cancels a pending "press Ctrl+C again to exit"
        let exit_armed = self.exit_armed.take();
        if self.viewer.is_some() {
            // Ctrl+C always remains a safe interrupt even when the action is remapped —
            // and even while a viewer (e.g. an in-flight /btw answer) has the keys
            if self.busy && self.keys.matches(Action::Interrupt, k) {
                serve.notify(json!({ "cmd": "interrupt" }));
                self.goal_text = None; // serve clears the goal; mirror it immediately
                if !self.local_queue.is_empty() {
                    self.push(
                        format!(
                            "排队保留 {} 条（中断后自动继续）· /queue clear 全清",
                            self.local_queue.len()
                        ),
                        Tone::Muted,
                    );
                }
                self.push("[interrupt requested]", Tone::Warning);
                return;
            }
            if ctrl && k.code == KeyCode::Char('e') {
                let viewer = self.viewer.clone().unwrap();
                if viewer.read_only {
                    self.push("该查看器为只读", Tone::Muted);
                    return;
                }
                if let Some(editor) = self.detect_nvim() {
                    self.launch_nvim(editor, viewer, false);
                } else {
                    self.push("未检测到 Neovim，继续使用内置查看器", Tone::Muted);
                }
                return;
            }
            if ctrl && k.code == KeyCode::Char('t') {
                self.viewer = None;
                return;
            }
            if self.viewer.as_ref().unwrap().search_input {
                let viewer = self.viewer.as_mut().unwrap();
                match k.code {
                    KeyCode::Esc => viewer.cancel_search(),
                    KeyCode::Enter => viewer.accept_search(),
                    KeyCode::Backspace => viewer.search_backspace(),
                    KeyCode::Char(c) if !ctrl => viewer.search_push(c),
                    _ => {}
                }
                return;
            }
            if !ctrl && matches!(k.code, KeyCode::Char('y') | KeyCode::Char('c')) {
                let text = self.viewer.as_ref().unwrap().text.clone();
                let result = self.copy_to_clipboard(&text);
                match result {
                    Ok(()) => {
                        let n = text.chars().count();
                        self.toast(format!("已复制 {n} 个字符到剪贴板"));
                    }
                    Err(error) => self.push(format!("复制失败：{error}"), Tone::Error),
                }
                return;
            }
            if k.code == KeyCode::Esc
                || k.code == KeyCode::Char('q')
                || self.keys.matches(Action::ExpandTools, k)
            {
                self.viewer = None;
                return;
            }
            let viewer = self.viewer.as_mut().unwrap();
            match k.code {
                // search lives in the transcript; elsewhere '/' would open an invisible
                // search row that swallows keys
                KeyCode::Char('/') if viewer.transcript => viewer.start_search(),
                KeyCode::Char('n') => viewer.next_match(true),
                KeyCode::Char('N') => viewer.next_match(false),
                KeyCode::Char('{') => viewer.next_prompt(false),
                KeyCode::Char('}') => viewer.next_prompt(true),
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
        // the full-access acknowledgement grabs the keyboard (zcode: the confirm
        // button stays disabled until "我已了解风险" is checked)
        if self.full_access.is_some() {
            match k.code {
                KeyCode::Char(' ') | KeyCode::Enter if k.code == KeyCode::Char(' ') => {
                    self.full_access = self.full_access.map(|v| !v);
                }
                KeyCode::Enter => {
                    if self.full_access == Some(true) {
                        self.full_access = None;
                        let id = serve.command("set_mode", vec![("mode", Value::String("auto".into()))]);
                        self.pending.push((id, Pending::Mode));
                    } else {
                        self.toast("先按空格勾选「我已了解风险」");
                    }
                }
                KeyCode::Esc => self.full_access = None,
                _ => {}
            }
            return;
        }
        // permission modal grabs the keyboard
        if self.permission.is_some() {
            match k.code {
                // scroll the command preview — reading is never answering
                KeyCode::Up => {
                    if let Some(p) = self.permission.as_ref() {
                        let total = wrap_chat_rows(&[p.detail.clone()], self.perm_wrap_w).len();
                        let max_scroll = total.saturating_sub(8);
                        self.perm_scroll = self.perm_scroll.saturating_sub(1).min(max_scroll);
                    }
                }
                KeyCode::Down => {
                    if let Some(p) = self.permission.as_ref() {
                        let total = wrap_chat_rows(&[p.detail.clone()], self.perm_wrap_w).len();
                        let max_scroll = total.saturating_sub(8);
                        self.perm_scroll = (self.perm_scroll + 1).min(max_scroll);
                    }
                }
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.answer(serve, true)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.answer(serve, false),
                // always: persist allow rules for THIS call — what 始终允许 promises
                // — then allow it. (It used to flip the whole session to /mode auto:
                // broader than the user meant, and remembered nothing past exit.)
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    // every `; && || |` segment gets its rule, or a risky tail the first
                    // rule does not cover would ask again next time ("a" that never sticks)
                    let rules: Vec<String> = self
                        .permission
                        .as_ref()
                        .map(Self::always_rules)
                        .unwrap_or_default();
                    for rule in &rules {
                        let id = serve.command(
                            "permissions_add",
                            vec![("rule", Value::String(rule.clone()))],
                        );
                        self.pending.push((id, if rule == &rules[0] { Pending::PermissionsAdd } else { Pending::PermissionsAddQuiet }));
                    }
                    self.answer(serve, true);
                }
                // copy the pending command to the clipboard without answering
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    if let Some(p) = self.permission.as_ref() {
                        let text = format!("{} {}", p.name, p.detail);
                        match self.copy_to_clipboard(&text) {
                            Ok(()) => self.toast("命令已复制（y/n/a 待选）"),
                            Err(e) => self.push(format!("复制失败：{e}"), Tone::Error),
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        if self.queue_dialog && !ctrl {
            // the dialog is modal: its keys never reach the input box
            self.queue_key(serve, k);
            return;
        }
        if self.todo_dialog && !ctrl {
            // modal like /queue: its keys never reach the input box
            self.todo_key(serve, k);
            return;
        }
        if self.problems_dialog && !ctrl {
            // modal like /todo: its keys never reach the input box
            self.problems_key(k);
            return;
        }
        // the choices panel navigates only when the input is empty — typing wins
        if self.choices.is_some()
            && !self.choices_collapsed
            && self.input.is_empty()
            && !self.queue_dialog
            && !self.todo_dialog
        {
            match k.code {
                KeyCode::Up => {
                    if let Some(c) = self.choices.as_mut() {
                        c.sel = c.sel.saturating_sub(1);
                    }
                    return;
                }
                KeyCode::Down => {
                    if let Some(c) = self.choices.as_mut() {
                        // + 1: the built-in explain row is selectable too
                        c.sel = (c.sel + 1).min(c.options.len());
                    }
                    return;
                }
                KeyCode::Enter => {
                    let sel = self.choices.as_ref().map(|c| c.sel).unwrap_or(0);
                    self.pick_choice(serve, sel);
                    return;
                }
                // Esc closes the panel first, even mid-turn; the next Esc interrupts
                KeyCode::Esc => {
                    self.choices_collapsed = true;
                    return;
                }
                // tab = amend: the options are suggestions, the user's own words win
                KeyCode::Tab => {
                    let lead = self
                        .choices
                        .as_ref()
                        .map(|c| Self::amend_lead(&c.question, self.chat_language()))
                        .unwrap_or_default();
                    self.input = lead.chars().collect();
                    self.cursor = self.input.len();
                    self.choices_collapsed = true;
                    return;
                }
                _ => {}
            }
        }
        // a lone option number + Enter picks it ("3" would otherwise go out as a prompt)
        if k.code == KeyCode::Enter
            && k.modifiers.is_empty()
            && !self.busy
            && !self.choices_collapsed
            && !self.queue_dialog
            && !self.todo_dialog
            && let Some(c) = &self.choices
            && let Ok(n) = self
                .input
                .iter()
                .collect::<String>()
                .trim()
                .parse::<usize>()
        {
            // the built-in explain row is one past the model's options (not on suggest panels)
            let rows = c.options.len() + usize::from(c.suggest.is_none());
            if (1..=rows).contains(&n) {
                self.clear_input();
                self.pick_choice(serve, n - 1);
                return;
            }
        }
        if ctrl && k.code == KeyCode::Char('t') {
            self.open_transcript();
            return;
        }
        if ctrl && k.code == KeyCode::Char('p') && self.viewer.is_none() {
            self.open_command_palette();
            return;
        }
        // Shift+Tab steps to the next mode at once, no dialog: ask → plan → confirm → readonly
        // → ask. (A click on the footer's shift+tab hint opens the mode picker instead.)
        if k.code == KeyCode::BackTab
            && self.picker.is_none()
            && self.popup.is_none()
            && self.viewer.is_none()
        {
            self.cycle_mode(serve);
            return;
        }
        if ctrl && k.code == KeyCode::Char('r') && self.picker.is_none() && self.popup.is_none() {
            self.open_prompt_history();
            return;
        }
        if k.modifiers.contains(KeyModifiers::ALT)
            && k.code == KeyCode::Char('s')
            && self.picker.is_none()
            && self.popup.is_none()
        {
            self.toggle_draft_stash();
            return;
        }
        if ctrl && k.code == KeyCode::Char('s') && self.picker.is_none() && self.popup.is_none() {
            if self.busy && !self.input.is_empty() {
                let (text, images) = self.take_submission();
                let draft = self.take_history_draft();
                self.clear_input();
                self.local_queue.push(QueuedPrompt {
                    text: text.clone(),
                    images,
                });
                self.restore_draft(draft);
                let head: String = text.lines().next().unwrap_or("").chars().take(40).collect();
                self.push(
                    format!(
                        "📋 已排队 {}（依次自动发送 · /queue 管理 · ctrl+c 只打断当前）：{head}",
                        self.local_queue.len()
                    ),
                    Tone::Muted,
                );
            } else if self.busy {
                self.push("输入为空 — 没有可排队的内容", Tone::Muted);
            } else {
                self.send_prompt(serve);
            }
            return;
        }
        // control keys (notably ctrl+c) must fall through to the interrupt handling
        // even while a ':' command line is open
        if !ctrl && let Some(send) = self.vim_command_key(k.code) {
            if send {
                if self.busy {
                    self.push("[queued — ctrl+c interrupts]", Tone::Muted);
                } else {
                    self.send_prompt(serve);
                }
            }
            return;
        }
        if ctrl && k.code == KeyCode::End {
            self.jump_to_bottom();
            return;
        }
        if self.keys.matches(Action::ExpandTools, k) {
            self.open_latest_viewer();
            return;
        }
        // Ctrl+C always remains a safe interrupt even when the action is remapped.
        // the prompt menu grabs number keys; anything else closes it first
        if self.prompt_menu.is_some() {
            match k.code {
                KeyCode::Char('1') => {
                    self.prompt_menu_copy(serve);
                    return;
                }
                KeyCode::Char('2') => {
                    self.prompt_menu_edit(serve);
                    return;
                }
                KeyCode::Char('3') => {
                    self.prompt_menu_resend(serve);
                    return;
                }
                KeyCode::Char('4') => {
                    self.prompt_menu_quote();
                    return;
                }
                KeyCode::Esc => {
                    self.prompt_menu = None;
                    return;
                }
                _ => {
                    self.prompt_menu = None; // close, then handle the key normally
                }
            }
        }
        // alt+e: edit the last prompt — interrupt + rewind + preload, two presses
        // because the rewind also rolls back code
        if self.keys.matches(Action::EditLast, k) {
            if self.editlast_armed {
                self.editlast_armed = false;
                let id = serve.command("editlast", vec![]);
                self.pending.push((id, Pending::EditLast));
                self.push("⟲ 打断并回滚上一句中 …", Tone::Muted);
            } else {
                self.editlast_armed = true;
                self.push(
                    "⚠ editlast：将打断当前回合并把对话与代码回滚到上一句之前，原文载入输入框 · 再按一次执行 · esc 取消"
                        .to_string(),
                    Tone::Warning,
                );
            }
            return;
        }
        // ctrl+c is two-stage: a non-empty input is cleared first; only an empty
        // input lets it (or Esc) interrupt the running turn
        // an open picker or popup takes Esc first (closing it); Ctrl+C still interrupts
        let interrupt_key = (k.code == KeyCode::Esc && self.picker.is_none() && self.popup.is_none())
            || (self.keys.matches(Action::Interrupt, k) && self.input.is_empty());
        if self.busy && interrupt_key {
            serve.notify(json!({ "cmd": "interrupt" }));
            self.goal_text = None;
            if !self.local_queue.is_empty() {
                self.push(
                    format!(
                        "排队保留 {} 条（中断后自动继续）· /queue clear 全清",
                        self.local_queue.len()
                    ),
                    Tone::Muted,
                );
            }
            self.push("[interrupt requested]", Tone::Warning);
            return;
        }
        // the picker grabs navigation keys while it is open
        if self.picker.is_some() {
            let scoped = self
                .picker
                .as_ref()
                .is_some_and(|p| Picker::session_scoped(p.kind));
            match k.code {
                // ctrl+a/ctrl+b toggle the project/branch filters (session pickers)
                KeyCode::Char('a') if ctrl && scoped => {
                    if let Some(p) = self.picker.as_mut() {
                        p.all_projects = !p.all_projects;
                        p.sel = 0;
                    }
                    return;
                }
                KeyCode::Char('b') if ctrl && scoped => {
                    if let Some(p) = self.picker.as_mut() {
                        p.branch_only = !p.branch_only;
                        p.sel = 0;
                    }
                    return;
                }
                // ctrl+r renames the highlighted session
                KeyCode::Char('r') if ctrl && scoped => {
                    if let Some(p) = self.picker.as_mut() {
                        if let Some(item) = p.selected_item() {
                            p.renaming = Some(item.id);
                            p.query.clear();
                        }
                    }
                    return;
                }
                // space previews the highlighted session in the read-only viewer
                KeyCode::Char(' ')
                    if !ctrl
                        && scoped
                        && self.picker.as_ref().is_some_and(|p| p.renaming.is_none()) =>
                {
                    if let Some(id) = self
                        .picker
                        .as_ref()
                        .and_then(|p| p.selected_item().map(|i| i.id))
                    {
                        let rid =
                            serve.command("session_preview", vec![("session", Value::String(id))]);
                        self.pending.push((rid, Pending::SessionPreview));
                    }
                    return;
                }
                // grok's model picker: ←→ step the reasoning level of the highlighted model
                KeyCode::Left | KeyCode::Right
                    if self.picker.as_ref().is_some_and(|p| p.kind == PickerKind::Model) =>
                {
                    if let Some(p) = self.picker.as_mut() {
                        p.step_effort(k.code == KeyCode::Right);
                    }
                    return;
                }
                KeyCode::Up => {
                    if let Some(p) = self.picker.as_mut() {
                        p.sel = p.sel.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    if let Some(p) = self.picker.as_mut() {
                        let len = p.filtered_items().len();
                        p.sel = (p.sel + 1).min(len.saturating_sub(1));
                    }
                }
                KeyCode::Esc => {
                    self.editlast_armed = false;
                    // cancel an in-progress rename without closing the picker
                    if let Some(p) = self.picker.as_mut()
                        && p.renaming.take().is_some()
                    {
                        p.query.clear();
                        return;
                    }
                    if self.picker.as_ref().map(|p| p.kind) == Some(PickerKind::NvimBackend)
                        && let Some(viewer) = self.pending_viewer.take()
                    {
                        self.viewer = Some(viewer);
                    }
                    if self.picker.as_ref().map(|p| p.kind) == Some(PickerKind::EditResolution) {
                        self.edited = None;
                    }
                    self.picker = None;
                }
                KeyCode::Enter | KeyCode::Tab => {
                    // commit a rename instead of accepting the pick
                    let rename = self.picker.as_ref().and_then(|p| p.renaming.clone());
                    if let Some(session) = rename {
                        let name = self
                            .picker
                            .as_ref()
                            .map(|p| p.query.trim().to_string())
                            .unwrap_or_default();
                        self.picker = None;
                        if !name.is_empty() {
                            serve.command(
                                "session_rename",
                                vec![
                                    ("session", Value::String(session)),
                                    ("name", Value::String(name.clone())),
                                ],
                            );
                            self.push(format!("✓ 会话已重命名：{name}"), Tone::Success);
                        }
                        return;
                    }
                    self.accept_picker(serve);
                }
                _ => {
                    if let Some(p) = self.picker.as_mut() {
                        p.filter_key(k.code, ctrl);
                    }
                }
            }
            return;
        }
        // popup grabs navigation keys while it is open
        // /effort and /mode: Enter picks the highlighted row rather than running what is typed
        let arg_runs = self.popup.as_ref().is_some_and(|p| p.kind == PopKind::Arg) && self.arg_row_runs();
        if let Some(p) = &mut self.popup {
            match k.code {
                KeyCode::Up if p.sel > 0 => {
                    p.sel -= 1;
                    p.cycling = false; // the next Tab inserts the row moved to
                    return;
                }
                KeyCode::Down => {
                    p.sel = min(p.sel + 1, p.items.len().saturating_sub(1));
                    p.cycling = false;
                    return;
                }
                KeyCode::Tab | KeyCode::Right if p.kind == PopKind::Arg => {
                    self.cycle_arg(1);
                    return;
                }
                KeyCode::BackTab if p.kind == PopKind::Arg => {
                    self.cycle_arg(-1);
                    return;
                }
                // Enter runs the highlighted command, typed in full or not: "/mo" ⏎ runs
                // /model, the first match (↑↓ picks another row first); Tab only completes
                KeyCode::Enter if p.kind == PopKind::Slash => {
                    self.run_popup_row(serve);
                    return;
                }
                // Enter runs the highlighted level/mode, typed in full or not: "/effort h" ⏎ sets
                // high (what was typed, "h", is no level). /model, /send and /resume keep running
                // what is typed (serve fuzzy-matches /model itself; a message follows /send's id)
                KeyCode::Enter if p.kind == PopKind::Arg && arg_runs => {
                    self.run_arg_row(serve);
                    return;
                }
                KeyCode::Enter if p.kind == PopKind::Arg => {
                    self.popup = None;
                }
                KeyCode::Tab | KeyCode::Right if p.kind != PopKind::History => {
                    self.accept_popup();
                    return;
                }
                // the history list: Enter (or Tab) jumps to the chosen prompt; Esc just closes
                KeyCode::Enter | KeyCode::Tab if p.kind == PopKind::History => {
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
        if ctrl && k.code == KeyCode::Char('e') && self.popup.is_none() {
            self.edit_draft();
            return;
        }
        if self.vim && self.edit_mode == EditMode::Insert && k.code == KeyCode::Esc {
            self.edit_mode = EditMode::Normal;
            self.popup = None;
            self.cursor = self.cursor.saturating_sub(1);
            return;
        }
        // Ctrl+C on an empty input while idle: the first press warns on the input
        // rule, a second one within EXIT_WINDOW leaves like /exit
        if self.keys.matches(Action::Interrupt, k)
            && self.input.is_empty()
            && self.history_nav.is_none()
            && !self.busy
        {
            self.editlast_armed = false;
            if exit_armed.is_some_and(|t| t.elapsed() < EXIT_WINDOW) {
                self.quit = true;
            } else {
                self.exit_armed = Some(std::time::Instant::now());
            }
            return;
        }
        if self.keys.matches(Action::Interrupt, k) || self.keys.matches(Action::ClearInput, k) {
            self.editlast_armed = false; // esc backs out of the armed confirm too
            if self.history_nav.is_some() {
                // esc while browsing history: back to the draft, not wiping it
                self.exit_history_nav();
                return;
            }
            self.clear_input();
            return;
        }
        // alt+↑/alt+↓ hop between user prompts — checked before history nav,
        // which matches plain ↑/↓ without looking at modifiers
        if self.keys.matches(Action::PromptPrev, k) {
            self.jump_prompt(false);
            return;
        }
        if self.keys.matches(Action::PromptNext, k) {
            self.jump_prompt(true);
            return;
        }
        // up/down in the input box walk the sent-prompt history (the unsent draft
        // stashed aside); inside a multi-line entry they move the cursor by lines
        // first. Falls through to chat scroll when there is nothing to recall.
        if !self.vim || self.edit_mode == EditMode::Insert {
            if k.code == KeyCode::Up && self.history_up() {
                return;
            }
            if k.code == KeyCode::Down && self.history_down() {
                return;
            }
        }
        if self.keys.matches(Action::ScrollUp, k) {
            self.scroll_back += 1;
            return;
        }
        if self.keys.matches(Action::ScrollDown, k) {
            self.scroll_chat_down(1);
            return;
        }
        if self.keys.matches(Action::PageUp, k) {
            self.scroll_back += 20;
            return;
        }
        if self.keys.matches(Action::PageDown, k) {
            self.scroll_chat_down(20);
            return;
        }
        // Alt+Enter while a turn runs supplements it — this must run BEFORE the
        // rebindable Newline action, whose default binding IS alt+enter, or the
        // generic newline eats the key and the supplement branch is unreachable
        if self.busy
            && !self.input.is_empty()
            && k.code == KeyCode::Enter
            && k.modifiers.contains(KeyModifiers::ALT)
        {
            let text: String = self.input.iter().collect();
            self.clear_input();
            let head: String = text.lines().next().unwrap_or("").chars().take(40).collect();
            serve.command("supplement", vec![("text", Value::String(text))]);
            self.push(
                format!("➕ 已并入当前回合（下一轮请求可见）：{head}"),
                Tone::Success,
            );
            return;
        }
        if self.keys.matches(Action::Newline, k) {
            self.insert_char('\n');
            self.refresh_popup();
            return;
        }
        if self.vim && self.edit_mode == EditMode::Normal && self.normal_edit(k.code) {
            return;
        }
        match k.code {
            KeyCode::Enter
                if self.rendered_input().text.trim_start().starts_with('/')
                    && file_prompt(&self.input.iter().collect::<String>()).is_none() =>
            {
                // judge by the displayed text: an image chip's raw path also starts
                // with '/', but it is an attachment, not a slash command
                let text: String = self.input.iter().collect();
                let draft = self.take_history_draft();
                self.clear_input();
                self.run_slash(&text, serve); // commands stay live during a turn
                self.restore_draft(draft);
            }
            KeyCode::Enter if self.vim && self.edit_mode == EditMode::Insert => {
                self.insert_char('\n');
                self.refresh_popup();
            }
            KeyCode::Enter if self.vim => {}
            KeyCode::Enter if self.multiline_enter => {
                self.insert_char('\n');
                self.refresh_popup();
            }
            KeyCode::Enter if !self.busy => self.send_prompt(serve),
            KeyCode::Enter if !self.input.is_empty() && self.queue_auto_append => {
                // /queue auto append: ride the running turn instead of queueing
                let text: String = self.input.iter().collect();
                self.clear_input();
                serve.command("supplement", vec![("text", Value::String(text.clone()))]);
                let head: String = text.lines().next().unwrap_or("").chars().take(40).collect();
                self.push(
                    format!("➕ auto append 已并入当前回合：{head}"),
                    Tone::Success,
                );
            }
            KeyCode::Enter if !self.input.is_empty() => {
                // actually queue it: sent in order when the running turn finishes;
                // ctrl+c interrupts the turn AND drops the queue
                let (text, images) = self.take_submission();
                let draft = self.take_history_draft();
                self.clear_input();
                self.local_queue.push(QueuedPrompt {
                    text: text.clone(),
                    images,
                });
                self.restore_draft(draft);
                let head: String = text.lines().next().unwrap_or("").chars().take(40).collect();
                self.push(
                    format!(
                        "📋 已排队 {}（依次自动发送 · /queue 管理 · ctrl+c 只打断当前）：{head}",
                        self.local_queue.len()
                    ),
                    Tone::Muted,
                );
            }
            KeyCode::Enter => {}
            KeyCode::Char(c) if !ctrl && (!self.vim || self.edit_mode == EditMode::Insert) => {
                self.insert_char(c);
                // other sessions start while this one runs: refetch the /send /resume
                // candidates as the argument begins (the popup fills in on the reply)
                if c == ' ' {
                    let t: String = self.input.iter().collect();
                    if (t == "/send " || t == "/resume ")
                        && !self.pending.iter().any(|(_, k)| matches!(k, Pending::ArgSessions))
                    {
                        let id = serve.command("sessions", vec![]);
                        self.pending.push((id, Pending::ArgSessions));
                    }
                }
                self.refresh_popup();
            }
            KeyCode::Backspace => {
                self.backspace_input();
                self.refresh_popup();
            }
            KeyCode::Delete => {
                self.delete_input();
                self.refresh_popup();
            }
            KeyCode::Left => self.move_input_cursor(true),
            KeyCode::Right => self.move_input_cursor(false),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            _ => {}
        }
    }

    /// The rule for one `; && || |` segment of a bash command: the command word
    /// plus its subcommand for known multi-word tools (`git branch -D x` →
    /// `bash:git branch`; `rm -rf /tmp` → `bash:rm`).
    fn bash_segment_rule(seg: &str) -> Option<String> {
        const SUBCOMMAND_TOOLS: &[&str] = &[
            "git", "npm", "pnpm", "yarn", "bun", "npx", "pip", "pip3", "uv", "brew", "cargo", "go",
            "docker", "kubectl", "helm", "apt", "apt-get", "dnf", "yum", "systemctl", "service",
            "gh", "tango",
        ];
        let mut toks = seg.split_whitespace();
        // leading `NAME=value` assignments are settings, not the command
        // (`F="a.docx" && python3 …` is a python3 segment, not an `F=` one)
        let cmd = loop {
            let t = toks.next()?;
            let name = t.split('=').next().unwrap_or("");
            if t.contains('=') && !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            break t;
        };
        let mut rule = cmd.to_string();
        if SUBCOMMAND_TOOLS.contains(&cmd) {
            if let Some(sub) = toks.next().filter(|t| {
                !t.starts_with('-')
                    && t.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
                    && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            }) {
                rule = format!("{cmd} {sub}");
            }
        }
        Some(format!("bash:{rule}"))
    }

    /// The bash command's segments (`; && || |`), heredoc bodies dropped: they are
    /// data written to a file, not commands to remember.
    fn bash_segments(command: &str) -> Vec<String> {
        let stripped = Self::strip_heredocs(command);
        stripped
            .split([';', '|', '&'])
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    }

    /// Drop the body of a heredoc that ends the command (`cat > f <<EOF … EOF`,
    /// the server's stripHeredocs shape): the body is data written to a file, so
    /// its words must never be taken for commands to remember. Other text is kept.
    fn strip_heredocs(cmd: &str) -> String {
        // walk right to left: the last `<<NAME` followed by a newline opens a body
        // that runs to a final `NAME` line
        let mut from = cmd.len();
        while let Some(at) = cmd[..from].rfind("<<") {
            let mut i = at + 2;
            let bytes = cmd.as_bytes();
            if i < bytes.len() && bytes[i] == b'-' {
                i += 1;
            }
            let quote = |c: u8| c == b'\'' || c == b'"';
            let q = i < bytes.len() && quote(bytes[i]);
            if q {
                i += 1;
            }
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let name = &cmd[start..i];
            if q && i < bytes.len() && quote(bytes[i]) {
                i += 1;
            }
            let opens = !name.is_empty() && cmd[i..].starts_with('\n');
            let body = &cmd[i + 1..];
            let terminator = format!("\n{name}");
            let closed = opens
                && body
                    .rfind(&terminator)
                    .is_some_and(|e| body[e + terminator.len()..].trim().is_empty());
            if closed {
                return format!("{}<<{name}", &cmd[..at]);
            }
            // not a heredoc opener that runs to the end: keep looking further left
            from = at;
        }
        cmd.to_string()
    }

    /// The rules "a 始终允许" saves for a pending permission (Rust mirror of
    /// core/permissions.ts suggestRule): bash → one rule per `; && || |` segment
    /// (`osascript … && rm -f x && python3 …` → `bash:osascript`, `bash:rm`,
    /// `bash:python3`), so the whole command is covered; a write outside cwd →
    /// its directory; anything else → the tool itself. Empty when there is
    /// nothing sensible to remember.
    fn always_rules(p: &Permission) -> Vec<String> {
        if p.name == "bash" {
            let mut out: Vec<String> = Vec::new();
            for seg in Self::bash_segments(&p.detail) {
                if let Some(rule) = Self::bash_segment_rule(&seg)
                    && !out.contains(&rule)
                {
                    out.push(rule);
                }
            }
            return out;
        }
        if matches!(p.name.as_str(), "write" | "edit" | "office_write") {
            let path = p.detail.trim();
            if path.is_empty() {
                return Vec::new();
            }
            let dir = std::path::Path::new(path)
                .parent()
                .and_then(|d| d.to_str())
                .filter(|d| !d.is_empty())
                .unwrap_or(".");
            return vec![format!("path:{dir}")];
        }
        vec![format!("tool:{}", p.name)]
    }

    /// What "a" will remember, for the dialog's hint: "记住 osascript · rm · python3"
    /// (the rule prefixes without their `bash:` kind). None when there is nothing.
    fn always_hint(p: &Permission) -> Option<String> {
        let rules = Self::always_rules(p);
        if rules.is_empty() {
            return None;
        }
        let names: Vec<String> = rules
            .iter()
            .map(|r| r.split_once(':').map_or(r.as_str(), |(_, v)| v).to_string())
            .collect();
        Some(names.join(" · "))
    }

    fn answer(&mut self, serve: &mut Serve, allow: bool) {
        self.ime_restore_prev();
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

    /// The zcode-style permission-mode picker: plan, confirm, ask, auto, readonly,
    /// one line each, the current one marked.
    fn open_mode_picker(&mut self) {
        let modes: [(&str, &str, &str); 5] = [
            ("plan", "💡 只出计划", "只研究、写计划，不改文件也不跑命令"),
            ("confirm", "✋ 每步确认", "改文件、跑命令前都先问"),
            ("ask", "🛡 风险确认（默认）", "项目内的改动直接做，高风险操作先问"),
            ("auto", "⚠ 完全放行", "不再确认，全部直接执行"),
            ("readonly", "🚫 只读", "只看不改，写入和命令都拒绝"),
        ];
        let items = modes
            .into_iter()
            .map(|(id, label, note)| PickItem {
                group: String::new(),
                id: id.into(),
                label: label.into(),
                note: note.into(),
                warn: false,
                current: match id {
                    "plan" => self.plan_mode,
                    other => !self.plan_mode && self.mode == other,
                },
            })
            .collect();
        self.picker = Some(Picker::new(PickerKind::Mode, items));
    }

    /// Shift+Tab's cycle. `auto` is left out: full access only ever comes through its
    /// acknowledgement (`/mode auto` or the picker); from auto the next press goes to plan.
    const MODE_CYCLE: [&'static str; 4] = ["ask", "plan", "confirm", "readonly"];

    /// Shift+Tab: switch to the next mode in
    /// `MODE_CYCLE` right away. The local state moves at once, so quick presses keep stepping
    /// instead of all starting from the same mode; serve's replies then confirm it.
    fn cycle_mode(&mut self, serve: &mut Serve) {
        let now = if self.plan_mode { "plan" } else { self.mode.as_str() };
        let at = Self::MODE_CYCLE.iter().position(|m| *m == now).unwrap_or(0);
        let next = Self::MODE_CYCLE[(at + 1) % Self::MODE_CYCLE.len()];
        if next == "plan" {
            // plan lies over the permission mode, which stays as it was
            let id = serve.command("set_plan", vec![("on", Value::Bool(true))]);
            self.pending.push((id, Pending::Plan));
            self.plan_mode = true;
            return;
        }
        if self.plan_mode {
            let id = serve.command("set_plan", vec![("on", Value::Bool(false))]);
            self.pending.push((id, Pending::Plan));
            self.plan_mode = false;
        }
        let id = serve.command("set_mode", vec![("mode", Value::String(next.into()))]);
        self.pending.push((id, Pending::Mode));
        self.mode = next.into();
    }

    fn send_prompt(&mut self, serve: &mut Serve) {
        let (text, images) = self.take_submission();
        // Tab's lead-in ("关于…Tango 在等你选择: ") frames the answer in the box; the
        // model gets the answer alone — and nothing goes out while it is still empty
        let (bare, led) = match Self::split_tab_lead(text.trim_start()) {
            (bare, true) => (bare.trim().to_string(), true),
            _ => (text.clone(), false),
        };
        if led && bare.is_empty() && images.is_empty() {
            self.toast("请写下你的回答，或直接选一个选项");
            return;
        }
        let text = if led { bare } else { text };
        if text.trim().is_empty() && images.is_empty() {
            return;
        }
        // sending from a recalled entry: the unsent draft returns to the box after
        let draft = self.take_history_draft();
        self.clear_input();
        if text.trim().starts_with('/') || text.trim() == ":q" {
            self.run_slash(&text, serve);
            self.restore_draft(draft);
            return;
        }
        if self.prompt_history.last() != Some(&text) {
            self.prompt_history.push(text.clone());
        }
        let (expanded, warns) = Self::expand_at(&text, self.serve_cwd.as_deref());
        for w in warns {
            self.push(format!("! {w}"), Tone::Muted);
        }
        // pin what we're working on: first line of the ask, capped for the status bar
        let first = text.lines().next().unwrap_or("").to_string();
        self.current_prompt = Some(first.chars().take(48).collect());
        self.push_user(&text);
        self.jump_to_bottom();
        self.busy = true;
        self.prompt_id = serve.command("prompt", prompt_extras(expanded, images));
        if let Some(line) = self.lines.last_mut() {
            line.prompt_state = Some(PromptState::Running);
        }
        self.restore_draft(draft);
    }

    fn frame(&mut self, v: Value, serve: &mut Serve) {
        let t = v.get("type").and_then(Value::as_str).unwrap_or("");
        match t {
            "hello" => {
                self.keys = Keymap::from_json(v.get("keys"));
                if self.vim_override.is_none() {
                    // /vim's process-wide choice wins over the persisted config on respawns
                    self.set_vim(v["editor"].as_str() == Some("vim"));
                }
                self.nvim_preference = v["nvimViewer"].as_str().unwrap_or("ask").into();
                self.theme = Theme::resolve(
                    v["theme"].as_str(),
                    std::env::var("COLORFGBG").ok().as_deref(),
                    crate::termbg::detected(),
                );
                self.mouse = v["mouse"].as_bool().unwrap_or(true);
                self.statusline = v["statusline"].as_str().map(|t| t.to_string());
                self.mouse_capture = Some(self.mouse);
                self.model = v["model"].as_str().unwrap_or("?").into();
                self.effort = v["effort"].as_str().unwrap_or("off").into();
                self.remember_effort();
                self.efforts = Self::string_array(&v["efforts"]);
                self.mode = v["mode"].as_str().unwrap_or("?").into();
                self.serve_cwd = v["cwd"].as_str().map(Into::into);
                self.load_notes();
                self.session = short_session(v["sessionId"].as_str().unwrap_or("?")).to_string();
                self.session_full = v["sessionId"].as_str().unwrap_or("").to_string();
                self.serve_branch = v["branch"].as_str().unwrap_or("").to_string();
                self.session_name = v["sessionName"].as_str().unwrap_or("").to_string();
                self.user_bubble = v["bubble"].as_bool().unwrap_or(true);
                self.animations = v["animations"].as_bool().unwrap_or(true);
                self.ascii = v["ascii"].as_bool().unwrap_or(false);
                crate::clipboard::set_mode(
                    v["clipboard"]
                        .as_str()
                        .and_then(crate::clipboard::Mode::parse)
                        .unwrap_or(crate::clipboard::Mode::Auto),
                );
                self.cost = v["totalCostUsd"].as_f64().unwrap_or(0.0);
                self.context_tokens = v["contextTokens"].as_u64().unwrap_or(0);
                self.context_window = v["contextWindow"].as_u64().unwrap_or(0);
                let n = v["tools"].as_array().map_or(0, |a| a.len());
                self.push(
                    format!("tango serve · session {} · {n} tools · /help · esc/ctrl+c interrupt · /exit quits", self.session),
                    Tone::Muted,
                );
                if let Some(prev) = self.nav_back.last() {
                    let short = short_session(prev).to_string();
                    self.push(format!("← /back 返回会话 {short}"), Tone::Muted);
                }
                // `tango -r` with no id: pick the session first (Esc in the picker leaves
                // you in the new session that just opened)
                if self.start_picker {
                    self.start_picker = false;
                    self.run_slash("/resume", serve);
                }
            }
            "stream" => {
                let ev = &v["event"];
                let d = ev["delta"].as_str().unwrap_or("");
                match ev["type"].as_str() {
                    Some("text") => {
                        self.stream.push_str(d);
                        self.turn_chars += d.chars().count() as u64;
                        self.last_output = Some(std::time::Instant::now());
                    }
                    Some("thinking") => {
                        self.thinking.push_str(d);
                        self.turn_chars += d.chars().count() as u64;
                        self.last_output = Some(std::time::Instant::now());
                    }
                    Some("tool_call_start") => {
                        let id = ev["id"].as_str().unwrap_or_default().to_string();
                        self.running_tools.insert(id.clone());
                        self.push(format!("⚙ {} …", ev["name"].as_str().unwrap_or("?")), Tone::Muted);
                        if let Some(line) = self.lines.last_mut() {
                            line.tool_id = Some(id);
                        }
                    }
                    _ => {}
                }
            }
            "message" => self.commit_stream(),
            // a running `task` sub-agent took a step: its live line shows the latest one
            // (`⚙ task explore · 第3步 grep 英智`); the result line replaces it when it ends
            "task_progress" => {
                self.last_output = Some(std::time::Instant::now());
                let id = v["callId"].as_str().unwrap_or_default();
                let detail: String = v["detail"].as_str().unwrap_or("").chars().take(80).collect();
                let text = format!(
                    "⚙ task {} · 第{}步 {} {}",
                    v["agent"].as_str().unwrap_or("?"),
                    v["step"].as_u64().unwrap_or(0),
                    v["tool"].as_str().unwrap_or("?"),
                    detail
                );
                if let Some(line) = self
                    .lines
                    .iter_mut()
                    .rev()
                    .find(|l| !id.is_empty() && l.tool_id.as_deref() == Some(id))
                {
                    line.text = text.trim_end().to_string();
                }
            }
            // chat-line notices from serve (warnings, goal/subtask status) — never part of a stream
            // serve found trust-gated keys in an undecided project: ask once
            "trust_request" => {
                let strs = |k: &str| -> Vec<String> {
                    v[k].as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default()
                };
                let root = v["root"].as_str().unwrap_or("?").to_string();
                if self.picker.is_none() {
                    self.ask_trust(&root, &strs("keys"), &strs("files"));
                } else {
                    self.push(
                        format!("项目 {root} 的配置待信任 · /trust 处理"),
                        Tone::Muted,
                    );
                }
            }
            "notice" => {
                if let Some(text) = v["text"].as_str() {
                    let from = self.lines.len();
                    let tone = if text.starts_with("⑂ subagent ") { Tone::Subagent } else { Tone::Muted };
                    self.push(text, tone);
                    for line in &mut self.lines[from..] {
                        line.notice = true;
                    }
                    self.jump_to_bottom();
                }
            }
            "tool_result" => {
                self.last_output = Some(std::time::Instant::now());
                let id = v["call"]["id"].as_str().unwrap_or_default().to_string();
                if !id.is_empty() {
                    self.running_tools.remove(&id);
                }
                let name = v["call"]["name"].as_str().unwrap_or("?");
                let (summary, call) = Self::call_brief(&v["call"]["input"]);
                let ms = v["ms"].as_u64().unwrap_or(0);
                let ok = !v["isError"].as_bool().unwrap_or(false);
                let interrupted = !ok && Self::interrupted_output(v["output"].as_str());
                // a failed call is one warning-colored line like an interrupted one: the
                // output stays behind double-click / ctrl+o instead of a red block
                let style = if ok { Tone::Muted } else { Tone::Warning };
                let full = v["output"].as_str().map(|s| {
                    let mut out = String::new();
                    for line in s.lines().take(200) {
                        out.push_str(line);
                        out.push('\n');
                    }
                    out.truncate(out.trim_end().len());
                    out
                });
                let note = if ok {
                    ""
                } else if interrupted {
                    " · 已中断"
                } else {
                    " · 出错 · 双击查看"
                };
                let text = format!("⚙ {name} {summary} · {ms}ms{note}");
                // freeze the RUNNING line into its summary in place: one line per call,
                // and the animation settles exactly where it played. A result without
                // a seen start (resumed session) still appends a line.
                let hit = self
                    .lines
                    .iter()
                    .rposition(|l| !id.is_empty() && l.tool_id.as_deref() == Some(id.as_str()));
                let frame = Some((format!("⚙ {name} "), format!(" · {ms}ms{note}")));
                if let Some(i) = hit {
                    let line = &mut self.lines[i];
                    line.text = text;
                    line.tone = style;
                    line.output = full;
                    line.tool_id = None;
                    line.call = Some(call);
                    line.frame = frame;
                } else {
                    self.push_tool(text, style, full);
                    if let Some(line) = self.lines.last_mut() {
                        line.call = Some(call);
                        line.frame = frame;
                    }
                }
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
                    brief: v["brief"].as_str().unwrap_or("").to_string(),
                });
                self.perm_scroll = 0;
                self.ime_for_dialog(); // y/n/a need a roman keyboard through a CJK IME
            }
            "turn_end" => {
                self.running_tools.clear();
                if let Some(c) = v["totalCostUsd"].as_f64() {
                    self.cost = c;
                }
                self.context_tokens = v["contextTokens"]
                    .as_u64()
                    .or_else(|| v["usage"]["input"].as_u64())
                    .unwrap_or(self.context_tokens);
                self.context_window = v["contextWindow"].as_u64().unwrap_or(self.context_window);
            }
            "btw_stream" => {
                let btw_title = self.btw_title.clone();
                if self.btw_id == v["id"].as_u64()
                    && v["event"]["type"].as_str() == Some("text")
                    && let Some(viewer) = self.viewer.as_mut()
                    && btw_title.as_deref() == Some(viewer.title.as_str())
                {
                    viewer
                        .text
                        .push_str(v["event"]["delta"].as_str().unwrap_or(""));
                }
            }
            "result" => {
                let id = v["id"].as_u64().unwrap_or(0);
                let ok = v["ok"].as_bool().unwrap_or(false);
                if id == self.prompt_id {
                    self.busy = false;
                    self.turn_started = None;
                    self.turn_chars = 0;
                    self.current_prompt = None; // the pin retires with the turn
                    // the local queue drains in order: the next ask goes out now
                    if let Some(next) = self.local_queue.first().cloned() {
                        self.local_queue.remove(0);
                        self.submit_text(serve, next.text, next.images);
                        // the last queued ask just went out: an open /queue dialog has nothing left
                        if self.local_queue.is_empty() {
                            self.queue_dialog = false;
                            self.queue_buttons.clear();
                        }
                    }
                    // auto recap: the answer's whole first line, so a glance reconstructs
                    // what this long turn actually produced. Never cut: the chat wraps it,
                    // and a cut mid-sentence ("…同事如果现在就看这份文档，可") reads as a bug
                    if let Some(last) = self.assistant_replies.last() {
                        let first = last.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                        let head = first.trim();
                        if !head.is_empty() {
                            self.push(format!("✓ 本轮：{head}"), Tone::Muted);
                        }
                    }
                    for line in &mut self.lines {
                        if line.prompt_state == Some(PromptState::Running) {
                            line.prompt_state = Some(PromptState::Completed);
                        }
                    }
                    // turn done and nothing queued: an un-handled suggestion takes
                    // the panel with 接受/忽略
                    if !self.busy {
                        self.arm_pending_panel();
                    }
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
                    if ok {
                        self.context_tokens =
                            d["contextTokens"].as_u64().unwrap_or(self.context_tokens);
                        self.context_window =
                            d["contextWindow"].as_u64().unwrap_or(self.context_window);
                    }
                    match (kind, ok) {
                        (Pending::ArgModels, ok) => {
                            if ok {
                                self.arg_models = d["models"]
                                    .as_array()
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|m| m["id"].as_str().map(String::from))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                            }
                        }
                        (Pending::ArgSessions, ok) => {
                            if ok {
                                self.arg_sessions = d["sessions"]
                                    .as_array()
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|s| {
                                                let id = s["id"].as_str()?.to_string();
                                                let name = s["name"].as_str().unwrap_or("");
                                                let topic = s["topic"].as_str().unwrap_or("");
                                                let what = if name.is_empty() { topic } else { name };
                                                let label = format!(
                                                    "{} · {}",
                                                    s["ago"].as_str().unwrap_or(""),
                                                    what.chars().take(40).collect::<String>()
                                                );
                                                Some((id, label, s["current"].as_bool().unwrap_or(false)))
                                            })
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                // fresh list while "/send " is still being typed: show it
                                self.refresh_popup();
                            }
                        }
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
                                                group: m["platform"]
                                                    .as_str()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                id,
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            // grok's model picker: each model shows its reasoning level, ←→ changes
                            // it for the highlighted row, Enter switches to both
                            let mut picker = Picker::new(PickerKind::Model, items);
                            picker.efforts = d["models"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|m| {
                                            Some((m["id"].as_str()?.to_string(), Self::string_array(&m["efforts"])))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            // what was chosen before: the level in use now, and the ones kept for other models
                            picker.effort_pick = self.effort_memory.clone();
                            if self.effort != "off" {
                                picker.effort_pick.insert(self.model.clone(), self.effort.clone());
                            }
                            self.picker = Some(picker);
                        }
                        (Pending::Sessions, true) => {
                            let items: Vec<PickItem> = d["sessions"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|s| {
                                            let id = s["id"].as_str()?.to_string();
                                            // the first user prompt is the identity;
                                            // uuid prefixes never told anyone anything
                                            let topic = s["topic"].as_str().unwrap_or("");
                                            let date = {
                                                // relative time when serve sends it ("3 小时前"),
                                                // the raw stamp as fallback
                                                let ago = s["ago"].as_str().unwrap_or("");
                                                if ago.is_empty() {
                                                    s["createdAt"]
                                                        .as_str()
                                                        .unwrap_or("")
                                                        .replace('T', " ")
                                                } else {
                                                    ago.to_string()
                                                }
                                            };
                                            let label = if topic.is_empty() {
                                                format!(
                                                    "{} · {} msgs · {}",
                                                    s["id"]
                                                        .as_str()
                                                        .unwrap_or("")
                                                        .chars()
                                                        .take(8)
                                                        .collect::<String>(),
                                                    s["messages"].as_u64().unwrap_or(0),
                                                    date
                                                )
                                            } else {
                                                topic.to_string()
                                            };
                                            let cwd = s["cwd"].as_str().unwrap_or("");
                                            let cwd_base = cwd.rsplit('/').next().unwrap_or(cwd);
                                            let name = s["name"].as_str().unwrap_or("");
                                            Some(PickItem {
                                                group: String::new(),
                                                note: format!(
                                                    "{} · {} 条 · {}",
                                                    if name.is_empty() { cwd_base } else { name },
                                                    s["messages"].as_u64().unwrap_or(0),
                                                    date
                                                ),
                                                warn: false,
                                                current: s["current"].as_bool().unwrap_or(false),
                                                label,
                                                id,
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            if items.is_empty() {
                                self.push("no sessions yet", Tone::Muted);
                            } else {
                                let kind = self.session_pick.take().unwrap_or(PickerKind::Session);
                                // the current session is never a handoff/takeover partner
                                let items: Vec<PickItem> = if kind == PickerKind::Session {
                                    items
                                } else {
                                    items.into_iter().filter(|i| !i.current).collect()
                                };
                                if items.is_empty() {
                                    self.push("no other sessions", Tone::Muted);
                                } else {
                                    let mut picker = Picker::new(kind, items);
                                    picker.cwd = self
                                        .serve_cwd
                                        .as_ref()
                                        .map(|p| p.display().to_string())
                                        .unwrap_or_default();
                                    picker.branch = self.serve_branch.clone();
                                    picker.scopes = d["sessions"]
                                        .as_array()
                                        .map(|a| {
                                            a.iter()
                                                .filter_map(|s| {
                                                    Some((
                                                        s["id"].as_str()?.to_string(),
                                                        s["cwd"].as_str().unwrap_or("").to_string(),
                                                        s["branch"]
                                                            .as_str()
                                                            .unwrap_or("")
                                                            .to_string(),
                                                    ))
                                                })
                                                .collect()
                                        })
                                        .unwrap_or_default();
                                    self.picker = Some(picker);
                                }
                            }
                        }
                        (Pending::TimelineMenu, true) => {
                            // locate the clicked bubble's turn by its first line,
                            // then editlast straight to that entry
                            let head = self.edit_target_head.clone().unwrap_or_default();
                            let hhead: String = head.trim().chars().take(8).collect();
                            let hit = d["nodes"].as_array().and_then(|nodes| {
                                nodes.iter().find_map(|n| {
                                    let f = n["firstLine"].as_str()?.trim();
                                    // match in either direction over the shorter
                                    // prefix: serve truncates at 80, we hold 40
                                    let n8: String = f.chars().take(8).collect();
                                    let matched = (n8.len() <= hhead.len()
                                        && hhead.starts_with(&n8))
                                        || (hhead.len() <= n8.len() && n8.starts_with(&hhead));
                                    matched.then(|| n["entryIndex"].as_u64().unwrap_or(0))
                                })
                            });
                            match hit {
                                Some(entry) => {
                                    let id = serve.command(
                                        "editlast",
                                        vec![("entryIndex", Value::from(entry))],
                                    );
                                    self.pending.push((id, Pending::EditLast));
                                }
                                None => {
                                    self.push(
                                        "没在时间轴上找到这条消息（可能已被回滚）",
                                        Tone::Error,
                                    );
                                }
                            }
                        }
                        (Pending::EditLast, true) => {
                            if let Some(prompt) = d["prompt"].as_str() {
                                self.input = prompt.chars().collect();
                                self.cursor = self.input.len();
                                self.push(
                                    "✎ 原句已载入输入框，修改后回车重发（该回合已回滚）",
                                    Tone::Success,
                                );
                            } else {
                                self.push("editlast: 没有可编辑的回合", Tone::Muted);
                            }
                        }
                        (Pending::ResumeResolve, true) => {
                            if let Some(full) = d["id"].as_str() {
                                self.push(format!("↻ resuming session {full} …"), Tone::Muted);
                                self.switch_session(Some(full.to_string()));
                            }
                        }
                        (Pending::SessionPreview, true) => {
                            if let Some(preview) = d["preview"].as_str() {
                                let head: String = preview
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .chars()
                                    .take(40)
                                    .collect();
                                let v = crate::viewer::Viewer::read_only(
                                    format!("会话预览 · {head}"),
                                    preview.to_string(),
                                );
                                self.request_viewer(v);
                            }
                        }
                        (Pending::Model, true) => {
                            self.model = d["model"].as_str().unwrap_or("?").into();
                            self.effort = d["effort"].as_str().unwrap_or("off").into();
                            self.remember_effort();
                            self.efforts = Self::string_array(&d["efforts"]);
                            self.push(
                                format!("model → {} (saved as default)", self.model),
                                Tone::Muted,
                            );
                            if let Some(w) = d["warning"].as_str() {
                                self.push(format!("! {w}"), Tone::Warning);
                            }
                            // the level chosen for the previous model does not exist on this one
                            if let Some(old) = d["effortDropped"].as_str() {
                                let choices = self.efforts.join(" · ");
                                self.push(
                                    if self.efforts.is_empty() {
                                        format!("推理力度 {old} 对 {} 不适用，已关闭 · 这个模型没有可选档位", self.model)
                                    } else {
                                        format!("推理力度 {old} 对 {} 不适用，已关闭 · 可选 {choices} · off", self.model)
                                    },
                                    Tone::Warning,
                                );
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
                            // badges follow the serve truth (goal cleared by interrupt etc.)
                            self.goal_text = d["goal"].as_str().map(|g| g.to_string());
                            self.plan_mode = d["plan"].as_bool().unwrap_or(self.plan_mode);
                            if let Some(m) = d["mode"].as_str() {
                                self.mode = m.into();
                            }
                            let t = &d["sessionTokens"];
                            let (tin, tout, tcache) = (
                                t["input"].as_u64().unwrap_or(0),
                                t["output"].as_u64().unwrap_or(0),
                                t["cacheRead"].as_u64().unwrap_or(0),
                            );
                            let cache_note = if tin + tcache > 0 {
                                format!(" · 缓存命中 {}%", tcache * 100 / (tin + tcache))
                            } else {
                                String::new()
                            };
                            self.push(
                                format!(
                                    "session {} · {} · {} · {} msgs · ${:.4} · tokens 入{} 出{}{cache_note} · {}",
                                    d["sessionId"].as_str().unwrap_or("?"),
                                    d["model"].as_str().unwrap_or("?"),
                                    d["mode"].as_str().unwrap_or("?"),
                                    d["contextMessages"].as_u64().unwrap_or(0),
                                    d["totalCostUsd"].as_f64().unwrap_or(0.0),
                                    tin,
                                    tout,
                                    d["cwd"].as_str().unwrap_or("?"),
                                ),
                                Tone::Muted,
                            );
                            if let Some(line) = d["configSources"].as_str() {
                                self.push(line, Tone::Muted);
                            }
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
                        (Pending::Diff, true) => {
                            let title = if d["isError"].as_bool().unwrap_or(false) {
                                "diff 错误"
                            } else if d["staged"].as_bool().unwrap_or(false) {
                                "暂存 diff"
                            } else {
                                "工作区 diff"
                            };
                            self.viewer = Some(Viewer::read_only(
                                title,
                                d["diff"].as_str().unwrap_or("clean — nothing changed"),
                            ));
                        }
                        (Pending::Btw, true) => {
                            self.btw_id = None;
                            self.busy = false;
                            let title = self.btw_title.take().unwrap_or_else(|| "临时提问".into());
                            let text = d["text"].as_str().unwrap_or("");
                            if !text.is_empty() {
                                if let Some(viewer) = self.viewer.as_mut()
                                    && viewer.title == title
                                {
                                    viewer.text = text.into();
                                } else {
                                    // the viewer was closed or replaced mid-flight; the
                                    // answer was paid for, so reopen it rather than drop it
                                    self.viewer = Some(Viewer::read_only(title, text));
                                }
                            }
                        }
                        (Pending::Timeline, true) => {
                            let mut items: Vec<PickItem> = d["nodes"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .enumerate()
                                        .filter_map(|(i, n)| {
                                            let entry = n["entryIndex"].as_u64()?;
                                            let snap = n["hasSnapshot"].as_bool().unwrap_or(false);
                                            let first: String = n["firstLine"]
                                                .as_str()
                                                .unwrap_or("")
                                                .chars()
                                                .take(48)
                                                .collect();
                                            let ts =
                                                n["ts"].as_str().unwrap_or("").replace('T', " ");
                                            let ts: String = ts.chars().take(16).collect();
                                            Some(PickItem {
                                                group: String::new(),
                                                label: format!("#{i} · {ts} · {first}"),
                                                note: if snap {
                                                    "✓ 代码快照".into()
                                                } else {
                                                    "无代码快照（仅对话）".into()
                                                },
                                                warn: false,
                                                current: false,
                                                id: entry.to_string(),
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            // newest first with the cursor on it: a rewind usually goes back a
                            // step or two, and /resume lists newest first too. #i keeps the
                            // chronological index (#0 = oldest) so labels don't shift.
                            items.reverse();
                            self.picker = Some(Picker::new(PickerKind::Timeline, items));
                        }
                        (Pending::Rewind, true) | (Pending::Undo, true) | (Pending::Redo, true) => {
                            let restored = d["codeRestored"].as_bool().unwrap_or(false);
                            let what = match kind {
                                Pending::Undo => "undo",
                                Pending::Redo => "redo",
                                _ => "rewind",
                            };
                            let deleted_names: Vec<String> = d["deleted"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(|f| f.to_string()))
                                        .take(3)
                                        .collect()
                                })
                                .unwrap_or_default();
                            let deleted = d["deleted"].as_array().map(|a| a.len()).unwrap_or(0);
                            let kept = d["kept"].as_array().map(|a| a.len()).unwrap_or(0);
                            let kept_names: Vec<String> = d["kept"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(|f| f.to_string()))
                                        .take(3)
                                        .collect()
                                })
                                .unwrap_or_default();
                            let kept_note = if kept_names.is_empty() {
                                String::new()
                            } else {
                                format!("（{}）", kept_names.join(", "))
                            };
                            let names = if deleted_names.is_empty() {
                                String::new()
                            } else {
                                format!("（{}）", deleted_names.join(", "))
                            };
                            self.push(
                                format!(
                                    "⟲ {what} 完成 · 代码{} · 删除遗留 {deleted} 个文件{names} · 保留用户文件 {kept} 个{kept_note}",
                                    if restored { "已恢复" } else { "未变（无快照）" }
                                ),
                                Tone::Muted,
                            );
                        }
                        (Pending::Fork, true) => {
                            if let Some(id) = d["sessionId"].as_str() {
                                self.push(format!("⑂ fork → {id} …"), Tone::Success);
                                self.switch_session(Some(id.to_string()));
                            }
                        }
                        (Pending::Send, true) => {
                            let to = short_session(d["to"].as_str().unwrap_or("?")).to_string();
                            let live = d["targetLive"]["pid"].as_u64().is_some();
                            self.push(
                                if live {
                                    format!("✉ 消息已发给会话 {to}")
                                } else {
                                    format!("✉ 消息已排队给会话 {to}（未在运行 · 它下次打开时收到）")
                                },
                                Tone::Success,
                            );
                        }
                        (Pending::Handoff, true) => {
                            let target: String = d["target"]
                                .as_str()
                                .unwrap_or("?")
                                .chars()
                                .take(8)
                                .collect();
                            let q = d["queueId"].as_str().unwrap_or("?");
                            self.push(
                                format!("⇢ 交接摘要已发给会话 {target} · 它下次运行时并入上下文（7 天内）"),
                                Tone::Success,
                            );
                            if let Some(pid) = d["targetLive"]["pid"].as_u64() {
                                self.push(
                                    format!("  ⚠ 会话 {target} 正在另一个窗口打开（pid {pid}）· 在那边发下一条消息前并入"),
                                    Tone::Warning,
                                );
                            }
                            self.push(
                                format!("  撤回：/handoff cancel {q} · 查看：/handoff list"),
                                Tone::Muted,
                            );
                        }
                        (Pending::HandoffList, true) => {
                            let list = d["handoffs"].as_array().cloned().unwrap_or_default();
                            if list.is_empty() {
                                self.push("没有排队中的交接摘要", Tone::Muted);
                            }
                            for h in &list {
                                let short = |k: &str| -> String {
                                    short_session(h[k].as_str().unwrap_or("?")).to_string()
                                };
                                let at: String = h["queuedAt"]
                                    .as_str()
                                    .unwrap_or("")
                                    .chars()
                                    .take(16)
                                    .collect();
                                let tag = if h["expired"].as_bool().unwrap_or(false) {
                                    " · 已过期（打开时丢弃）"
                                } else {
                                    ""
                                };
                                self.push(
                                    format!(
                                        "  {}  {} ⇢ {}  {}{tag}",
                                        h["queueId"].as_str().unwrap_or("?"),
                                        short("from"),
                                        short("target"),
                                        at.replace('T', " ")
                                    ),
                                    Tone::Selected,
                                );
                                let first = h["preview"]
                                    .as_str()
                                    .unwrap_or("")
                                    .lines()
                                    .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
                                    .unwrap_or("")
                                    .chars()
                                    .take(80)
                                    .collect::<String>();
                                if !first.is_empty() {
                                    self.push(format!("    {first}"), Tone::Muted);
                                }
                            }
                            if !list.is_empty() {
                                self.push("  撤回：/handoff cancel <queue id>", Tone::Muted);
                            }
                        }
                        (Pending::HandoffCancel, true) => {
                            let target: String = d["target"]
                                .as_str()
                                .unwrap_or("?")
                                .chars()
                                .take(8)
                                .collect();
                            self.push(
                                format!(
                                    "✕ 已撤回交接摘要 {}（目标会话 {target}）",
                                    d["queueId"].as_str().unwrap_or("?")
                                ),
                                Tone::Success,
                            );
                        }
                        (Pending::Takeover, true) => {
                            let from: String =
                                short_session(d["from"].as_str().unwrap_or("?")).to_string();
                            self.push(format!("⇠ 已接管会话 {from} · 摘要："), Tone::Success);
                            if let Some(pid) = d["sourceLive"]["pid"].as_u64() {
                                self.push(
                                    format!("  ⚠ 会话 {from} 仍在另一个窗口打开（pid {pid}）· 摘要只是此刻的快照，那边的后续进展不会同步"),
                                    Tone::Warning,
                                );
                            }
                            self.stream = d["brief"].as_str().unwrap_or("").to_string();
                            self.commit_stream();
                        }
                        (Pending::Branch, true) => {
                            self.push(
                                "⑂ 已就地分叉 · 被放弃的未来保留在日志里，/redo 可找回",
                                Tone::Success,
                            );
                        }
                        (Pending::Tree, true) => {
                            let mut text = String::new();
                            if let Some(nodes) = d["nodes"].as_array() {
                                let ids: Vec<&str> = nodes
                                    .iter()
                                    .filter_map(|n| n["sessionId"].as_str())
                                    .collect();
                                for n in nodes {
                                    let id = n["sessionId"].as_str().unwrap_or("?");
                                    let name = n["name"].as_str().unwrap_or("");
                                    let fork_of = n["forkOf"]["sessionId"].as_str();
                                    let depth = fork_of
                                        .and_then(|p| ids.iter().position(|x| *x == p))
                                        .map(|p| p + 1)
                                        .unwrap_or(0);
                                    let indent: String = "  ".repeat(depth);
                                    let title = if name.is_empty() {
                                        id.to_string()
                                    } else {
                                        format!("{name} · {id}")
                                    };
                                    let current = if depth == 0 { "（根）" } else { "" };
                                    text.push_str(&format!("{indent}{title}{current}\n"));
                                }
                            }
                            self.viewer = Some(Viewer::read_only("会话分叉树", text));
                        }
                        (Pending::Find, true) => {
                            // the full id when serve handshook; `session` alone in old tests
                            let cur = if self.session_full.is_empty() {
                                self.session.clone()
                            } else {
                                self.session_full.clone()
                            };
                            let items: Vec<PickItem> = d["hits"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|h| {
                                            let sid = h["sessionId"].as_str()?;
                                            let entry = h["entryIndex"].as_u64()?;
                                            let orphaned = h["orphaned"].as_bool().unwrap_or(false);
                                            let is_current = sid == cur;
                                            let first: String = h["firstLine"]
                                                .as_str()
                                                .unwrap_or("")
                                                .chars()
                                                .take(48)
                                                .collect();
                                            let snippet: String = h["snippet"]
                                                .as_str()
                                                .unwrap_or("")
                                                .chars()
                                                .take(60)
                                                .collect();
                                            let tag = if orphaned { "⟲被放弃 " } else { "" };
                                            let which =
                                                if is_current { "本会话" } else { "他会话" };
                                            Some(PickItem {
                                                group: String::new(),
                                                label: format!("[{which}] {tag}{first}"),
                                                note: snippet,
                                                warn: false,
                                                current: false,
                                                id: format!(
                                                    "{sid}:{entry}:{}",
                                                    if is_current { 1 } else { 0 }
                                                ),
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            if items.is_empty() {
                                self.push("find: 无命中", Tone::Muted);
                            } else {
                                self.picker = Some(Picker::new(PickerKind::Find, items));
                            }
                        }
                        (Pending::Export, true) => {
                            let path = d["path"].as_str().unwrap_or("?");
                            let format = d["format"].as_str().unwrap_or("md");
                            self.push(format!("已导出 {format} → {path}"), Tone::Success);
                        }
                        (Pending::Transcript, true) => {
                            let text = d["text"].as_str().unwrap_or("").to_string();
                            self.viewer =
                                Some(Viewer::read_only("完整 transcript · ⟲=被放弃的未来", text));
                        }
                        (Pending::Rename, true) => {
                            let name = d["name"].as_str().unwrap_or("").to_string();
                            self.session = name;
                            self.push("会话已重命名", Tone::Success);
                        }
                        (Pending::Recap, true) => {
                            self.btw_id = None;
                            self.busy = false;
                            let title = self.btw_title.take().unwrap_or_else(|| "一行总结".into());
                            let text = d["text"].as_str().unwrap_or("");
                            if !text.is_empty() {
                                if let Some(viewer) = self.viewer.as_mut()
                                    && viewer.title == title
                                {
                                    viewer.text = text.into();
                                } else {
                                    self.viewer = Some(Viewer::read_only(title, text));
                                }
                            }
                        }
                        (Pending::Context, true) => {
                            let g = |k: &str| d[k].as_u64().unwrap_or(0);
                            let total = g("total").max(1);
                            let window = g("window").max(1);
                            let bar = |tok: u64| {
                                let width = ((tok as f64 / total as f64) * 24.0).round() as usize;
                                format!("{}{}", "█".repeat(width), "·".repeat(24 - width.min(24)))
                            };
                            let pct = d["pct"].as_u64().unwrap_or(0);
                            let text = format!(
                                "上下文构成（估算 token · chars/3.5）\n\n\
                                system   {:>7}  {}\n\
                                skills   {:>7}  {}\n\
                                tools    {:>7}  {}\n\
                                history  {:>7}  {}\n\
                                ─────────────────────\n\
                                合计     {:>7}  / 窗口 {}  ({pct}%)\n\
                                {}",
                                g("system"),
                                bar(g("system")),
                                g("skills"),
                                bar(g("skills")),
                                g("tools"),
                                bar(g("tools")),
                                g("history"),
                                bar(g("history")),
                                total,
                                window,
                                if pct >= 80 {
                                    "⚠ 已超 80%，建议 /compact"
                                } else if pct >= 60 {
                                    "⚠ 已超 60%"
                                } else {
                                    ""
                                }
                            );
                            self.viewer = Some(Viewer::read_only("上下文构成", text));
                        }
                        (Pending::Usage, true) => {
                            let tt = &d["totalTokens"];
                            let mut text = format!(
                                "总花费 ${:.4} · tokens 入 {} · 出 {}\n\n按模型：\n",
                                d["totalCostUsd"].as_f64().unwrap_or(0.0),
                                tt["input"].as_u64().unwrap_or(0),
                                tt["output"].as_u64().unwrap_or(0),
                            );
                            if let Some(models) = d["byModel"].as_array() {
                                for m in models {
                                    text.push_str(&format!(
                                        "  {:<24} 请求 {:>4} · 输入 {:>7} · 输出 {:>6} · ${:.4}\n",
                                        m["model"].as_str().unwrap_or("?"),
                                        m["requests"].as_u64().unwrap_or(0),
                                        m["input"].as_u64().unwrap_or(0),
                                        m["output"].as_u64().unwrap_or(0),
                                        m["costUsd"].as_f64().unwrap_or(0.0)
                                    ));
                                }
                            }
                            text.push_str("\n按会话：\n");
                            if let Some(sessions) = d["bySession"].as_array() {
                                for x in sessions {
                                    let name = x["name"].as_str().unwrap_or("");
                                    text.push_str(&format!(
                                        "  {} {} · 消息 {:>4} · ${:.4}\n",
                                        x["id"]
                                            .as_str()
                                            .unwrap_or("?")
                                            .chars()
                                            .take(8)
                                            .collect::<String>(),
                                        if name.is_empty() { "" } else { name },
                                        x["messages"].as_u64().unwrap_or(0),
                                        x["costUsd"].as_f64().unwrap_or(0.0)
                                    ));
                                }
                            }
                            self.viewer = Some(Viewer::read_only("使用统计", text));
                        }
                        (Pending::Insights, true) => {
                            let path = d["path"].as_str().unwrap_or("?");
                            self.push(format!("insights → {path}"), Tone::Success);
                        }
                        (Pending::AutocompactShow, true) => {
                            let v = d["autocompact"].as_f64().unwrap_or(0.75);
                            let desc = if v <= 0.0 {
                                "已关闭".to_string()
                            } else {
                                format!("上下文达窗口 {:.0}% 时触发", v * 100.0)
                            };
                            self.push(
                                format!("自动压缩：{desc} · /autocompact <N%|0.N|off>"),
                                Tone::Muted,
                            );
                        }
                        (Pending::AutocompactSet, true) => {
                            let v = d["autocompact"].as_f64().unwrap_or(0.0);
                            if v > 0.0 {
                                self.push(
                                    format!("自动压缩阈值 → {:.0}%", v * 100.0),
                                    Tone::Success,
                                );
                            } else {
                                self.push("自动压缩已关闭", Tone::Muted);
                            }
                        }
                        (Pending::Statusline, true) => {
                            if let Some(t) = d["template"].as_str() {
                                self.statusline = Some(t.to_string());
                                self.push(format!("状态栏模板已保存并应用"), Tone::Success);
                            } else {
                                self.statusline = None;
                                self.push("状态栏已恢复默认", Tone::Muted);
                            }
                        }
                        (Pending::PresetSave, true) => {
                            self.push(
                                format!(
                                    "模板已保存（输入框已清空）· /preset {} 可取回",
                                    d["name"].as_str().unwrap_or("")
                                ),
                                Tone::Success,
                            );
                        }
                        (Pending::PresetList, true) => {
                            let items: Vec<PickItem> = d["presets"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|p| {
                                            Some(PickItem {
                                                group: String::new(),
                                                label: p["name"].as_str()?.to_string(),
                                                note: p["preview"]
                                                    .as_str()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                warn: false,
                                                current: false,
                                                id: p["name"].as_str()?.to_string(),
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            if items.is_empty() {
                                self.push(
                                    "暂无模板 · /preset save <name> 保存当前输入",
                                    Tone::Muted,
                                );
                            } else {
                                self.picker = Some(Picker::new(PickerKind::Preset, items));
                            }
                        }
                        (Pending::PresetGet, true) => {
                            if let Some(text) = d["text"].as_str() {
                                self.input = text.chars().collect();
                                self.cursor = self.input.len();
                                self.push(
                                    "模板已填入（未发送）· 检查后 ctrl+s 发送",
                                    Tone::Success,
                                );
                            }
                        }
                        (Pending::PermissionsList, true) => {
                            let items: Vec<PickItem> = d["allow"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|r| {
                                            Some(PickItem {
                                                group: String::new(),
                                                label: r["rule"].as_str()?.to_string(),
                                                note: r["note"].as_str().unwrap_or("").to_string(),
                                                warn: false,
                                                current: false,
                                                id: r["rule"].as_str()?.to_string(),
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            if items.is_empty() {
                                self.push(
                                    "暂无允许规则 · /permissions add tool:diff|path:src|bash:git status",
                                    Tone::Muted,
                                );
                            } else {
                                self.picker = Some(Picker::new(PickerKind::Permissions, items));
                            }
                        }
                        (Pending::PermissionsAdd, true) => {
                            self.push(
                                format!(
                                    "规则已添加并生效（共 {} 条）→ {}",
                                    d["allow"].as_array().map(|a| a.len()).unwrap_or(0),
                                    d["path"].as_str().unwrap_or("config.json")
                                ),
                                Tone::Success,
                            );
                        }
                        // a later segment's rule of the same "a" press: saved, not re-announced
                        (Pending::PermissionsAddQuiet, true) => {}

                        (Pending::Import, true) => {
                            let imported: Vec<&str> = d["imported"]
                                .as_array()
                                .map(|a| a.iter().filter_map(Value::as_str).collect())
                                .unwrap_or_default();
                            let mut text = format!("已导入 {} 条允许规则", imported.len());
                            for r in &imported {
                                text.push_str(&format!("\n  + {r}"));
                            }
                            let skipped = d["skipped"].as_array().cloned().unwrap_or_default();
                            if !skipped.is_empty() {
                                text.push_str(&format!("\n未能转换 {} 条：", skipped.len()));
                                for s in &skipped {
                                    text.push_str(&format!(
                                        "\n  - {} （{}）",
                                        s["raw"].as_str().unwrap_or("?"),
                                        s["reason"].as_str().unwrap_or("?")
                                    ));
                                }
                            }
                            let tone = if imported.is_empty() {
                                Tone::Muted
                            } else {
                                Tone::Success
                            };
                            self.push(text, tone);
                        }
                        (Pending::PermissionsRemove, true) => {
                            self.push("规则已移除", Tone::Muted);
                        }
                        (Pending::Trust, true) => {
                            let yes = d["trusted"].as_bool().unwrap_or(false);
                            let root = d["root"].as_str().unwrap_or("?");
                            self.push(
                                format!(
                                    "{} {root}",
                                    if yes {
                                        "✓ 已信任"
                                    } else {
                                        "✗ 不信任"
                                    }
                                ),
                                Tone::Success,
                            );
                            if d["reload"].as_bool().unwrap_or(false) {
                                // hooks / MCP / rules are wired at startup: respawn serve, same session
                                if self.busy || self.session_full.is_empty() {
                                    self.push("回合结束后 /restart 生效", Tone::Muted);
                                } else {
                                    self.restart =
                                        Some((self.session_full.clone(), self.mode.clone()));
                                    self.quit = true;
                                }
                            }
                        }
                        (Pending::TrustStatus, true) => {
                            self.push(d["line"].as_str().unwrap_or("配置：?"), Tone::Muted);
                            let gated: Vec<String> = d["gated"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(String::from))
                                        .collect()
                                })
                                .unwrap_or_default();
                            match d["root"].as_str() {
                                None => self.push("当前目录没有项目配置层", Tone::Muted),
                                Some(_) if gated.is_empty() => {
                                    self.push("项目配置里没有需要信任的项（hooks/permissions/mcpServers/extraDirs）", Tone::Muted)
                                }
                                Some(root) => {
                                    let root = root.to_string();
                                    self.ask_trust(&root, &gated, &[]);
                                }
                            }
                        }
                        (Pending::ConfigShow, true) => {
                            let path = d["path"].as_str().unwrap_or("?").to_string();
                            let cfg =
                                serde_json::to_string_pretty(&d["config"]).unwrap_or_default();
                            // layered: list every file that fed the merged view
                            let mut head = String::new();
                            if let Some(line) = d["line"].as_str() {
                                head.push_str(line);
                                head.push('\n');
                            }
                            for src in d["sources"].as_array().into_iter().flatten() {
                                let mark = if src["error"].is_string() {
                                    "✗"
                                } else if src["exists"].as_bool().unwrap_or(false) {
                                    "✓"
                                } else {
                                    "—"
                                };
                                head.push_str(&format!(
                                    "  {mark} {:<7} {}\n",
                                    src["scope"].as_str().unwrap_or("?"),
                                    src["path"].as_str().unwrap_or("?")
                                ));
                            }
                            if head.is_empty() {
                                head = format!("{path}\n");
                            }
                            self.viewer = Some(Viewer::read_only(
                                "配置",
                                format!(
                                    "{head}\n合并结果：\n{cfg}\n\n（全局 {path}；项目共享 .tango/config.json 随仓库提交，个人 .tango/config.local.json 不进 git）"
                                ),
                            ));
                        }
                        (Pending::Pwd, true) => {
                            let cwd = d["cwd"].as_str().unwrap_or("?");
                            self.push(format!("cwd {cwd}"), Tone::Muted);
                        }
                        (Pending::Cd, true) => {
                            let cwd = d["cwd"].as_str().unwrap_or("?").to_string();
                            self.serve_cwd = Some(std::path::PathBuf::from(&cwd));
                            self.serve_branch = d["branch"].as_str().unwrap_or("").to_string();
                            // notes are per directory: switch to the new directory's
                            self.todos
                                .retain(|t| !matches!(t.kind, TodoKind::Note { .. }));
                            self.notes_loaded = false;
                            self.load_notes();
                            // problem paths are relative to the directory just left
                            self.problems.clear();
                            self.problems_sel = 0;
                            self.push(format!("cwd → {cwd}"), Tone::Success);
                        }
                        (Pending::Cwds, true) => {
                            let dirs = d["dirs"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|v| v.as_str().map(str::to_string))
                                        .collect::<Vec<_>>()
                                })
                                .unwrap_or_default();
                            self.open_cwd_picker(&dirs);
                        }
                        (Pending::AddDir, true) => {
                            let n = d["extraDirs"].as_array().map(|a| a.len()).unwrap_or(0);
                            let path = d["path"].as_str().unwrap_or("config.json");
                            self.push(
                                format!("额外可写目录已保存（共 {n} 个）→ {path}"),
                                Tone::Success,
                            );
                        }
                        (Pending::Init, true) => {
                            let path = d["path"].as_str().unwrap_or("AGENTS.md");
                            self.push(format!("已生成 {path} · 编辑补全后自动生效"), Tone::Success);
                        }
                        (Pending::Doctor, true) => {
                            let mut text = String::from("环境体检：\n\n");
                            if let Some(checks) = d["checks"].as_array() {
                                for c in checks {
                                    let ok = c["ok"].as_bool().unwrap_or(false);
                                    let mark = if ok { "✓" } else { "✗" };
                                    let name = c["name"].as_str().unwrap_or("?");
                                    let detail = c["detail"].as_str().unwrap_or("");
                                    text.push_str(&format!("  {mark} {name}  {detail}\n"));
                                }
                            }
                            self.viewer = Some(Viewer::read_only("doctor", text));
                        }
                        (Pending::DebugZip, true) => {
                            let path = d["path"].as_str().unwrap_or("?");
                            self.push(format!("诊断包 → {path}（不含密钥）"), Tone::Success);
                        }
                        (Pending::Plan, true) => {
                            self.plan_mode = d["plan"].as_bool().unwrap_or(false);
                            // roles.plan may have switched the model (and switches it back)
                            if let Some(m) = d["model"].as_str()
                                && m != self.model
                            {
                                self.model = m.into();
                                self.push(format!("模型 → {m}"), Tone::Muted);
                            }
                            if let Some(w) = d["warning"].as_str() {
                                self.push(w.to_string(), Tone::Error);
                            }
                            self.push(
                                if self.plan_mode {
                                    "PLAN 模式开启 · 只读研究，写操作将被拒绝 · /plan 恢复"
                                } else {
                                    "PLAN 模式关闭 · 可以执行修改了"
                                },
                                if self.plan_mode {
                                    Tone::Badge
                                } else {
                                    Tone::Muted
                                },
                            );
                        }
                        (Pending::Effort, true) => {
                            self.effort = d["effort"].as_str().unwrap_or("off").into();
                            self.remember_effort();
                            if d.get("efforts").is_some() {
                                self.efforts = Self::string_array(&d["efforts"]);
                            }
                            let choices = self.efforts.join(" · ");
                            // bare /effort asks `status`, which also carries the model: show what there is to pick
                            if d.get("model").is_some() {
                                self.push(
                                    if self.efforts.is_empty() {
                                        format!("推理力度：{} · 这个模型没有可选档位", self.effort)
                                    } else {
                                        format!("推理力度：{} · 可选 {choices} · off", self.effort)
                                    },
                                    Tone::Muted,
                                );
                            } else {
                                self.push(format!("推理力度 → {}", self.effort), Tone::Muted);
                            }
                        }
                        (Pending::Memory, true) => {
                            let text = d["text"].as_str().unwrap_or("");
                            let path = d["path"].as_str().unwrap_or("AGENTS.md");
                            let body = if text.trim().is_empty() {
                                format!(
                                    "（空）\n\n用 /remember <一句话> 添加，或 ctrl+e 直接编辑 {path}"
                                )
                            } else {
                                text.to_string()
                            };
                            let mut viewer = Viewer::new(format!("长期记忆 · {path}"), body);
                            viewer.prompt_state = None;
                            self.viewer = Some(viewer);
                        }
                        (Pending::Remember, true) => {
                            let line = d["remembered"].as_str().unwrap_or("");
                            self.push(format!("已记住：{line}（全部会话生效）"), Tone::Success);
                        }
                        (Pending::Skills, true) => {
                            let items: Vec<PickItem> = d["skills"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|sk| {
                                            Some(PickItem {
                                                group: String::new(),
                                                label: format!(
                                                    "{} [{}]",
                                                    sk["name"].as_str()?,
                                                    sk["scope"].as_str().unwrap_or("?")
                                                ),
                                                note: sk["description"]
                                                    .as_str()
                                                    .unwrap_or("")
                                                    .to_string(),
                                                warn: false,
                                                current: false,
                                                id: sk["path"].as_str()?.to_string(),
                                            })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            if items.is_empty() {
                                self.push("暂无技能 · /save-as-skill 从会话蒸馏", Tone::Muted);
                            } else {
                                self.picker = Some(Picker::new(PickerKind::Skills, items));
                            }
                        }
                        (Pending::Goal, true) => match d["goal"].as_str() {
                            Some(g) if !g.is_empty() => {
                                self.goal_text = Some(g.to_string());
                                let max = d["maxRounds"].as_u64().unwrap_or(5);
                                self.push(
                                    format!("● goal：{g} · 自动继续上限 {max} 轮 · /goal off 停止"),
                                    Tone::Success,
                                );
                            }
                            _ => {
                                self.goal_text = None;
                                self.push("goal 已清除", Tone::Muted);
                            }
                        },
                        (Pending::Loop, true) => {
                            if let Some(p) = d["loop"].as_str() {
                                let every = d["everySec"].as_u64().unwrap_or(0);
                                let nice = if every % 3600 == 0 {
                                    format!("{}h", every / 3600)
                                } else if every % 60 == 0 {
                                    format!("{}m", every / 60)
                                } else {
                                    format!("{every}s")
                                };
                                self.push(
                                    format!("● loop 每 {nice}：{p} · /loop off 停"),
                                    Tone::Success,
                                );
                            } else {
                                self.push("loop 已停止", Tone::Muted);
                            }
                        }
                        (Pending::Worktree, true) => {
                            if let Some(list) = d["worktrees"].as_array() {
                                let items: Vec<PickItem> = list
                                    .iter()
                                    .filter_map(|w| {
                                        Some(PickItem {
                                            group: String::new(),
                                            label: format!(
                                                "{} · {} · 脏文件 {}",
                                                w["name"].as_str()?,
                                                w["branch"].as_str().unwrap_or("?"),
                                                w["dirty"].as_i64().unwrap_or(-1)
                                            ),
                                            note: w["path"].as_str().unwrap_or("").to_string(),
                                            warn: false,
                                            current: false,
                                            id: w["name"].as_str()?.to_string(),
                                        })
                                    })
                                    .collect();
                                if items.is_empty() {
                                    self.push(
                                        "暂无 worktree · /worktree new <name> 创建",
                                        Tone::Muted,
                                    );
                                } else {
                                    self.picker = Some(Picker::new(PickerKind::Worktree, items));
                                }
                            } else if d.get("merged").is_some() {
                                let ok = d["merged"].as_bool().unwrap_or(false);
                                self.push(
                                    if ok {
                                        "已合并回主工作区".to_string()
                                    } else {
                                        format!(
                                            "合并输出：{}",
                                            d["output"]
                                                .as_str()
                                                .unwrap_or("")
                                                .chars()
                                                .take(300)
                                                .collect::<String>()
                                        )
                                    },
                                    if ok { Tone::Success } else { Tone::Warning },
                                );
                            } else if d.get("removed").is_some() {
                                self.push("worktree 已删除", Tone::Muted);
                            } else if let Some(p) = d["path"].as_str() {
                                self.push(format!("worktree 已创建 → {p}"), Tone::Success);
                            }
                        }
                        (Pending::Bg, true) => {
                            let t = &d["task"];
                            self.push(
                                format!(
                                    "● 后台任务 {} 已启动（worktree 隔离）· /tasks 查看",
                                    t["id"].as_str().unwrap_or("?")
                                ),
                                Tone::Success,
                            );
                        }
                        (Pending::Tasks, true) => {
                            let mut text = String::from("后台任务：\n\n");
                            if let Some(list) = d["tasks"].as_array() {
                                for t in list {
                                    let status = t["status"].as_str().unwrap_or("?");
                                    let mark = match status {
                                        "running" => "▶",
                                        "done" => "✓",
                                        "failed" => "✗",
                                        _ => "■",
                                    };
                                    let prompt: String = t["prompt"]
                                        .as_str()
                                        .unwrap_or("")
                                        .chars()
                                        .take(40)
                                        .collect();
                                    let sess = short_session(t["sessionId"].as_str().unwrap_or(""));
                                    text.push_str(&format!(
                                        "  {mark} {} [{}] {} · {}\n      /resume {} 可查看 · /stop {}\n",
                                        t["id"].as_str().unwrap_or("?"),
                                        status,
                                        prompt,
                                        t["model"].as_str().unwrap_or("?"),
                                        sess,
                                        t["id"].as_str().unwrap_or("?"),
                                    ));
                                }
                            }
                            if text.trim() == "后台任务：" {
                                text.push_str("  （空）· /bg <描述> 启动一个");
                            }
                            self.viewer = Some(Viewer::read_only("后台任务", text));
                        }
                        (Pending::StopTask, true) => {
                            self.push("后台任务已停止", Tone::Muted);
                        }
                        (Pending::AgentsShow, true) => {
                            let mut text = String::from("角色模型路由：\n\n");
                            let driver = d["driver"].as_str().unwrap_or("?");
                            let get = |k: &str| d["roles"][k].as_str().unwrap_or(driver);
                            for (role, note) in [
                                ("driver", "主会话（立即生效）"),
                                ("subagent", "/subagent、/bg 与 task 工具默认"),
                                ("longContext", "/bg --long 长上下文"),
                                ("plan", "plan 模式期间切换到它，退出后换回"),
                            ] {
                                text.push_str(&format!("  {role:<12} {} · {note}\n", get(role)));
                            }
                            text.push_str("\n/agents <role> <model> 设置\n\n子 agent（模型用 task 工具派活）：\n\n");
                            for a in d["subAgents"].as_array().into_iter().flatten() {
                                let model = a["model"].as_str().filter(|m| !m.is_empty());
                                text.push_str(&format!(
                                    "  {:<12} {}{} · {}\n",
                                    a["name"].as_str().unwrap_or("?"),
                                    if a["readonly"].as_bool() == Some(true) { "只读" } else { "可写" },
                                    model.map(|m| format!(" · {m}")).unwrap_or_default(),
                                    a["description"].as_str().unwrap_or("")
                                ));
                            }
                            text.push_str("\nconfig.json 的 subAgents 可自定义");
                            self.viewer = Some(Viewer::read_only("agents", text));
                        }
                        (Pending::AgentsSet, true) => {
                            self.push("角色模型已保存", Tone::Success);
                        }
                        (Pending::Subtask, true) => {
                            let t = &d["task"];
                            self.push(
                                format!(
                                    "⑂ 子代理 {} 已派出（独立 worktree）· 摘要将写回本会话",
                                    t["id"].as_str().unwrap_or("?")
                                ),
                                Tone::Subagent,
                            );
                        }
                        (Pending::McpList, true) => {
                            self.render_mcp_list(d);
                        }
                        (Pending::McpAdd, true) => {
                            let auth = d["auth"].as_str().unwrap_or("none");
                            let id = d["added"].as_str().unwrap_or("?");
                            if auth != "none" {
                                self.push(format!("已添加 MCP {id} · 还需要：{auth}"), Tone::Warning);
                            } else {
                                self.push(format!("已添加 MCP {id}"), Tone::Success);
                            }
                            self.push("/restart 后连接并加载工具".to_string(), Tone::Muted);
                        }
                        (Pending::PluginsList, true) => {
                            let mut text = String::from("本地插件：\n\n");
                            if let Some(list) = d["plugins"].as_array() {
                                for p in list {
                                    let enabled = p["enabled"].as_bool().unwrap_or(false);
                                    text.push_str(&format!(
                                        "  {} · {} · {}\n",
                                        p["name"].as_str().unwrap_or("?"),
                                        p["description"].as_str().unwrap_or(""),
                                        if enabled {
                                            "启用"
                                        } else {
                                            "停用（plugin.json enabled:false）"
                                        }
                                    ));
                                }
                            }
                            if text.trim() == "本地插件：" {
                                text.push_str("  （无）· 放 ~/.tango/plugins/<名>/plugin.json");
                            }
                            self.viewer = Some(Viewer::read_only("plugins", text));
                        }
                        (Pending::History, true) => self.load_history(d),
                        (Pending::Theme, true) => {
                            self.theme = Theme::resolve(
                                d["theme"].as_str(),
                                std::env::var("COLORFGBG").ok().as_deref(),
                                crate::termbg::detected(),
                            );
                            self.push(
                                format!("theme → {}", d["theme"].as_str().unwrap_or("auto")),
                                Tone::Muted,
                            );
                        }
                        (Pending::Mouse, true) => {
                            self.mouse = d["mouse"].as_bool().unwrap_or(true);
                            self.push(
                                format!("mouse → {}", if self.mouse { "on" } else { "off" }),
                                Tone::Muted,
                            );
                        }
                        (Pending::NvimViewer, true) => {
                            self.nvim_preference = d["nvimViewer"].as_str().unwrap_or("ask").into();
                            self.push("Neovim 只读偏好已保存", Tone::Muted);
                        }
                        (Pending::Handoff, false)
                        | (Pending::Takeover, false)
                        | (Pending::HandoffList, false)
                        | (Pending::HandoffCancel, false) => {
                            self.push(
                                format!("{}", v["error"].as_str().unwrap_or("?")),
                                Tone::Error,
                            );
                        }
                        (Pending::Recap, false) => {
                            self.btw_id = None;
                            self.btw_title = None;
                            self.busy = false;
                            self.push(
                                format!("recap error: {}", v["error"].as_str().unwrap_or("?")),
                                Tone::Error,
                            );
                        }
                        (Pending::Btw, false) => {
                            self.btw_id = None;
                            let title = self.btw_title.take();
                            self.busy = false;
                            if title.is_some()
                                && self
                                    .viewer
                                    .as_ref()
                                    .is_some_and(|v| Some(&v.title) == title.as_ref())
                            {
                                self.viewer = None; // close the empty side-question dialog
                            }
                            self.push(
                                format!("btw error: {}", v["error"].as_str().unwrap_or("?")),
                                Tone::Error,
                            );
                        }
                        (Pending::ResumeResolve, false) => self.push(
                            format!(
                                "resume: {} · 当前会话不变",
                                v["error"].as_str().unwrap_or("?")
                            ),
                            Tone::Error,
                        ),
                        (pending, false) => {
                            self.push(
                                format!("error: {}", v["error"].as_str().unwrap_or("?")),
                                Tone::Error,
                            );
                            // Shift+Tab moved the badge before serve answered: put it back
                            if matches!(pending, Pending::Mode | Pending::Plan) {
                                let id = serve.command("status", vec![]);
                                self.pending.push((id, Pending::Status));
                            }
                        }
                    }
                }
            }
            // a model request failed before any output; serve retries it with backoff
            "retry" => {
                let text = v["text"].as_str().map(String::from).unwrap_or_else(|| {
                    format!(
                        "⟳ retry {}/{}: {}",
                        v["attempt"].as_u64().unwrap_or(0),
                        v["max"].as_u64().unwrap_or(0),
                        v["reason"].as_str().unwrap_or("?")
                    )
                });
                self.push(text, Tone::Muted);
            }
            "error" => self.push(
                format!("error: {}", v["message"].as_str().unwrap_or("?")),
                Tone::Error,
            ),
            // a language server's view of a file tango edited (lsp-diagnostics.ts): the
            // newest list replaces the file's old one; an empty list drops the file
            "diagnostics" => {
                let file = v["file"].as_str().unwrap_or("").trim();
                if !file.is_empty() {
                    let mut items: Vec<Problem> = v["problems"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Problem::from_json).collect())
                        .unwrap_or_default();
                    items.sort_by_key(|p| (p.severity, p.line, p.col));
                    self.problems.retain(|f| f.file != file);
                    if !items.is_empty() {
                        self.problems.push(FileProblems {
                            file: file.to_string(),
                            items,
                        });
                        self.problems.sort_by(|a, b| a.file.cmp(&b.file));
                    }
                    self.problems_sel = self
                        .problems_sel
                        .min(self.problem_count().saturating_sub(1));
                }
            }
            "cwd_missing" => {
                let cwd = v["cwd"].as_str().unwrap_or("");
                let msg = format!(
                    "error: 工作目录不存在：{cwd}。请选择用过的目录，或输入 /路径（也可 /cd <目录>）。"
                );
                if self.lines.last().map(|l| l.text.as_str()) != Some(msg.as_str()) {
                    self.push(msg, Tone::Error);
                }
                let dirs = v["dirs"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.open_cwd_picker(&dirs);
            }
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
        // replayed ?choice / ?suggest blocks re-arm the panel and todo like live ones;
        // any later user message answered them, so only those after the last one survive
        let todos_before = self.todos.len();
        for it in items {
            let text = it["text"].as_str().unwrap_or("");
            match it["kind"].as_str() {
                Some("user") => {
                    self.choices = None;
                    self.todos.truncate(todos_before);
                    self.push_user(text);
                    // backfill the ↑/↓ input-box history so recall works right
                    // after resume too (same dedup rule as a live send)
                    if self.prompt_history.last().map(String::as_str) != Some(text) {
                        self.prompt_history.push(text.to_string());
                    }
                    if let Some(line) = self.lines.last_mut() {
                        line.prompt_state = Some(PromptState::Completed);
                    }
                }
                // a /subagent write-back keeps its own color when the session is resumed
                Some("assistant") if text.starts_with("⑂ subagent ") => {
                    self.push(text, Tone::Subagent);
                }
                Some("assistant") => {
                    self.stream = text.to_string();
                    self.commit_stream();
                }
                Some("mail") => {
                    let short: String =
                        short_session(it["from"].as_str().unwrap_or("?")).to_string();
                    self.push(format!("✉ 来自会话 {short} 的消息："), Tone::Muted);
                    self.stream = it["text"].as_str().unwrap_or("").to_string();
                    self.commit_stream();
                }
                Some("handoff") => {
                    let short: String =
                        short_session(it["from"].as_str().unwrap_or("?")).to_string();
                    let head = if it["via"] == "takeover" {
                        format!("⇠ 已接管会话 {short} · 摘要：")
                    } else {
                        format!("⇠ 收到会话 {short} 的交接摘要：")
                    };
                    self.push(head, Tone::Muted);
                    self.stream = text.to_string();
                    self.commit_stream();
                }
                Some("tool") => {
                    let name = it["name"].as_str().unwrap_or("?");
                    let ok = !it["isError"].as_bool().unwrap_or(false);
                    let style = if ok { Tone::Muted } else { Tone::Warning };
                    let out = it["output"]
                        .as_str()
                        .map(|s| s.lines().take(200).collect::<Vec<_>>().join("\n"));
                    let (brief, call) = Self::call_brief(&it["input"]);
                    self.push_tool(format!("⚙ {name} {brief}"), style, out);
                    if let Some(line) = self.lines.last_mut() {
                        line.call = Some(call);
                        line.frame = Some((format!("⚙ {name} "), String::new()));
                    }
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

    /// Input box rows: the work rule, the tinted block (text rows, then the
    /// mode · model info row). No blank padding rows: the draft sits right under the rule.
    fn input_height(&self, width: u16) -> u16 {
        let text = self.input_layout(Self::input_text_w(width)).lines.len().min(8) as u16;
        text + 2
    }

    /// Width the draft wraps at: the block's left bar + gap take INPUT_X columns.
    fn input_text_w(width: u16) -> u16 {
        width.saturating_sub(INPUT_X).max(1)
    }

    /// First screen row of the draft text (right under the rule).
    fn input_text_top(&self) -> u16 {
        self.input_area.y + 1
    }

    /// Draft rows visible in the block.
    fn input_visible(&self) -> usize {
        self.input_area.height.saturating_sub(2).max(1) as usize
    }

    fn chat_height(&self, area: Rect, input_h: u16) -> usize {
        (area.height.saturating_sub(1 + input_h)) as usize
    }

    /// Height of the choices panel as a layout row: 0 when hidden. The question
    /// wraps at the dialog's inner width, so long asks stay fully readable.
    fn choices_panel_height(&self, width: u16, term_h: u16) -> u16 {
        let Some(c) = &self.choices else { return 0 };
        if self.choices_collapsed {
            return 0;
        }
        let w = 72u16.min(width).max(3);
        let iw = (w as usize).saturating_sub(2); // bordered dialog inner width
        let q_rows = wrap_chat_rows(&[c.question.clone()], iw).len() as u16;
        let notes = c.options.iter().filter(|o| o.note.is_some()).count() as u16;
        let content = q_rows + c.preview.len() as u16 + c.options.len() as u16 + notes + 1; // + explain row
        // borders + hint rows; capped so the chat keeps at least half the screen
        (content + 2 + HINT_ROWS).min(term_h / 2).max(3)
    }

    /// True once the conversation has content: anything besides the session banner
    /// and serve notices. The empty-session splash ends here and the wordmark stays.
    fn conversation_started(&self) -> bool {
        self.lines.iter().any(|l| !l.notice && !l.text.starts_with("tango serve"))
    }

    /// The plain wordmark rows (newline-joined) that lead the chat once the
    /// conversation has started, when the terminal is wide and tall enough for it.
    /// The wordmark is a display-only line: never in `self.lines`, so it is not
    /// saved, exported or replayed when the session is reopened.
    fn kept_wordmark(&self) -> Option<String> {
        if !self.conversation_started()
            || (self.chat_inner_w as usize) < App::WORDMARK_W + 4
            || !self.chat_roomy
        {
            return None;
        }
        Some(self.wordmark_lines(self.chat_inner_w as usize).1.join("\n"))
    }

    /// the chat exactly as drawn — scroll math and mouse hit testing use the same lines
    fn chat_lines(&self) -> Vec<ChatLine> {
        let wordmark = self.kept_wordmark();
        // the expand hint is spelled out on the first collapsed long thinking block only
        let first_folded = self.lines.iter().position(|l| {
            l.tone == Tone::Thinking
                && !l.expanded
                && (l.text.lines().count() > THINK_PREVIEW_LINES
                    || l.text.chars().count() > THINK_PREVIEW_CHARS)
        });
        let mut lines: Vec<ChatLine> = self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                !self.focus
                    || (line.tone != Tone::Thinking
                        && !(line.tone == Tone::Muted && line.text.starts_with("⚙ ")))
            })
            .map(|(source_index, line)| {
                let mut text = if line.tone == Tone::Thinking {
                    thinking_view(&line.text, line.expanded, first_folded == Some(source_index))
                } else if let (Some(call), Some((head, tail))) = (&line.call, &line.frame) {
                    // one row, whatever the width: the call gets what head, tail and
                    // the optional [HH:MM] leave (1 spare column for … under /ascii)
                    let width = if self.chat_inner_w > 0 {
                        self.chat_inner_w as usize
                    } else {
                        BRIEF_CHARS + display_width(head) + display_width(tail)
                    };
                    let ts = if self.timestamps { 8 } else { 0 };
                    let avail = width
                        .saturating_sub(display_width(head) + display_width(tail) + ts + 1);
                    format!("{head}{}{tail}", fit_call(call, avail))
                } else {
                    line.text.clone()
                };
                // density 2 previews every call's output inline; 0 and 1 (default) keep each
                // call to its one line, failures too: their output is a double-click away
                let show_output = self.density >= 2;
                if show_output && let Some(output) = &line.output {
                    let preview = output
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .take(if self.density >= 2 { 30 } else { 8 })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !preview.is_empty() {
                        text.push('\n');
                        text.push_str(
                            &preview
                                .lines()
                                .map(|line| format!("  {line}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                    }
                }
                if self.ascii {
                    if line.tone == Tone::Thinking
                        && let Some(rest) = text.strip_prefix(THINK_ICON)
                    {
                        text = format!("{THINK_ICON_ASCII}{rest}");
                    }
                    text = asciify(&text);
                }
                if self.timestamps
                    && let Some(ts) = &line.ts
                    && !text.starts_with('[')
                {
                    text = format!("{ts} {text}");
                }
                ChatLine {
                    text,
                    tone: line.tone,
                    ts: line.ts.clone(),
                    output: line.output.clone(),
                    prompt_state: line.prompt_state,
                    tool_id: line.tool_id.clone(),
                    source_index,
                    notice: line.notice,
                    expanded: line.expanded,
                    call: line.call.clone(),
                    frame: line.frame.clone(),
                    wordmark: false,
                }
            })
            .collect();
        // the wordmark leads the chat
        if let Some(text) = wordmark {
            lines.insert(
                0,
                ChatLine {
                    text,
                    tone: Tone::Text,
                    ts: None,
                    output: None,
                    prompt_state: None,
                    tool_id: None,
                    source_index: usize::MAX,
                    notice: false,
                    expanded: false,
                    call: None,
                    frame: None,
                    wordmark: true,
                },
            );
        }
        lines
    }

    /// The newest user prompt whose first row is above the view (global row `offset`),
    /// and the scroll_back that puts that row just under the header row.
    fn prompt_above(
        &self,
        offset: usize,
        tones: &[Tone],
        chat_h: usize,
    ) -> Option<(String, usize)> {
        let mut found: Option<(usize, usize)> = None; // (line, first row)
        let mut prev_line = usize::MAX;
        for (g, row) in self.chat_rows.iter().enumerate().take(offset) {
            if row.line != prev_line {
                prev_line = row.line;
                if tones.get(row.line) == Some(&Tone::User) {
                    found = Some((row.line, g));
                }
            }
        }
        let (line, first) = found?;
        let text = self
            .chat_lines()
            .get(line)?
            .text
            .lines()
            .next()?
            .to_string();
        let total = self.chat_rows.len();
        // the header covers the top row, so land the prompt's first row on row 1
        let back = total
            .saturating_sub(chat_h + first.saturating_sub(1))
            .max(1);
        Some((text, back))
    }

    /// alt+↑/alt+↓ hop between user prompts. Backward reuses prompt_above —
    /// the prompt's first row lands right under the header row; forward lands
    /// the next prompt starting below the view the same way, and past the
    /// newest one it returns to the bottom (and clears the unread counter).
    fn jump_prompt(&mut self, next: bool) {
        let chat_h = self.chat_area.height as usize;
        if chat_h == 0 || self.chat_rows.is_empty() {
            return;
        }
        let total = self.chat_rows.len();
        let offset = total.saturating_sub(chat_h + self.scroll_back);
        if !next {
            if let Some((_, back)) = self.prompt_above(offset, &self.tone_rows(), chat_h) {
                self.scroll_back = back;
            }
            return;
        }
        let tones = self.tone_rows();
        let mut prev_line = usize::MAX;
        let target = self.chat_rows.iter().enumerate().find_map(|(g, row)| {
            if row.line != prev_line {
                prev_line = row.line;
                if tones.get(row.line) == Some(&Tone::User) && g >= offset + chat_h {
                    return Some(g);
                }
            }
            None
        });
        match target {
            Some(first) => {
                self.scroll_back = total
                    .saturating_sub(chat_h + first.saturating_sub(1))
                    .max(1);
            }
            None => self.jump_to_bottom(),
        }
    }

    /// per chat-line tones for row-walking helpers
    fn tone_rows(&self) -> Vec<Tone> {
        self.chat_lines().iter().map(|l| l.tone).collect()
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        self.hint_buttons.clear();
        self.modal_rect = None;
        // short terminals give the rows to the conversation: no wordmark in the chat
        self.chat_roomy = area.height >= 20;
        let input_h = self.input_height(area.width);
        // the choices panel takes a real layout row — the chat scrolls up to make
        // room instead of being covered (the text under it stays reachable)
        let panel_h = self.choices_panel_height(area.width, area.height);
        // jump-to-bottom pill: its own row above the input, never over the chat or the
        // status bar; the chat shrinks by one so nothing gets covered
        let jump_h = u16::from(self.scroll_back > 0);
        // `~/dir:branch` + key hints under the block, when the terminal can spare a row
        let footer_h = u16::from(area.height >= 24);
        let chat_h = self
            .chat_height(area, input_h)
            .saturating_sub(panel_h as usize + jump_h as usize + footer_h as usize);
        let [chat_a, jump_a, panel_a, input_a, footer_a, status_a] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(jump_h),
            Constraint::Length(panel_h),
            Constraint::Length(input_h),
            Constraint::Length(footer_h),
            Constraint::Length(1),
        ])
        .areas(area);
        self.footer_mode_area = Rect::default();
        self.footer_palette_area = Rect::default();
        if footer_h > 0 {
            let line = self.footer_line(footer_a.width as usize);
            // each hint is a "  key" span + a " desc" span: the clickable rect covers both
            let mut x = 0u16;
            let mut spans = line.spans.iter().peekable();
            while let Some(sp) = spans.next() {
                let mut w = display_width(sp.content.as_ref()) as u16;
                let start = x;
                let hit = match sp.content.trim() {
                    "shift+tab" => Some(true),
                    "ctrl+p" => Some(false),
                    _ => None,
                };
                if let Some(is_mode) = hit {
                    if let Some(desc) = spans.next() {
                        w += display_width(desc.content.as_ref()) as u16;
                    }
                    // skip the two-space gap so the rect starts at the key text
                    let rect = Rect { x: footer_a.x + start + 2, y: footer_a.y, width: w - 2, height: 1 };
                    if is_mode {
                        self.footer_mode_area = rect;
                    } else {
                        self.footer_palette_area = rect;
                    }
                }
                x = x.saturating_add(w);
            }
            f.render_widget(Paragraph::new(line), footer_a);
        }

        self.chat_area = chat_a;
        // set before the rows are built: chat_lines() sizes the kept wordmark with it
        self.chat_inner_w = chat_a.width;
        self.input_area = input_a;

        // status bar
        let spans = self.status_spans(status_a.width);
        // the " sess … " span is clickable (copies the full id): find it and its offset
        self.session_id_area = Rect::default();
        if !self.session_full.is_empty() {
            let mut x = 0u16;
            for sp in &spans {
                let w = display_width(sp.content.as_ref()) as u16;
                if sp.content.starts_with(" sess ") {
                    self.session_id_area = Rect {
                        x: status_a.x + x,
                        y: status_a.y,
                        width: w,
                        height: 1,
                    };
                    break;
                }
                x = x.saturating_add(w);
            }
        }
        self.problems_area = Rect::default();
        self.history_area = Rect::default();
        {
            let mut x = 0u16;
            for sp in &spans {
                let w = display_width(sp.content.as_ref()) as u16;
                let rect = Rect {
                    x: status_a.x + x,
                    y: status_a.y,
                    width: w,
                    height: 1,
                };
                // the history emoji is the first span, whatever the bar's template
                if x == 0 {
                    self.history_area = rect;
                }
                if matches!(
                    sp.content.trim_start().chars().next(),
                    Some('✗' | '⚠' | '!' | 'w')
                ) {
                    self.problems_area = rect;
                }
                x = x.saturating_add(w);
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), status_a);

        if !self.busy {
            // an interrupted/abandoned turn must not leave a gear spinning forever
            self.running_tools.clear();
        }
        self.think_frame = if cfg!(test) {
            0 // deterministic colors; the wave math is unit-tested separately
        } else {
            std::time::Instant::now()
                .duration_since(*THINK_EPOCH)
                .as_millis()
                / THINK_FRAME_MS
        };
        // chat with bottom-anchored scrolling (scroll_back=0 follows the newest line).
        // We own the wrap (wrap_chat_rows) so every terminal row maps back to an exact
        // (source line, char range) — character-exact mouse selection and click targets.
        let inner_w = chat_a.width as usize;
        let chat_lines = self.chat_lines();
        let mut texts: Vec<String> = chat_lines.iter().map(|l| l.text.clone()).collect();
        if !self.stream.is_empty() {
            // live assistant text (not committed yet) hangs off the last line, markdown stripped
            texts.push(self.stream.replace("**", "").replace('`', ""));
        }
        let tones: Vec<Tone> = chat_lines.iter().map(|l| l.tone).collect();
        let user_flags: Vec<bool> = tones.iter().map(|t| *t == Tone::User).collect();
        let think_flags: Vec<bool> = tones.iter().map(|t| *t == Tone::Thinking).collect();
        let wordmark_flags: Vec<bool> = chat_lines.iter().map(|l| l.wordmark).collect();
        let gear_running: Vec<bool> = chat_lines
            .iter()
            .map(|l| l.tool_id.as_deref().is_some_and(|id| self.running_tools.contains(id)))
            .collect();
        let mut all_rows = wrap_chat_rows_mixed(
            &texts,
            inner_w,
            &user_flags,
            &think_flags,
            &wordmark_flags,
            self.user_bubble,
        );
        layout_user_bubbles(&mut all_rows, &user_flags, inner_w, self.user_bubble);
        // The chat is bottom-anchored: a short conversation sits at the bottom of the
        // area. Pad under the wordmark instead, so it stays at the top with the first
        // message right under it; the pad shrinks as the conversation grows and is gone
        // once the chat scrolls. Pad rows reuse the wordmark flag (decoration: blank,
        // not selected, not clicked).
        if wordmark_flags.first().copied().unwrap_or(false) && self.scroll_back == 0 {
            let wm_rows = all_rows.iter().filter(|r| r.wordmark).count();
            let pad = chat_h.saturating_sub(all_rows.len());
            for i in 0..pad {
                let mut r = all_rows[wm_rows - 1].clone();
                r.cs = 0;
                r.ce = 0;
                r.text = String::new();
                all_rows.insert(wm_rows + i, r);
            }
        }
        let total = all_rows.len();
        if self.last_visual_total > 0 && total > self.last_visual_total && self.scroll_back > 0 {
            let added = total - self.last_visual_total;
            self.scroll_back += added;
            self.unread_rows += added;
        }
        if self.scroll_back == 0 {
            self.unread_rows = 0;
        }
        self.last_visual_total = total;
        let offset = total.saturating_sub(chat_h + self.scroll_back);
        self.chat_rows = all_rows;
        self.chat_scroll_offset = offset;

        // the LAST thinking block, but only if it belongs to the running turn
        // (its rows sit after the newest user prompt): older blocks stay dim.
        let live_think_block: Option<usize> = {
            let last_user = user_flags
                .iter()
                .rposition(|u| *u)
                .unwrap_or(usize::MAX);
            let mut block: Option<usize> = None;
            let mut open = false;
            for (i, r) in self.chat_rows.iter().enumerate() {
                if r.think {
                    if !open {
                        open = true;
                        block = Some(i);
                    }
                } else {
                    open = false;
                }
            }
            block.filter(|&start| {
                self.chat_rows.get(start).is_some_and(|r| r.line >= last_user)
            })
        };
        // render the visible window row by row with char-exact selection highlight
        let sel = self.selection_range(chat_a.y);
        // the wordmark kept in the chat: same gradient rows the splash draws
        let wordmark: Vec<Line> = if chat_lines.first().is_some_and(|l| l.wordmark) {
            self.wordmark_lines(inner_w).0
        } else {
            Vec::new()
        };
        let mut shown: Vec<Line> = Vec::new();
        if let Some(splash) = self.splash_lines(chat_a.width as usize, chat_h) {
            shown = splash;
        } else {
        for g in offset..total.min(offset + chat_h as usize) {
            let Some(row) = self.chat_rows.get(g) else {
                continue;
            };
            // a wordmark row draws its gradient line (the nth of the 5 rows of the
            // wordmark line); it is decoration: not selected, not clicked
            if row.wordmark {
                // pad rows (blank text) fill the space under the wordmark
                if row.text.is_empty() {
                    shown.push(Line::from(""));
                    continue;
                }
                let n = chat_lines
                    .get(row.line)
                    .map_or(0, |l| l.text.chars().take(row.cs).filter(|&c| c == '\n').count());
                if let Some(line) = wordmark.get(n) {
                    shown.push(line.clone());
                } else {
                    shown.push(Line::from(row.text.clone()));
                }
                continue;
            }
            let tone = tones.get(row.line).copied().unwrap_or(Tone::Text);
            let style = self.theme.style(tone);
            let chars: Vec<char> = row.text.chars().collect();
            let (s_from, s_to) = match sel {
                Some((ra, ca, rb, cb)) if ra <= g && g <= rb => {
                    let from = if g == ra { ca.min(chars.len()) } else { 0 };
                    let to = if g == rb {
                        cb.min(chars.len())
                    } else {
                        chars.len()
                    };
                    (from, to.max(from))
                }
                _ => (0, 0),
            };
            // rounded bubble frame rows: ╭─╮ / ╰─╯ in the bubble's own style
            if row.border {
                // same width as the text rows: x + interior + │ + space each side
                let block = (inner_w as usize).saturating_sub(row.x as usize + BUBBLE_MARGIN);
                // a top border is always followed by this frame's text row; a
                // bottom one is not (the previous bubble's bottom border is also
                // user-flagged, so looking backwards misclassifies adjacent bubbles)
                let is_top = self
                    .chat_rows
                    .get(g + 1)
                    .is_some_and(|n| n.user && !n.border);
                let edge = if is_top {
                    ("╭", "╮")
                } else {
                    ("╰", "╯")
                };
                let frame = format!(
                    "{}{}{}",
                    edge.0,
                    "─".repeat(block.saturating_sub(2)),
                    edge.1
                );
                shown.push(Line::from(vec![
                    Span::raw(" ".repeat(row.x as usize)),
                    Span::styled(frame, self.theme.user_bubble(style)),
                ]));
                continue;
            }
            let mut pad = if row.user {
                vec![Span::raw(" ".repeat(row.x as usize))]
            } else {
                Vec::new()
            };
            if row.think {
                // "▍ " gutter (left quarter block: fills the cell's full height, so
                // consecutive rows join without the per-row seam a box glyph like ┃
                // shows); the color scrolls down the live block while a turn runs
                let local = live_think_block.map_or(g, |b| g.saturating_sub(b));
                let live = self.busy
                    && self.animations
                    && live_think_block.is_some_and(|b| g >= b);
                let color = thinking_bar_color(self.think_frame, local, live);
                pad.push(Span::styled("▍".to_string(), Style::default().fg(color)));
                pad.push(Span::raw(" "));
            }
            let base = if row.user {
                self.theme.user_bubble(style)
            } else {
                style
            };
            if sel.is_some() && s_to > s_from {
                let mut spans = pad;
                spans.push(Span::styled(
                    chars[..s_from].iter().collect::<String>(),
                    base,
                ));
                spans.push(Span::styled(
                    chars[s_from..s_to].iter().collect::<String>(),
                    base.reversed(),
                ));
                spans.push(Span::styled(chars[s_to..].iter().collect::<String>(), base));
                shown.push(Line::from(spans));
            } else if row.user && !self.user_bubble {
                // Claude Code mode: same theme, left-aligned, full-width bg
                let text = format!(" {} ", row.text);
                let fill = (inner_w as usize).saturating_sub(display_width(&text));
                let line = format!("{}{}", text, " ".repeat(fill));
                shown.push(Line::from(Span::styled(line, base)));
            } else if row.user {
                // │ text │ — the interior padded so the right bar closes the block.
                // The frame is x + max_row_width + 4 wide (layout_user_bubbles), so the
                // interior holds exactly that width: -4, not -5, or a row that fills the
                // bubble pushes its │ one column past the ╮ above it.
                let maxw = (inner_w as usize).saturating_sub(row.x as usize + 4 + BUBBLE_MARGIN);
                // on a terminal too narrow for the frame, clip rather than push the │ past it
                let text = clip_to_width(&row.text, maxw);
                let fill = maxw.saturating_sub(display_width(&text));
                let mut spans = pad;
                spans.push(Span::styled("│ ".to_string(), base));
                spans.push(Span::styled(text, base));
                spans.push(Span::styled(format!("{} │", " ".repeat(fill)), base));
                shown.push(Line::from(spans));
            } else if row.think {
                let mut spans = pad;
                // the 🤖 (or ASCII *) heading a block stays upright; only the reasoning is italic
                let icon = [THINK_ICON, THINK_ICON_ASCII]
                    .into_iter()
                    .find(|i| row.text.starts_with(i));
                match icon {
                    Some(icon) => {
                        spans.push(Span::styled(
                            icon.to_string(),
                            style.remove_modifier(Modifier::ITALIC),
                        ));
                        spans.push(Span::styled(row.text[icon.len()..].to_string(), style));
                    }
                    None => spans.push(Span::styled(row.text.clone(), style)),
                }
                shown.push(Line::from(spans));
            } else if gear_running.get(row.line).copied().unwrap_or(false)
                && row.text.starts_with("⚙")
            {
                // the tool is still running: the gear itself cycles color
                let rest = row.text.strip_prefix("⚙ ").unwrap_or(&row.text).to_string();
                let mut spans = pad;
                spans.push(Span::styled(
                    "⚙ ".to_string(),
                    Style::default().fg(gear_color(
                        if self.animations { self.think_frame } else { 0 },
                        true,
                    )),
                ));
                spans.push(Span::styled(rest, style));
                shown.push(Line::from(spans));
            } else {
                shown.push(Line::from(Span::styled(row.text.clone(), style)));
            }
        }
        }
        f.render_widget(Paragraph::new(Text::from(shown)), chat_a);
        self.prompt_header = None;
        if self.scroll_back > 0
            && chat_a.height > 1
            && let Some((text, back)) = self.prompt_above(offset, &tones, chat_h)
        {
            let area = Rect {
                height: 1,
                ..chat_a
            };
            let label = clip_to_width(&format!("{text}  · 点击跳到这条提示"), area.width as usize);
            f.render_widget(
                Paragraph::new(label).style(self.theme.style(Tone::Muted).reversed()),
                area,
            );
            self.prompt_header = Some((area, back));
        }
        // toast: top-right of the chat, one row below the prompt header (which owns row 0
        // while scrolled back), so neither covers the other
        let toast_row = u16::from(self.prompt_header.is_some());
        let success = self.theme.style(Tone::Success);
        if let Some(text) = self.live_toast()
            && chat_a.height > toast_row
            && chat_a.width > 4
        {
            // clip_to_width pads to the full width: trim so the toast hugs the right edge
            let label = clip_to_width(&format!(" ✓ {text}"), chat_a.width as usize - 3).trim_end().to_string() + " ";
            let w = display_width(&label) as u16;
            let area = Rect {
                x: chat_a.x + chat_a.width.saturating_sub(w + 1),
                y: chat_a.y + toast_row,
                width: w,
                height: 1,
            };
            f.render_widget(Paragraph::new(label).style(success.reversed()), area);
        }
        self.jump_area = Rect::default();
        if jump_h == 1 {
            // widest label that fits — on a narrow phone terminal the hint drops first
            let n = self.unread_rows;
            let news = if n > 0 {
                format!("{n} 行新内容 · ")
            } else {
                String::new()
            };
            let labels = [
                format!(" ↓ {news}ctrl+end 回到底部 "),
                format!(" ↓ {news}点击回到底部 "),
                format!(" ↓ {news}回到底部 "),
                format!(
                    " ↓ {}新 · 回到底部 ",
                    if n > 0 { format!("{n}") } else { String::new() }
                ),
                " ↓ 回到底部 ".to_string(),
                " ↓ ".to_string(),
            ];
            let label = labels
                .into_iter()
                .find(|l| display_width(l) <= jump_a.width as usize)
                .unwrap_or_else(|| " ↓ ".to_string());
            let width = (display_width(&label) as u16).min(jump_a.width);
            self.jump_area = Rect {
                x: jump_a.x + jump_a.width.saturating_sub(width) / 2,
                y: jump_a.y,
                width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(label).style(self.theme.style(Tone::Badge)),
                self.jump_area,
            );
        }

        // /queue dialog: clickable per-item actions
        if self.queue_dialog {
            let n = self.local_queue.len();
            let h = (n.min(10) as u16) + 3 + HINT_ROWS; // rows + rule + footer buttons + borders
            let w = 72u16.min(area.width);
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            self.modal_rect = Some(pop);
            let inner = dialog_frame(&mut self.hint_buttons, f,
                pop,
                "提示队列",
                "↑↓ 选择 · enter 并入 · d 删 · a 全部并入 · c 清空 · 点击按钮 · esc 关闭",
                self.theme,
            );
            let sel = self.queue_sel.min(n.saturating_sub(1));
            self.queue_buttons.clear();
            let iw = inner.width as usize;
            let btn_style = self.theme.style(Tone::Badge);
            let del_style = self.theme.style(Tone::Warning);
            let mut y = inner.y;
            for (i, q) in self.local_queue.iter().enumerate() {
                if i >= 10 {
                    break;
                }
                let head: String = q
                    .text
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(iw.saturating_sub(26))
                    .collect();
                let text_area = Rect {
                    x: inner.x,
                    y,
                    width: inner.width.saturating_sub(24),
                    height: 1,
                };
                let (marker, row_style) = if i == sel {
                    ("❯", self.theme.style(Tone::Badge))
                } else {
                    (" ", Style::default())
                };
                f.render_widget(
                    Paragraph::new(format!("{marker}{}. {head}", i + 1)).style(row_style),
                    text_area,
                );
                // [并入] [删] buttons at the right edge
                let by = y;
                let bx1 = inner.x + inner.width.saturating_sub(16);
                let b1 = Rect {
                    x: bx1,
                    y: by,
                    width: 8,
                    height: 1,
                };
                f.render_widget(Paragraph::new(" [并入] ").style(btn_style), b1);
                let bx2 = inner.x + inner.width.saturating_sub(7);
                let b2 = Rect {
                    x: bx2,
                    y: by,
                    width: 7,
                    height: 1,
                };
                f.render_widget(Paragraph::new(" [删] ").style(del_style), b2);
                self.queue_buttons.push((b1, QueueAction::Supplement(i)));
                self.queue_buttons.push((b2, QueueAction::Remove(i)));
                y += 1;
            }
            if n == 0 {
                f.render_widget(Paragraph::new("（队列为空 · 忙碌时按回车排队）"), inner);
            }
            // footer: [全部并入] [清空]
            let fy = inner.y + inner.height.saturating_sub(1);
            let fb1 = Rect {
                x: inner.x + inner.width / 2 - 18,
                y: fy,
                width: 12,
                height: 1,
            };
            let fb2 = Rect {
                x: inner.x + inner.width / 2 + 4,
                y: fy,
                width: 8,
                height: 1,
            };
            f.render_widget(Paragraph::new(" [全部并入] ").style(btn_style), fb1);
            f.render_widget(Paragraph::new(" [清空] ").style(del_style), fb2);
            self.queue_buttons.push((fb1, QueueAction::SupplementAll));
            self.queue_buttons.push((fb2, QueueAction::ClearAll));
        }

        // the /problems dialog: what language servers report for the files tango edited
        if self.problems_dialog {
            let rows = self.problem_rows();
            let h = (rows.len().min(16) as u16 + 3 + HINT_ROWS).min(area.height);
            let w = 100u16.min(area.width);
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            self.modal_rect = Some(pop);
            let title = format!("诊断 · Problems ({})", rows.len());
            let inner = dialog_frame(
                &mut self.hint_buttons,
                f,
                pop,
                &title,
                "↑↓/滚轮 滚动 · enter 填入输入框 · y 复制 · x 清空 · esc 关闭",
                self.theme,
            );
            self.problems_rows.clear();
            if rows.is_empty() {
                let text = "没有诊断 · tango 编辑文件后，语言服务器报告的问题列在这里";
                f.render_widget(
                    Paragraph::new(text).style(self.theme.style(Tone::Muted)),
                    inner,
                );
            } else {
                // the selection is kept in view: the window follows it (no stored offset)
                let keep = (inner.height as usize).max(1);
                let sel = self.problems_sel.min(rows.len() - 1);
                let start = sel.saturating_sub(keep - 1);
                for (i, (text, severity)) in rows.iter().enumerate().skip(start).take(keep) {
                    let selected = i == sel;
                    let tone = if selected {
                        Tone::Badge
                    } else {
                        severity.tone()
                    };
                    let head = if selected { "❯ " } else { "  " };
                    let text = format!(
                        "{head}{}",
                        clip_to_width(text, inner.width.saturating_sub(2) as usize)
                    );
                    let r = Rect {
                        x: inner.x,
                        y: inner.y + (i - start) as u16,
                        width: inner.width,
                        height: 1,
                    };
                    f.render_widget(Paragraph::new(text).style(self.theme.style(tone)), r);
                    self.problems_rows.push((r, i));
                }
            }
        }
        // the /todo dialog: every unanswered choice and side suggestion, one row each
        if self.todo_dialog {
            let n = self.todos.len();
            let h = (n.min(12) as u16) + 3 + HINT_ROWS; // rows + borders + padding
            let w = 76u16.min(area.width);
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            self.modal_rect = Some(pop);
            let inner = dialog_frame(&mut self.hint_buttons, f,
                pop,
                "待办 · Todo",
                "↑↓ 选择 · enter 完成/展开/执行/答复 · d 删除/忽略 · esc 关闭",
                self.theme,
            );
            self.todo_buttons.clear();
            let btn_style = self.theme.style(Tone::Badge);
            let del_style = self.theme.style(Tone::Warning);
            let iw = inner.width as usize;
            let mut y = inner.y;
            for (i, item) in self.todos.iter().enumerate() {
                if i >= 12 || y >= inner.y + inner.height {
                    break;
                }
                let ascii = self.ascii;
                let (badge, head, b1_label, b2_label, act1, act2) = match &item.kind {
                    TodoKind::Choice { question, .. } => (
                        if ascii { "?" } else { "❓" },
                        question.clone(),
                        "[展开]",
                        "[忽略]",
                        TodoAction::Expand(i),
                        TodoAction::Drop(i),
                    ),
                    TodoKind::Suggest { text } => (
                        if ascii { "*" } else { "💡" },
                        text.clone(),
                        "[执行]",
                        "[丢弃]",
                        TodoAction::Run(i),
                        TodoAction::Drop(i),
                    ),
                    TodoKind::Question { text } => (
                        "？",
                        text.clone(),
                        "[答复]",
                        "[忽略]",
                        TodoAction::Reply(i),
                        TodoAction::Drop(i),
                    ),
                    TodoKind::Note { text, done } => (
                        if ascii {
                            if *done { "x" } else { "o" }
                        } else if *done {
                            "☑"
                        } else {
                            "☐"
                        },
                        text.clone(),
                        if *done { "[取消]" } else { "[完成]" },
                        "[删除]",
                        TodoAction::Done(i),
                        TodoAction::Drop(i),
                    ),
                };
                // a finished note reads as struck through and dim
                let head_style = if matches!(&item.kind, TodoKind::Note { done: true, .. }) {
                    Style::default().add_modifier(Modifier::CROSSED_OUT | Modifier::DIM)
                } else {
                    Style::default()
                };
                let head: String = head.chars().take(iw.saturating_sub(22)).collect();
                let text_area = Rect {
                    x: inner.x,
                    y,
                    width: inner.width.saturating_sub(20),
                    height: 1,
                };
                let (marker, row_style) = if i == self.todo_sel.min(n.saturating_sub(1)) {
                    ("❯", self.theme.style(Tone::Badge))
                } else {
                    (" ", Style::default())
                };
                f.render_widget(
                    Paragraph::new(format!("{marker}{badge} {head}")).style(row_style.patch(head_style)),
                    text_area,
                );
                let b1 = Rect {
                    x: inner.x + inner.width.saturating_sub(14),
                    y,
                    width: 7,
                    height: 1,
                };
                let b2 = Rect {
                    x: inner.x + inner.width.saturating_sub(7),
                    y,
                    width: 7,
                    height: 1,
                };
                f.render_widget(Paragraph::new(b1_label).style(btn_style), b1);
                f.render_widget(Paragraph::new(b2_label).style(del_style), b2);
                self.todo_buttons.push((b1, act1));
                self.todo_buttons.push((b2, act2));
                y += 1;
            }
            if n == 0 {
                f.render_widget(
                    Paragraph::new("（空 · /todo <内容> 记一条；Tango 的 ?choice/?suggest 也收进这里）"),
                    inner,
                );
            }
        }

        // the prompt menu: a small centered dialog anchored to a user bubble
        if self.prompt_menu.is_some() {
            let w = 44u16.min(area.width);
            let h = 6 + HINT_ROWS;
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            self.modal_rect = Some(pop);
            let inner = dialog_frame(&mut self.hint_buttons, f, pop, "这条消息", "数字键或点击 · esc 关闭", self.theme);
            self.prompt_menu_buttons.clear();
            let mut y = inner.y;
            for (n, label) in [
                (1u8, "复制原文（还原换行）"),
                (2u8, "编辑重发（回滚到这句之前）"),
                (3u8, "原样重发"),
                (4u8, "引用到输入框"),
            ] {
                let row = Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                };
                f.render_widget(Paragraph::new(format!("{n}. {label}")), row);
                self.prompt_menu_buttons.push((row, n));
                y += 1;
            }
        }

        // the choices panel: a reserved layout row between chat and input (the
        // chat scrolls up to make room — never covered), question wrapped whole
        if let Some(c) = self.choices.clone()
            && !self.choices_collapsed
            && panel_h > 0
        {
            self.choice_rows.clear();
            let pop = Rect {
                x: panel_a.x + (panel_a.width.saturating_sub(72u16.min(panel_a.width))) / 2,
                y: panel_a.y,
                width: 72u16.min(panel_a.width),
                height: panel_h,
            };
            let title = self.choice_title();
            let inner = dialog_frame(&mut self.hint_buttons, f,
                pop,
                &title,
                "↑↓/点击 选择 · enter 确认（或输编号+enter） · tab 改写自答 · esc 收起",
                self.theme,
            );
            // the question wraps at the dialog width — long asks stay fully readable
            let q_rows = wrap_chat_rows(&[c.question.clone()], inner.width.max(1) as usize);
            let q_style = self.theme.style(Tone::Selected);
            for (qi, qr) in q_rows.iter().enumerate() {
                f.render_widget(
                    Paragraph::new(qr.text.clone()).style(q_style),
                    Rect {
                        x: inner.x,
                        y: inner.y + qi as u16,
                        width: inner.width,
                        height: 1,
                    },
                );
            }
            let mut y = inner.y + q_rows.len() as u16;
            // bar-indented preview block: what the ask is about (commands, paths)
            let bar = self.theme.style(Tone::Muted);
            for line in &c.preview {
                let text: String = line
                    .chars()
                    .take(inner.width.saturating_sub(4) as usize)
                    .collect();
                f.render_widget(
                    Paragraph::new(format!("│ {text}")).style(bar),
                    Rect {
                        x: inner.x + 2,
                        y,
                        width: inner.width.saturating_sub(2),
                        height: 1,
                    },
                );
                y += 1;
            }
            for (i, opt) in c.options.iter().enumerate() {
                let row = Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                };
                let label: String = opt
                    .label
                    .chars()
                    .take(inner.width.saturating_sub(6) as usize)
                    .collect();
                let style = if i == c.sel {
                    self.theme.style(Tone::Badge)
                } else {
                    self.theme.style(Tone::Text)
                };
                let marker = if i == c.sel { "❯" } else { " " };
                f.render_widget(
                    Paragraph::new(format!("{}. {marker} {label}", i + 1)).style(style),
                    row,
                );
                self.choice_rows.push((row, i));
                y += 1;
                // muted annotation row — clicking it picks the option above too
                if let Some(note) = &opt.note {
                    let note_row = Rect {
                        x: inner.x + 4,
                        y,
                        width: inner.width.saturating_sub(4),
                        height: 1,
                    };
                    let note: String = note
                        .chars()
                        .take(inner.width.saturating_sub(8) as usize)
                        .collect();
                    f.render_widget(
                        Paragraph::new(note).style(self.theme.style(Tone::Muted)),
                        note_row,
                    );
                    self.choice_rows.push((note_row, i));
                    y += 1;
                }
            }
            // the built-in explain row: ask the model to walk through every
            // option, then re-present the block
            let erow = Rect {
                x: inner.x,
                y,
                width: inner.width,
                height: 1,
            };
            let n = c.options.len();
            let emarker = if c.sel == n { "❯" } else { " " };
            let estyle = if c.sel == n {
                self.theme.style(Tone::Badge)
            } else {
                self.theme.style(Tone::Muted)
            };
            f.render_widget(
                Paragraph::new(format!(
                    "{}. {emarker} {} 详细解释各选项",
                    n + 1,
                    if self.ascii { "?" } else { "❓" }
                ))
                .style(estyle),
                erow,
            );
            self.choice_rows.push((erow, n));
        }

        // completion popup, glued above the input rule
        if let Some(p) = &self.popup {
            let ph = (p.items.len().min(6) + 2) as u16 + HINT_ROWS;
            let pop = Rect {
                x: 0,
                y: input_a.y.saturating_sub(ph),
                width: area.width,
                height: ph,
            };
            let title = match p.kind {
                PopKind::Slash => "命令补全",
                PopKind::File => "文件引用",
                PopKind::Arg => "参数补全",
                PopKind::History => "输入历史 · 新的在下",
            };
            self.popup_area = pop;
            let hint = match p.kind {
                PopKind::Arg => "tab 下一个 · shift+tab 上一个 · enter 执行 · esc 关闭",
                PopKind::History => "↑↓ 选择 · enter 跳到该处 · esc 关闭",
                PopKind::Slash => "tab 补全 · enter 执行 · esc 关闭",
                PopKind::File => "tab/enter 选择 · esc 关闭",
            };
            let inner = dialog_frame(&mut self.hint_buttons, f, pop, title, hint, self.theme);
            // keep the selection in view: 6 rows show, a long list (models) scrolls
            let skip = p.sel.saturating_sub(5);
            let rows: Vec<Line> = p
                .items
                .iter()
                .enumerate()
                .skip(skip)
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
            f.render_widget(Paragraph::new(rows), inner);
        }

        // input rule: work state on the left, and on long turns a rotating tip on the right
        // (Claude Code-style "⌞ Tip:" without spending a layout row)
        let tip = if self.busy
            && let Some(t0) = self.turn_started
            && t0.elapsed().as_secs() >= 30
        {
            Some(Self::tip_for(t0.elapsed().as_secs()))
        } else {
            None
        };
        let anim = if self.busy && self.animations {
            let since = self.last_output.map_or(std::time::Duration::ZERO, |t| t.elapsed());
            let speed = sweep_speed(since);
            let frames = self.think_frame.saturating_sub(self.sweep_frame).min(8) as u64;
            self.sweep_pos = self.sweep_pos.wrapping_add(frames * speed);
            self.sweep_frame = self.think_frame;
            Some((self.sweep_pos, speed == 0))
        } else {
            None
        };
        let rule_row = Self::rule_line(
            &self.work_text(),
            tip,
            area.width as usize,
            self.theme.style(Tone::Muted),
            self.theme.style(if self.busy {
                Tone::Success
            } else if self.exit_pending() {
                Tone::Warning
            } else {
                Tone::Muted
            }),
            anim,
        );
        f.render_widget(
            Paragraph::new(rule_row),
            Rect {
                x: 0,
                y: input_a.y,
                width: area.width,
                height: 1,
            },
        );
        // the tinted block under the rule: mode bar | pad, draft, pad, info row
        let block = Rect {
            x: 0,
            y: input_a.y + 1,
            width: area.width,
            height: input_a.height.saturating_sub(1),
        };
        let accent = self.mode_color();
        f.render_widget(
            Paragraph::new("").style(Style::default().bg(self.theme.input_bg())),
            block,
        );
        let bar: Vec<Line> = (0..block.height)
            .map(|_| Line::from(Span::styled("▌", Style::default().fg(accent))))
            .collect();
        f.render_widget(
            Paragraph::new(bar),
            Rect { width: 1, ..block },
        );
        let text_w = Self::input_text_w(area.width);
        let layout = self.input_layout(text_w);
        let visible = self.input_visible();
        let input_scroll = layout.cursor_row.saturating_sub(visible - 1);
        let input_lines: Vec<Line> = match self.input_sel {
            Some((a, b)) if a != b => {
                let (a, b) = (a.min(b), a.max(b));
                let hl = Style::default().reversed();
                layout
                    .lines
                    .iter()
                    .zip(&layout.starts)
                    .map(|(line, &start)| {
                        let chars: Vec<char> = line.chars().collect();
                        let from = a.saturating_sub(start).min(chars.len());
                        let to = b.saturating_sub(start).min(chars.len());
                        if start == usize::MAX || from >= to {
                            return Line::from(line.clone());
                        }
                        Line::from(vec![
                            Span::raw(chars[..from].iter().collect::<String>()),
                            Span::styled(chars[from..to].iter().collect::<String>(), hl),
                            Span::raw(chars[to..].iter().collect::<String>()),
                        ])
                    })
                    .collect()
            }
            _ => layout.lines.into_iter().map(Line::from).collect(),
        };
        let text_top = self.input_text_top();
        f.render_widget(
            Paragraph::new(input_lines).scroll((input_scroll as u16, 0)),
            Rect {
                x: INPUT_X,
                y: text_top,
                width: text_w,
                height: visible as u16,
            },
        );
        let info = self.input_info_line(text_w as usize);
        let info_y = block.y + block.height.saturating_sub(1);
        // the mode label and the model part (everything after "mode · ") are click targets
        let mode_w = display_width(&self.mode_label()) as u16;
        self.mode_area = Rect {
            x: INPUT_X,
            y: info_y,
            width: mode_w.min(text_w),
            height: 1,
        };
        self.model_area = Rect::default();
        if !self.model.is_empty() {
            let skip = usize::from(!self.mode_label().is_empty()) * 2;
            let before: usize = info.spans.iter().take(skip).map(|s| display_width(&s.content)).sum();
            let model_w: usize = info.spans.iter().skip(skip).map(|s| display_width(&s.content)).sum();
            self.model_area = Rect {
                x: INPUT_X + before as u16,
                y: info_y,
                width: (model_w as u16).min(text_w.saturating_sub(before as u16)),
                height: 1,
            };
        }
        f.render_widget(
            Paragraph::new(info),
            Rect {
                x: INPUT_X,
                y: info_y,
                width: text_w,
                height: 1,
            },
        );
        f.set_cursor_position(Position::new(
            INPUT_X + layout.cursor_col as u16,
            text_top + (layout.cursor_row - input_scroll) as u16,
        ));

        // model/session/command picker overlay
        if let Some(p) = &self.picker {
            let filtered = p.filtered_items();
            // display rows: a group header precedes the first item of each group
            // (model picker groups by source platform; headers are never selectable)
            enum PickRow<'a> {
                Header(&'a str),
                Item(usize),
            }
            let mut display: Vec<PickRow> = Vec::with_capacity(filtered.len());
            let mut last_group: Option<&str> = None;
            for (i, it) in filtered.iter().enumerate() {
                if !it.group.is_empty() && last_group != Some(it.group.as_str()) {
                    display.push(PickRow::Header(it.group.as_str()));
                    last_group = Some(it.group.as_str());
                }
                display.push(PickRow::Item(i));
            }
            let two_line = Picker::session_scoped(p.kind);
            let per = if two_line { 2usize } else { 1 };
            let line_count = display
                .iter()
                .map(|r| match r {
                    PickRow::Item(_) => per,
                    _ => 1,
                })
                .sum::<usize>();
            let pagination = usize::from(line_count > 14);
            let query_rows = usize::from(p.filterable());
            let shown = if two_line { 14 / per } else { 14 };
            let h = (line_count.min(14) + pagination + query_rows + 2) as u16 + HINT_ROWS;
            let w = (area.width.min(72)).max(44).min(area.width);
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            let title = match p.kind {
                PickerKind::Model => "选择模型",
                PickerKind::Mode => "执行授权 · 选中即切换",
                PickerKind::Session => "选择会话",
                PickerKind::HandoffTarget => "交接给哪个会话 · 当前会话的摘要会并入它",
                PickerKind::TakeoverSource => "接管哪个会话 · 它的摘要会并入当前会话",
                PickerKind::SendTarget => "把消息发给哪个会话 · 选好后再输入内容",
                PickerKind::Timeline => "时间轴 · 选中即回滚到该回合之前",
                PickerKind::Find => "搜索命中 · 本会话=回滚到该处 · 他会话=切换过去",
                PickerKind::Preset => "提示模板 · 选中填入输入框（不发送）",
                PickerKind::Permissions => "允许规则 · /permissions rm <rule> 移除",
                PickerKind::Skills => "技能列表 · global=全局 project=项目",
                PickerKind::Worktree => "worktree · /worktree merge <名> 合并 · rm 删除",
                PickerKind::Trust => "信任此项目的配置？（详情见上方）",
                PickerKind::Command => "命令面板",
                PickerKind::History => "搜索 prompt 历史",
                PickerKind::Theme => "选择主题",
                PickerKind::NvimBackend => "检测到 Neovim，如何查看？",
                PickerKind::EditResolution => "内容已修改，如何处理？",
                PickerKind::Cwd => "选择工作目录",
            };
            let hint = if p.kind == PickerKind::Cwd {
                "输入 /路径 回车直接切换 · ↑↓ 选择用过的目录 · 末项自己输入 · esc 取消"
            } else if p.kind == PickerKind::Model {
                "输入 过滤 · ↑↓/滚轮 选模型 · ←→ 推理力度（随回车一起生效）· enter 确认 · esc 取消"
            } else if p.filterable() {
                "输入 过滤 · ↑↓/滚轮 选择 · enter 确认 · esc 取消"
            } else {
                match p.kind {
                    PickerKind::NvimBackend => "↑↓/滚轮 选择 · enter 确认 · esc 使用内置查看器",
                    PickerKind::EditResolution => "↑↓ 选择 · enter 提交 · esc 取消",
                    PickerKind::Trust => "↑↓ 选择 · enter 确认 · esc 稍后（本次不生效，/trust 再选）",
                    _ => "↑↓ 选择 · enter 确认 · esc 取消",
                }
            };
            self.modal_rect = Some(pop);
            let inner = dialog_frame(&mut self.hint_buttons, f, pop, title, hint, self.theme);
            let label_w = (inner.width as usize).saturating_sub(28);
            let sel_row = display
                .iter()
                .position(|r| matches!(r, PickRow::Item(i) if *i == p.sel))
                .unwrap_or(0);
            let keep = shown.saturating_sub(1);
            let start = if sel_row < keep {
                0
            } else {
                sel_row - (keep - 1)
            };
            self.picker_area = pop;
            let win = &display[start..(start + shown).min(display.len())];
            self.picker_rows = win
                .iter()
                .map(|r| match r {
                    PickRow::Item(i) => Some(*i),
                    PickRow::Header(_) => None,
                })
                .collect();
            let mut rows: Vec<Line> = win
                .iter()
                .flat_map(|slot| -> Vec<Line> {
                    match slot {
                        PickRow::Header(g) => vec![Line::from(Span::styled(
                            format!(" ── {} ──", g),
                            self.theme.style(Tone::Muted),
                        ))],
                        PickRow::Item(i) => {
                            let it = filtered[*i];
                            let marks = format!(
                                "{}{} {}",
                                if it.warn { "⚠" } else { " " },
                                if it.current { "✓" } else { " " },
                                if *i == p.sel { "▸" } else { " " }
                            );
                            let style = if it.warn {
                                self.theme.style(Tone::Muted)
                            } else if *i == p.sel {
                                self.theme.style(Tone::Selected)
                            } else {
                                self.theme.style(Tone::Text)
                            };
                            if two_line {
                                // Claude Code style: title row + muted meta row beneath
                                let note: String = it
                                    .note
                                    .chars()
                                    .take((inner.width as usize).saturating_sub(8))
                                    .collect();
                                vec![
                                    Line::from(Span::styled(
                                        format!(" {marks} {}", it.label),
                                        style,
                                    )),
                                    Line::from(Span::styled(
                                        format!("      {note}"),
                                        self.theme.style(Tone::Muted),
                                    )),
                                ]
                            } else {
                                let pad = label_w.saturating_sub(display_width(&it.label));
                                // the model picker names the reasoning level beside the context window
                                let note = match p.effort_shown(&it.id) {
                                    Some(level) if it.note.is_empty() => format!("effort {level}"),
                                    Some(level) => format!("{} · effort {level}", it.note),
                                    None => it.note.clone(),
                                };
                                let note: String = note
                                    .chars()
                                    .take((inner.width as usize).saturating_sub(label_w + 9))
                                    .collect();
                                vec![Line::from(Span::styled(
                                    format!(" {marks} {}{} {}", it.label, " ".repeat(pad), note),
                                    style,
                                ))]
                            }
                        }
                    }
                })
                .collect();
            if p.filterable() {
                rows.insert(
                    0,
                    Line::from(Span::styled(
                        format!(
                            " {}: {}",
                            if p.kind == PickerKind::Cwd {
                                "目录"
                            } else {
                                "搜索"
                            },
                            p.query
                        ),
                        self.theme.style(Tone::Muted),
                    )),
                );
            }
            if display.len() > 14 {
                let nums: Vec<usize> = win
                    .iter()
                    .filter_map(|r| match r {
                        PickRow::Item(i) => Some(*i),
                        PickRow::Header(_) => None,
                    })
                    .collect();
                let first = nums.first().copied().unwrap_or(0);
                let last = nums.last().copied().unwrap_or(first);
                rows.push(Line::from(Span::styled(
                    format!("   {}–{} / {}", first + 1, last + 1, filtered.len()),
                    self.theme.style(Tone::Muted),
                )));
            }
            f.render_widget(Paragraph::new(rows), inner);
        }

        // permission modal
        if let Some(p) = &self.permission {
            let w = (area.width.min(76)).max(40).min(area.width);
            // the full command wraps; the window shows at most 8 rows and scrolls
            let wrap_w = (w as usize).saturating_sub(2);
            self.perm_wrap_w = wrap_w;
            let cmd_rows = wrap_chat_rows(&[p.detail.clone()], wrap_w);
            let max_rows = 8.min(area.height.saturating_sub(6 + HINT_ROWS) as usize);
            let total = cmd_rows.len();
            let max_scroll = total.saturating_sub(max_rows);
            self.perm_scroll = self.perm_scroll.min(max_scroll);
            let shown = cmd_rows
                .iter()
                .skip(self.perm_scroll)
                .take(max_rows)
                .map(|r| Line::from(Span::styled(r.text.clone(), self.theme.style(Tone::Muted))))
                .collect::<Vec<_>>();
            let brief_rows = usize::from(!p.brief.is_empty());
            let h = (3 + brief_rows + shown.len() + usize::from(total > max_rows)) as u16 + HINT_ROWS;
            let h = h.max(5 + HINT_ROWS).min(area.height);
            let popup = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            // what "a" remembers, up front: for a multi-command line every command
            // word, so a risky tail does not ask again next time (and the user sees
            // how broad that is before pressing it)
            let mut hint = String::from("y 允许 · n 拒绝");
            match Self::always_hint(&p) {
                Some(what) => hint.push_str(&format!(" · a 始终允许（记住 {what}）")),
                None => hint.push_str(" · a 始终允许"),
            }
            hint.push_str(" · c 复制命令");
            if total > max_rows {
                hint = format!(
                    "↑↓/滚轮 滚动（{}/{} 行） · {}",
                    self.perm_scroll + 1,
                    total,
                    hint
                );
            }
            self.modal_rect = Some(popup);
            let inner = dialog_frame(&mut self.hint_buttons, f, popup, &p.name, &hint, self.theme);
            let mut body = Vec::new();
            if !p.brief.is_empty() {
                body.push(Line::from(Span::styled(
                    p.brief.clone(),
                    self.theme.style(Tone::Selected),
                )));
            }
            body.push(Line::from(Span::styled(
                p.reason.clone(),
                self.theme.style(Tone::Warning),
            )));
            body.push(Line::from(""));
            body.extend(shown);
            f.render_widget(Paragraph::new(body), inner);
        }

        // the full-access (auto) acknowledgement dialog
        if let Some(ack) = self.full_access {
            let w = area.width.min(64).max(40);
            let h = (12 + HINT_ROWS).min(area.height);
            let popup = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            let hint = if ack {
                "enter 确认启用 · esc 取消".to_string()
            } else {
                "空格 勾选了解风险 · esc 取消".to_string()
            };
            self.modal_rect = Some(popup);
            let inner = dialog_frame(&mut self.hint_buttons, f, popup, "⚠ 要启用完全放行吗？", &hint, self.theme);
            let mut body = vec![
                Line::from(Span::styled(
                    "启用后，任务可以直接执行以下操作，无需逐次请求你确认：",
                    self.theme.style(Tone::Muted),
                )),
                Line::from(""),
            ];
            for line in [
                "• 写入、编辑任何路径下的文件（包括项目外）",
                "• 执行任意 shell 命令（包括 rm、git push、安装依赖）",
                "• 访问网络、调用外部服务",
            ] {
                body.push(Line::from(Span::styled(line.to_string(), self.theme.style(Tone::Text))));
            }
            body.push(Line::from(""));
            body.push(Line::from(Span::styled(
                "出错时可能无法撤销。建议只在可信的仓库或隔离环境里使用。",
                self.theme.style(Tone::Warning),
            )));
            body.pop(); // the last blank row: the checkbox takes it
            let ascii = self.ascii;
            let box_char = if ascii {
                if ack { "[x]" } else { "[ ]" }
            } else if ack {
                "☑"
            } else {
                "☐"
            };
            let check_style = if ack {
                self.theme.style(Tone::Error)
            } else {
                self.theme.style(Tone::Muted)
            };
            let check_text = format!("{box_char} 我已了解风险，并对自己的数据安全负责");
            f.render_widget(Paragraph::new(body), inner);
            // last inner row: the checkbox (a button) on the left, 启用 on the right
            let row_y = inner.y + inner.height.saturating_sub(1);
            self.full_access_buttons.clear();
            let check_w = inner.width.saturating_sub(12).max(1);
            let check_rect = Rect { x: inner.x, y: row_y, width: check_w, height: 1 };
            let marker_style = self.theme.style(Tone::Badge);
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("❯ ", marker_style),
                    Span::styled(
                        check_text.chars().take(check_w as usize - 2).collect::<String>(),
                        check_style,
                    ),
                ])),
                check_rect,
            );
            self.full_access_buttons.push((check_rect, FullAccessAction::Toggle));
            let enable_rect = Rect { x: inner.x + inner.width.saturating_sub(9), y: row_y, width: 8, height: 1 };
            f.render_widget(
                Paragraph::new(if ack { "[ 启用 ]" } else { "[ 启用 ]" }).style(if ack {
                    self.theme.style(Tone::Error)
                } else {
                    Style::default().add_modifier(Modifier::DIM)
                }),
                enable_rect,
            );
            self.full_access_buttons.push((enable_rect, FullAccessAction::Enable));
        } else {
            self.full_access_buttons.clear();
        }

        if let Some(viewer) = self.viewer.as_mut() {
            let w = (area.width * 8 / 10).max(20).min(area.width);
            let h = (area.height * 8 / 10).max(8).min(area.height);
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            let title = viewer.title.clone();
            let hint = if viewer.transcript {
                "/ 搜索 · n/N 匹配 · {/} prompt · y 复制 · esc 关闭"
            } else {
                "y 复制 · esc 关闭 · ↑↓/滚轮滚动 · ctrl+e 用 nvim 打开"
            };
            self.modal_rect = Some(pop);
            let inner = dialog_frame(&mut self.hint_buttons, f, pop, &title, hint, self.theme);
            let inner_width = inner.width.max(1) as usize;
            let search_rows =
                usize::from(viewer.transcript && (viewer.search_input || !viewer.query.is_empty()));
            let inner_height = inner.height.saturating_sub(search_rows as u16) as usize;
            let mut offset = 0;
            viewer.line_offsets = viewer
                .text
                .lines()
                .map(|line| {
                    let current = offset;
                    offset += display_width(line).div_ceil(inner_width).max(1);
                    current
                })
                .collect();
            viewer.max_scroll = offset.saturating_sub(inner_height);
            viewer.scroll = viewer.scroll.min(viewer.max_scroll);
            let body = Paragraph::new(viewer.text.clone())
                .wrap(Wrap { trim: false })
                .scroll((viewer.scroll.min(u16::MAX as usize) as u16, 0));
            f.render_widget(
                body,
                Rect {
                    height: inner_height as u16,
                    ..inner
                },
            );
            if search_rows > 0 {
                let status = if viewer.query.is_empty() {
                    String::new()
                } else if viewer.matches.is_empty() {
                    " · 无匹配".into()
                } else {
                    format!(" · {}/{}", viewer.match_index + 1, viewer.matches.len())
                };
                let search_area = Rect {
                    y: inner.y + inner_height as u16,
                    height: 1,
                    ..inner
                };
                f.render_widget(
                    Paragraph::new(format!("/{}{status}", viewer.query))
                        .style(self.theme.style(Tone::Selected)),
                    search_area,
                );
                if viewer.search_input {
                    f.set_cursor_position(Position::new(
                        search_area.x + 1 + display_width(&viewer.query) as u16,
                        search_area.y,
                    ));
                }
            }
        }
        // a modal dialog stands on a scrim: the rest of the screen is dimmed, so its surface is
        // set apart whatever the terminal's own background is
        if let Some(keep) = self.modal_rect {
            crate::dialog::dim_outside(f, keep, self.theme);
        }
    }

    /// Status bar spans fitted to `width`; a custom {placeholder} template replaces the whole bar.
    /// On a narrow terminal the least useful segments go first (ctx, sess, the pinned ▶),
    /// then the model name shortens — work/idle, cost and the ❓ badge always stay.
    fn status_spans(&mut self, width: u16) -> Vec<Span<'static>> {
        // while a turn runs, replace the dumb "working" with live numbers: how long
        // it's been and how much has arrived — the "is it stuck?" check at a glance
        let work = self.work_text();
        if let Some(template) = self.statusline.clone() {
            let percent = if self.context_window > 0 {
                self.context_tokens.saturating_mul(100) / self.context_window
            } else {
                0
            };
            let raw = template
                .replace("{model}", &self.model)
                .replace("{mode}", &self.mode)
                .replace("{cost}", &format!("{:.4}", self.cost))
                .replace("{session}", &self.session)
                .replace(
                    "{tokens}",
                    &Self::token_count(self.context_tokens).to_string(),
                )
                .replace(
                    "{ctx}",
                    &format!(
                        "{}/{}",
                        Self::token_count(self.context_tokens),
                        Self::token_count(self.context_window)
                    ),
                )
                .replace("{pct}", &format!("{percent}%"))
                .replace("{work}", &work);
            return vec![
                Span::styled(self.history_label(), self.theme.style(Tone::Badge)),
                Span::styled(
                    format!(
                        " {} ",
                        clip_to_width(raw.trim(), (width as usize).saturating_sub(2)).trim_end()
                    ),
                    self.theme.style(Tone::Badge),
                ),
            ];
        }
        // (segment, drop order: lower goes first)
        // model and mode live in the input block's info row now
        let mut segs: Vec<(Span<'static>, u8)> = vec![
            // the history emoji leads the bar; it is only a shortcut, so on a narrow
            // screen it goes before the cost, the ❓ badge and the problems count
            (
                Span::styled(self.history_label(), self.theme.style(Tone::Badge)),
                5,
            ),
            (
                Span::styled(
                    format!(" ${:.4} ", self.cost),
                    self.theme.style(Tone::Warning),
                ),
                10,
            ),
            (
                Span::styled(
                    format!(
                        " sess {} ",
                        if self.session_name.is_empty() {
                            self.session.clone()
                        } else {
                            self.session_name.chars().take(16).collect::<String>()
                        }
                    ),
                    self.theme.style(Tone::Muted),
                ),
                1,
            ),
        ];
        if self.context_window > 0 {
            let percent = self.context_tokens.saturating_mul(100) / self.context_window;
            segs.push((
                Span::styled(
                    format!(
                        " ctx {}/{} {}% ",
                        Self::token_count(self.context_tokens),
                        Self::token_count(self.context_window),
                        percent
                    ),
                    self.theme.style(self.context_tone()),
                ),
                0,
            ));
        }
        if let Some(p) = self.current_prompt.clone() {
            segs.push((
                Span::styled(format!(" ▶ {p} "), self.theme.style(Tone::Badge)),
                2,
            ));
        }
        if !self.todos.is_empty() {
            segs.push((
                Span::styled(
                    format!(" {}{}待 /todo ", if self.ascii { "?" } else { "❓" }, self.todos.len()),
                    self.theme.style(Tone::Warning),
                ),
                8,
            ));
        }
        // errors and warnings among the language servers' problems; info and hints are
        // in /problems only. The badge starts with ✗/⚠ (ASCII: !/w), which is how the
        // click target and fit_status recognise it
        let (errors, warnings) = self.problem_counts();
        if errors + warnings > 0 {
            let (x, w) = if self.ascii {
                ("!", "w")
            } else {
                ("✗", "⚠")
            };
            let mut text = String::new();
            if errors > 0 {
                text.push_str(&format!(" {x}{errors}"));
            }
            if warnings > 0 {
                text.push_str(&format!(" {w}{warnings}"));
            }
            text.push_str(" /problems ");
            let tone = if errors > 0 {
                Tone::Error
            } else {
                Tone::Warning
            };
            segs.push((Span::styled(text, self.theme.style(tone)), 9));
        }
        if self.focus {
            segs.push((Span::styled(" focus ", self.theme.style(Tone::Badge)), 4));
        }
        if self.goal_text.is_some() {
            segs.push((Span::styled(" ●goal ", self.theme.style(Tone::Badge)), 4));
        }
        if self.vim {
            let mode = if self.vim_command.is_some() {
                "COMMAND"
            } else if self.edit_mode == EditMode::Insert {
                "INSERT"
            } else {
                "NORMAL"
            };
            segs.push((
                Span::styled(format!(" VIM {mode} "), self.theme.style(Tone::Badge)),
                7,
            ));
        }
        Self::fit_status(segs, width as usize)
    }

    /// Drop the lowest-priority segments until the bar fits; shorten the model name and
    /// the pinned prompt before dropping them. Whatever is left is clipped, never wrapped.
    fn fit_status(mut segs: Vec<(Span<'static>, u8)>, width: usize) -> Vec<Span<'static>> {
        let total = |segs: &[(Span<'static>, u8)]| -> usize {
            segs.iter().map(|(s, _)| display_width(&s.content)).sum()
        };
        while total(&segs) > width {
            let Some((i, _)) = segs
                .iter()
                .enumerate()
                .min_by_key(|(_, (s, p))| (*p, s.content.len()))
            else {
                break;
            };
            let text = segs[i].0.content.trim().to_string();
            // shrink before dropping: pinned prompt → 16, ❓ badge → just the count
            let limit = if text.starts_with('▶') {
                16
            } else if text.starts_with('❓') || text.starts_with('?') {
                4
            } else if matches!(text.chars().next(), Some('✗' | '⚠' | '!' | 'w')) {
                // the problems badge → its first count alone (never a clipped marker)
                let head = text.split_whitespace().next().unwrap_or("").to_string();
                let style = segs[i].0.style;
                segs[i].0 = Span::styled(format!(" {head} "), style);
                continue;
            } else if text.chars().next().is_some_and(|c| c == '●') || text == "idle" {
                // the work segment (busy dot, or idle):
                // counter first, then the timer — the dot stays to the last
                if text == "idle" {
                    4 // never clipped to a stub; dropped whole when out of room
                } else if display_width(&text) > 9 {
                    9
                } else {
                    1
                }
            } else {
                0
            };
            if limit > 0 && display_width(&text) > limit {
                let style = segs[i].0.style;
                let short = clip_to_width(&text, limit);
                segs[i].0 = Span::styled(format!(" {} ", short.trim_end()), style);
                continue;
            }
            if segs.len() == 1 {
                let style = segs[i].0.style;
                segs[i].0 = Span::styled(clip_to_width(&text, width.saturating_sub(2)), style);
                break;
            }
            segs.remove(i);
        }
        segs.into_iter().map(|(s, _)| s).collect()
    }

    /// 42s · 13m22s · 1h30m
    fn fmt_elapsed(secs: u64) -> String {
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m{:02}s", secs / 60, secs % 60)
        } else {
            format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
        }
    }

    /// a first Ctrl+C is waiting for its second press
    fn exit_pending(&self) -> bool {
        self.exit_armed.is_some_and(|t| t.elapsed() < EXIT_WINDOW)
    }

    /// "● 12s · ↓14.0k" (a steady dot) while a turn runs, "idle" otherwise.
    /// Lives on the input rule's
    /// left end (and in a custom statusline's {work}); the default status bar no longer carries it.
    fn work_text(&self) -> String {
        if !self.busy {
            if self.exit_pending() {
                return EXIT_HINT.to_string();
            }
            return "idle".to_string();
        }
        match self.turn_started {
            Some(t0) => {
                let ms = t0.elapsed().as_millis();
                format!(
                    "{} {} · ↓{}",
                    "●",
                    Self::fmt_elapsed(ms as u64 / 1000),
                    Self::fmt_tokens(self.turn_chars)
                )
            }
            None => "● working".to_string(),
        }
    }

    /// The input rule: work state on the left, a rotating tip on the right, dashes between.
    /// Too narrow for both → the tip goes; narrower still → the work text sheds the counter,
    /// then the timer, keeping the dot. `anim` (busy only) is the
    /// sweep's (head, stalled): the leading ● stays steady, the dashes carry the cyan band,
    /// and a stalled turn greys the dot and freezes the band out.
    fn rule_line(
        work: &str,
        tip: Option<&str>,
        width: usize,
        dash: Style,
        live: Style,
        anim: Option<(u64, bool)>,
    ) -> Line<'static> {
        let mut work = work.to_string();
        let mut tip = tip.map(str::to_string);
        let room = width.saturating_sub(4); // "─ " left, " ─" right
        loop {
            let need = display_width(&work) + tip.as_ref().map_or(0, |t| display_width(t) + 3);
            if need <= room || (tip.is_none() && display_width(&work) <= room) {
                break;
            }
            if tip.is_some() {
                tip = None;
            } else if let Some((head, _)) = work.split_once(" · ") {
                work = head.to_string();
            } else if let Some((dancer, _)) = work.split_once(' ') {
                work = dancer.to_string();
            } else {
                break;
            }
        }
        let right = tip.map(|t| format!(" {t} ─")).unwrap_or_default();
        let Some((head, stalled)) = anim else {
            let left = format!("─ {work} ");
            let fill = width.saturating_sub(display_width(&left) + display_width(&right));
            return Line::from(vec![
                Span::styled(left, live),
                Span::styled("─".repeat(fill), dash),
                Span::styled(right, dash),
            ]);
        };
        let mut spans = vec![Span::styled("─ ", dash)];
        let rest = match work.strip_prefix('●') {
            Some(rest) => {
                // steady while working; greys out with the rest on a stall
                let dot = if stalled {
                    dash
                } else {
                    live.add_modifier(ratatui::style::Modifier::BOLD)
                };
                spans.push(Span::styled("●", dot));
                rest.to_string()
            }
            None => work.clone(),
        };
        spans.push(Span::styled(format!("{rest} "), if stalled { dash } else { live }));
        let used = 2 + display_width(&work) + 1;
        let fill = width.saturating_sub(used + display_width(&right));
        // the dashes, cell by cell where the band lights them, merged runs elsewhere
        let mut run = 0usize;
        for x in 0..fill {
            match sweep_color(x, head, fill, stalled) {
                Some(c) => {
                    if run > 0 {
                        spans.push(Span::styled("─".repeat(run), dash));
                        run = 0;
                    }
                    spans.push(Span::styled("─", dash.fg(c)));
                }
                None => run += 1,
            }
        }
        if run > 0 {
            spans.push(Span::styled("─".repeat(run), dash));
        }
        spans.push(Span::styled(right, dash));
        Line::from(spans)
    }

    /// The working dot, dancing: a couple swaying and dipping the tango, one frame
    /// once per heartbeat tick off the turn clock, so every step is actually drawn.
    /// Every frame is 3 columns wide so the rest of the status bar never shifts.
    /// The working figure, dancing the tango in braille: a 6×4-dot stick
    /// figure — head, torso, swinging arms, stepping legs — one pose per
    /// 120ms animation frame (the busy heartbeat), eight poses to a full
    /// sway. Every frame is 3 cells wide so the rest of the line never shifts.
    fn tango_frame(ms: u128) -> String {
        const POSES: [&[(u8, u8)]; 8] = [
            &[(2, 0), (3, 0), (2, 1), (3, 1), (2, 2), (3, 2), (1, 0), (4, 0), (1, 3), (4, 3)], // arms up, legs apart
            &[(2, 0), (3, 0), (2, 1), (3, 1), (2, 2), (3, 2), (1, 2), (4, 2), (2, 3), (3, 3)], // arms down, feet together
            &[(2, 0), (3, 0), (2, 1), (3, 1), (2, 2), (3, 2), (0, 0), (4, 2), (1, 3), (3, 3)], // lean left
            &[(2, 0), (3, 0), (2, 1), (3, 1), (2, 2), (3, 2), (1, 1), (4, 1), (2, 3), (3, 3)], // arms mid
            &[(2, 0), (3, 0), (2, 1), (3, 1), (2, 2), (3, 2), (1, 0), (4, 0), (1, 3), (4, 3)], // arms up, legs apart
            &[(2, 0), (3, 0), (2, 1), (3, 1), (2, 2), (3, 2), (1, 2), (4, 2), (2, 3), (3, 3)], // arms down, feet together
            &[(2, 0), (3, 0), (2, 1), (3, 1), (2, 2), (3, 2), (5, 0), (1, 2), (4, 3), (2, 3)], // lean right
            &[(2, 0), (3, 0), (2, 1), (3, 1), (2, 2), (3, 2), (1, 1), (4, 1), (2, 3), (3, 3)], // arms mid
        ];
        Self::braille_fig(POSES[((ms / 120) % POSES.len() as u128) as usize])
    }

    /// Render dot coordinates on a 6×4 canvas as three braille cells
    /// (2×4 dots each: dot1..8 bits of U+2800).
    fn braille_fig(dots: &[(u8, u8)]) -> String {
        const BIT: [[u16; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
        let mut cells = [0u16; 3];
        for &(x, y) in dots {
            if let (c @ 0..=2, y @ 0..=3) = (x / 2, y) {
                cells[c as usize] |= BIT[(x % 2) as usize][y as usize];
            }
        }
        cells
            .iter()
            .map(|&m| char::from_u32(0x2800 + m as u32).unwrap())
            .collect()
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// Width of the "tango" wordmark in block letters (letters + the spaces between them).
    const WORDMARK_W: usize = 26;

    /// The wordmark's 5 rows, centered in `chat_w`: styled lines for drawing, and
    /// the same rows as plain text padded to the chat width (so the chat's wrapper
    /// never re-wraps or re-centers them) for the row bookkeeping.
    fn wordmark_lines(&self, chat_w: usize) -> (Vec<Line<'static>>, Vec<String>) {
        const W: usize = App::WORDMARK_W;
        const FONT: [&[&str; 5]; 5] = [
            // t
            &["█████", "  █  ", "  █  ", "  █  ", "  █  "],
            // a
            &[" ██ ", "█  █", "████", "█  █", "█  █"],
            // n
            &["█  █", "██ █", "█ ██", "█  █", "█  █"],
            // g
            &["███ ", "█   ", "█ ██", "█  █", "███ "],
            // o
            &[" ███ ", "█   █", "█   █", "█   █", " ███ "],
        ];
        const GRAD: [(u8, u8, u8); 5] = [
            (140, 235, 250),
            (110, 215, 240),
            (80, 190, 225),
            (55, 160, 205),
            (35, 125, 180),
        ];
        const TWINKLE: [f32; 5] = [0.68, 0.82, 0.95, 1.08, 1.18];
        // animations off: a fixed twinkle phase and a still dancer
        let t = if self.animations { Self::now_secs() } else { 0 };
        let tick = (t % 60) as u128;
        let truecolor = supports_truecolor();
        let theme_k = if self.theme.light { 0.8 } else { 1.0 };
        let mut lines: Vec<Line> = Vec::new();
        for r in 0..5 {
            // centering margin first, then a single space between letters
            let mut spans: Vec<Span> = vec![Span::raw(" ".repeat(chat_w.saturating_sub(W) / 2))];
            let mut col = 0usize; // global wordmark column, drives the twinkle
            for (li, letter) in FONT.iter().enumerate() {
                if li > 0 {
                    spans.push(Span::raw(" "));
                }
                for c in letter[r].chars() {
                    if c == '█' {
                        let style = if truecolor {
                            // grok's logo shimmer formula over the row gradient
                            let idx =
                                ((col as u128 * 7 + tick * 3 + col as u128 * tick) % 5) as usize;
                            let k = TWINKLE[idx] * theme_k;
                            let (cr, cg, cb) = GRAD[r];
                            Style::default().fg(Color::Rgb(
                                (cr as f32 * k).min(255.0) as u8,
                                (cg as f32 * k).min(255.0) as u8,
                                (cb as f32 * k).min(255.0) as u8,
                            ))
                        } else {
                            Style::default().fg(Color::Cyan)
                        };
                        spans.push(Span::styled("█", style));
                    } else {
                        spans.push(Span::raw(" "));
                    }
                    col += 1;
                }
            }
            lines.push(Line::from(spans));
        }
        let mut plain: Vec<String> = Vec::new();
        for line in &lines {
            let mut text = String::new();
            for sp in &line.spans {
                text.push_str(sp.content.as_ref());
            }
            plain.push(format!("{text}{}", " ".repeat(chat_w.saturating_sub(display_width(&text)))));
        }
        (lines, plain)
    }

    /// The empty-session splash: a "tango" wordmark in 5×5 block letters —
    /// per-row cyan gradient with grok's star twinkle on top (each cell steps
    /// a 5-brightness palette by (i*7 + t*3 + i*t) % 5) — over a slowly posing
    /// braille dancer and a hint line. None once any conversation content
    /// exists, while a stream runs, or when the terminal is too narrow/short.
    fn splash_lines(&self, chat_w: usize, chat_h: usize) -> Option<Vec<Line<'static>>> {
        const W: usize = App::WORDMARK_W;
        if chat_w < W + 4
            || chat_h < 9
            || !self.stream.is_empty()
            || self.lines.iter().any(|l| !l.notice && !l.text.starts_with("tango serve"))
        {
            return None;
        }
        let (lines, _) = self.wordmark_lines(chat_w);
        let mut lines: Vec<Line> = lines;
        let mut block_h = 5;
        // the dancer's pose follows the same clock as the twinkle (still when animations are off)
        let t = if self.animations { Self::now_secs() } else { 0 };
        block_h += 3; // dancer row + blank + hint
        let center = |w: usize, text: &str| {
            format!("{}{}", " ".repeat((w.saturating_sub(display_width(text))) / 2), text)
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            center(chat_w, &Self::tango_frame(((t % 8) as u128) * 120)),
            self.theme.style(Tone::User),
        )));
        lines.push(Line::from(Span::styled(
            center(chat_w, "输入问题开始 · /help 查看命令"),
            self.theme.style(Tone::Muted),
        )));
        // startup notices (e.g. the same-cwd concurrency warning) sit under the hint,
        // wrapped narrower than the chat and centered row by row
        let notices: Vec<&ChatLine> = self.lines.iter().filter(|l| l.notice).collect();
        if !notices.is_empty() {
            let texts: Vec<String> = notices.iter().map(|l| l.text.clone()).collect();
            let rows = wrap_chat_rows(&texts, chat_w.saturating_sub(4).min(72));
            // too tall to fit beside the wordmark: fall back to the plain chat so
            // the notice is never hidden
            if block_h + 1 + rows.len() > chat_h {
                return None;
            }
            lines.push(Line::from(""));
            block_h += 1 + rows.len();
            for row in rows {
                lines.push(Line::from(Span::styled(
                    center(chat_w, row.text.trim_end()),
                    self.theme.style(notices[row.line].tone),
                )));
            }
        }
        let mut out: Vec<Line> = vec![Line::from(""); chat_h.saturating_sub(block_h) / 2];
        out.extend(lines);
        Some(out)
    }

    /// 500 · 14.0k · 1.5M
    fn fmt_tokens(n: u64) -> String {
        if n < 1000 {
            format!("{n}")
        } else if n < 1_000_000 {
            format!("{:.1}k", n as f64 / 1000.0)
        } else {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        }
    }

    /// The tip showing right now: pool member rotating every 20s of the turn.
    fn tip_for(elapsed_secs: u64) -> &'static str {
        TIPS[((elapsed_secs / 20) as usize) % TIPS.len()]
    }

    /// Printed to the plain terminal after the TUI tears down: how to get back.
    /// None when serve never handshook (no session to resume).
    pub fn exit_note(&self) -> Option<String> {
        if self.session_full.is_empty() {
            None
        } else {
            Some(format!(
                "session {} · 恢复：tango --resume {}",
                self.session_full,
                short_session(&self.session_full)
            ))
        }
    }

    fn mode_color(&self) -> Color {
        self.theme.mode_color(&self.mode, self.plan_mode)
    }

    /// The label the input block shows for the current mode: `plan` overrides.
    fn mode_label(&self) -> String {
        if self.plan_mode { "plan".into() } else { self.mode.clone() }
    }

    /// the level in use belongs to the current model: keep it for when the model comes back
    fn remember_effort(&mut self) {
        if self.model.is_empty() {
            return;
        }
        if self.effort == "off" {
            self.effort_memory.remove(&self.model);
        } else {
            self.effort_memory.insert(self.model.clone(), self.effort.clone());
        }
    }

    /// `ask · claude-opus-5-5 apikeyfan · effort max` — mode in its accent, model and level
    /// bright, provider dim. Narrow widths drop the level, then the provider, then clip the model.
    fn input_info_line(&self, width: usize) -> Line<'static> {
        let (provider, name) = match self.model.split_once('/') {
            Some((p, n)) => (p.to_string(), n.to_string()),
            None => (String::new(), self.model.clone()),
        };
        let mode = self.mode_label();
        let mut spans = Vec::new();
        // before `hello` arrives mode/model are empty: show only what is known
        if !mode.is_empty() {
            spans.push(Span::styled(mode.clone(), Style::default().fg(self.mode_color())));
            if !name.is_empty() {
                spans.push(Span::styled(" · ".to_string(), self.theme.style(Tone::Muted)));
            }
        }
        if name.is_empty() {
            return Line::from(spans);
        }
        let used = if mode.is_empty() { 0 } else { display_width(&mode) + 3 };
        let room = width.saturating_sub(used);
        let full = display_width(&name) + 1 + display_width(&provider);
        if !provider.is_empty() && full <= room {
            spans.push(Span::styled(name, self.theme.style(Tone::Text)));
            spans.push(Span::styled(format!(" {provider}"), self.theme.style(Tone::Muted)));
        } else {
            spans.push(Span::styled(clip_to_width(&name, room), self.theme.style(Tone::Text)));
        }
        // the reasoning level in use, when one is set: ` · effort max`, bright as the model.
        // It goes when the row is too narrow for it (the model keeps its place).
        if self.effort != "off" {
            let shown = format!(" · effort {}", self.effort);
            let left = room.saturating_sub(full.min(room));
            if !self.model.is_empty() && display_width(&shown) <= left {
                spans.push(Span::styled(shown, self.theme.style(Tone::Text)));
            }
        }
        Line::from(spans)
    }

    /// Footer under the input block: `~/dir:branch` on the left (dim), key hints on the right.
    fn footer_line(&self, width: usize) -> Line<'static> {
        let home = std::env::var("HOME").unwrap_or_default();
        let dir = self
            .serve_cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let dir = match dir.strip_prefix(&home) {
            Some(rest) if !home.is_empty() => format!("~{rest}"),
            _ => dir,
        };
        let place = if self.serve_branch.is_empty() {
            dir
        } else {
            format!("{dir}:{}", self.serve_branch)
        };
        let hints: [(&str, &str); 2] = [("shift+tab", "mode"), ("ctrl+p", "commands")];
        let hint_w: usize = hints.iter().map(|(k, d)| display_width(k) + 1 + display_width(d) + 2).sum();
        let key = self.theme.style(Tone::Text);
        let muted = self.theme.style(Tone::Muted);
        let mut spans = Vec::new();
        // the place keeps priority; hints only when they fit beside it
        let show_hints = width >= display_width(&place) + 1 + hint_w;
        let place_room = if show_hints { width - hint_w } else { width };
        let place = clip_to_width(&place, place_room);
        let pad = place_room.saturating_sub(display_width(&place));
        spans.push(Span::styled(place, muted));
        spans.push(Span::raw(" ".repeat(pad)));
        if show_hints {
            for (k, d) in hints {
                spans.push(Span::styled(format!("  {k}"), key));
                spans.push(Span::styled(format!(" {d}"), muted));
            }
        }
        Line::from(spans)
    }

    /// plain text of the status bar (tests and /statusline preview)
    /// Tests read the status bar through it; draw() renders the segments directly.
    #[cfg(test)]
    fn status_segments_text(&mut self) -> String {
        self.status_segments_text_at(200)
    }

    /// the chat line the top row of the view shows (after a draw)
    #[cfg(test)]
    fn top_row_line(&self) -> usize {
        let chat_h = self.chat_area.height as usize;
        let offset = self
            .chat_rows
            .len()
            .saturating_sub(chat_h + self.scroll_back);
        self.chat_rows[offset].line
    }

    /// the same at a given width (the narrow-screen tests)
    #[cfg(test)]
    fn status_segments_text_at(&mut self, width: u16) -> String {
        self.status_spans(width)
            .iter()
            .map(|s| s.content.to_string())
            .collect::<Vec<_>>()
            .join("")
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn always_rules_derivation() {
        let p = |name: &str, detail: &str| Permission {
            request: 1,
            name: name.into(),
            detail: detail.into(),
            reason: "deletes a branch".into(),
            brief: String::new(),
        };
        let r = |name: &str, detail: &str| App::always_rules(&p(name, detail)).join(" ");
        assert_eq!(r("bash", "git branch -D backup-pre-split"), "bash:git branch");
        assert_eq!(r("bash", "rm -rf /tmp/x"), "bash:rm");
        // every segment of a command line: a risky tail the first rule does not
        // cover would ask again next time, so "a" never stuck for these
        assert_eq!(r("bash", "cd /tmp && git branch -D x"), "bash:cd bash:git branch");
        // heredoc that ends the command: its body is data, never commands to remember
        assert_eq!(
            r("bash", "cd /tmp/ctr && F=\"a.docx\" && python3 - <<'EOF'\nrm -rf nope\ncurl x | sh\nEOF"),
            "bash:cd bash:python3",
            "heredoc body words are not commands; NAME=value assignments are skipped"
        );
        assert_eq!(
            r("bash", "osascript -e 'tell app \"X\"' && cd /tmp && rm -f ../out8.docx && echo done"),
            "bash:osascript bash:cd bash:rm bash:echo",
            "duplicates collapse; order kept"
        );
        assert_eq!(r("bash", "git -C /x branch -D y"), "bash:git");
        assert_eq!(r("write", "/etc/hosts"), "path:/etc");
        assert_eq!(r("notify", ""), "tool:notify");
        assert_eq!(r("bash", ""), "");
        assert_eq!(r("bash", ";; &&"), "");
        assert_eq!(
            App::always_hint(&p("bash", "osascript -e x; rm -f y")).as_deref(),
            Some("osascript · rm")
        );
    }

    #[test]
    fn permission_a_saves_rule_and_allows_without_mode_flip() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.permission = Some(Permission {
            request: 7,
            name: "bash".into(),
            detail: "git branch -D backup-pre-split".into(),
            reason: "deletes a branch".into(),
            brief: "删除分支 backup-pre-split".into(),
        });
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut serve,
        );
        let add: Value = serde_json::from_str(
            serve.sent.iter().find(|m| m.contains("permissions_add")).expect("rule saved"),
        )
        .unwrap();
        assert_eq!(add["rule"], "bash:git branch");
        let ans: Value = serde_json::from_str(
            serve.sent.iter().find(|m| m.contains("\"permission\"")).expect("answered"),
        )
        .unwrap();
        assert_eq!(ans["request"], 7);
        assert_eq!(ans["allow"], true);
        assert!(
            !serve.sent.iter().any(|m| m.contains("set_mode")),
            "a must not flip the session to auto mode"
        );
        assert!(app.permission.is_none());
    }

    #[test]
    fn permission_a_on_a_multi_segment_command_saves_one_rule_per_command() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.permission = Some(Permission {
            request: 8,
            name: "bash".into(),
            detail: "osascript -e 'tell application \"Word\"'; cd /tmp/ctr && F=\"a.docx\" && rm -f ../out8.docx".into(),
            reason: "rule covers the prefix, but a later command deletes files: rm -f ../out8.docx".into(),
            brief: String::new(),
        });
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &mut serve,
        );
        let rules: Vec<String> = serve
            .sent
            .iter()
            .filter(|m| m.contains("permissions_add"))
            .map(|m| serde_json::from_str::<Value>(m).unwrap()["rule"].as_str().unwrap().to_string())
            .collect();
        // the first rule alone was what "a" used to save — and the reason this line
        // kept asking: rm was never remembered
        assert_eq!(rules, vec!["bash:osascript", "bash:cd", "bash:rm"]);
        assert!(
            serve.sent.iter().any(|m| m.contains("\"permission\"")),
            "the call is allowed"
        );
        assert!(app.permission.is_none());
    }

    /// two quick press/release pairs on the same spot = a double-click
    fn dblclick(app: &mut App, row: u16, col: u16) {
        app.chat_press(row, col);
        app.chat_release(0, row, col);
        app.chat_press(row, col);
        app.chat_release(0, row, col);
    }

    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn task_progress_updates_the_running_task_line() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(json!({"type":"stream","event":{"type":"tool_call_start","id":"c7","name":"task"}}), &mut serve);
        app.frame(
            json!({"type":"task_progress","callId":"c7","agent":"explore","step":2,"tool":"grep","detail":"英智"}),
            &mut serve,
        );
        let line = app.lines.iter().find(|l| l.tool_id.as_deref() == Some("c7")).unwrap();
        assert_eq!(line.text, "⚙ task explore · 第2步 grep 英智");
        // another call's progress leaves this line alone
        app.frame(json!({"type":"task_progress","callId":"zz","agent":"explore","step":9,"tool":"ls","detail":""}), &mut serve);
        assert!(app.lines.iter().all(|l| !l.text.contains("第9步")));
        // the result freezes it into the usual summary line
        app.frame(
            json!({"type":"tool_result","call":{"id":"c7","name":"task","input":{"agent":"explore","description":"find","prompt":"p"}},"ms":5,"isError":false,"output":"r"}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.starts_with("⚙ task explore · find")));
    }

    #[test]
    fn task_calls_summarize_as_agent_and_description() {
        let s = |v: serde_json::Value| App::call_summary(&v);
        assert_eq!(
            s(serde_json::json!({"agent": "explore", "description": "find auth", "prompt": "long brief"})),
            "explore · find auth"
        );
        // agent defaults to explore; background is marked
        assert_eq!(
            s(serde_json::json!({"description": "scan", "prompt": "p", "background": true})),
            "explore · scan · 后台"
        );
        // other tools keep their usual summary
        assert_eq!(s(serde_json::json!({"path": "a.ts"})), "a.ts");
    }

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
        let mut serve = Serve::mock();
        app.frame(
            json!({
                "type": "hello", "sessionId": "12345678", "model": "mock/mock",
                "mode": "ask", "theme": "light", "tools": []
            }),
            &mut serve,
        );
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
        assert_eq!(app.lines.len(), 1, "one prompt, one bubble");
        assert_eq!(app.lines[0].text, "❯ hello\nworld");
        assert_eq!(app.lines[0].output, None);
    }

    #[test]
    fn user_prompt_newlines_become_real_rows_in_one_bubble() {
        let texts = vec!["❯ hello\nworld\n\nend".to_string()];
        let mut rows = wrap_chat_rows_mixed(&texts, 40, &[true], &[], &[], true);
        layout_user_bubbles(&mut rows, &[true], 40, true);
        let body: Vec<&str> = rows
            .iter()
            .filter(|r| !r.border)
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(body, vec!["❯ hello", "world", "", "end"]);
        assert_eq!(
            rows.iter().filter(|r| r.border).count(),
            2,
            "a single frame"
        );
        // char offsets still index the whole text, skipping each '\n'
        let spans: Vec<(usize, usize)> = rows
            .iter()
            .filter(|r| !r.border)
            .map(|r| (r.cs, r.ce))
            .collect();
        assert_eq!(spans, vec![(0, 7), (8, 13), (14, 14), (15, 18)]);
    }

    #[test]
    fn prompt_marker_stays_on_the_first_row_of_a_long_cjk_run() {
        let texts = vec!["❯ 这是一个很长很长的中文句子没有空格一直写下去".to_string()];
        let rows = wrap_chat_rows(&texts, 20);
        assert!(rows[0].text.starts_with("❯ 这是"), "{:?}", rows[0].text);
        assert!(rows.iter().all(|r| display_width(&r.text) <= 20));
        // short words still wrap at spaces
        let rows = wrap_chat_rows(&["❯ hello wonderful world".to_string()], 12);
        let got: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(got, vec!["❯ hello ", "wonderful ", "world"]);
    }

    #[test]
    fn latest_full_text_opens_in_viewer_without_moving_chat() {
        let mut app = App::new();
        app.push_user(&"x".repeat(1000));
        app.nvim = Some(None); // internal-viewer behavior does not depend on the host's editors
        app.scroll_back = 7;
        app.open_latest_viewer();
        assert_eq!(app.scroll_back, 7);
        assert_eq!(app.viewer.as_ref().unwrap().text, "x".repeat(1000));
        assert_eq!(app.viewer.as_ref().unwrap().title, "粘贴上下文");
    }

    #[test]
    fn detected_nvim_prompts_before_the_first_external_view() {
        let mut app = App::new();
        app.push_user(&"x".repeat(1000));
        app.nvim = Some(Some(Neovim {
            executable: "nvim".into(),
        }));
        // a chat line's output (ctrl+o / double-click) is a quick look: the built-in
        // dialog even with nvim around and nvimViewer=readonly — no picker, no nvim
        app.nvim_preference = "readonly".into();
        app.open_latest_viewer();
        assert!(app.picker.is_none() && app.pending_viewer.is_none());
        assert_eq!(app.viewer.as_ref().unwrap().text, "x".repeat(1000));
        app.viewer = None;
        // other views (session preview …) still ask once
        app.nvim_preference = "ask".into();
        app.request_viewer(Viewer::read_only("会话预览", "p"));
        assert_eq!(
            app.picker.as_ref().map(|p| p.kind),
            Some(PickerKind::NvimBackend)
        );
        assert!(app.pending_viewer.is_some());
        assert!(app.viewer.is_none());
    }

    #[test]
    fn edited_content_requires_an_explicit_resolution() {
        let mut app = App::new();
        let viewer = Viewer::with_source("粘贴上下文", "old", 0, Some(PromptState::Completed));
        app.finish_nvim(viewer, "new".into(), false);
        assert_eq!(
            app.picker.as_ref().map(|p| p.kind),
            Some(PickerKind::EditResolution)
        );
        assert!(app.edited.is_some());
        assert!(app.lines.iter().any(|l| l.text.contains("已经处理完成")));
    }

    #[test]
    fn read_only_neovim_exit_returns_to_the_prompt() {
        let mut app = App::new();
        app.input = "draft".chars().collect();
        app.cursor = app.input.len();
        let viewer = Viewer::with_source("粘贴上下文", "old", 0, Some(PromptState::Completed));
        app.finish_nvim(viewer, "old".into(), true);
        assert!(app.viewer.is_none());
        assert!(app.picker.is_none());
        assert_eq!(app.input.iter().collect::<String>(), "draft");
        assert_eq!(app.cursor, 5);
    }

    #[test]
    fn unchanged_neovim_edit_returns_to_the_prompt() {
        let mut app = App::new();
        let viewer = Viewer::with_source("粘贴上下文", "old", 0, Some(PromptState::Completed));
        app.finish_nvim(viewer, "old".into(), false);
        assert!(app.viewer.is_none());
        assert!(app.picker.is_none());
    }

    #[test]
    fn multiline_paste_is_a_placeholder_followed_by_visible_typing() {
        let mut app = App::new();
        app.insert_paste("one\ntwo\nthree");
        app.insert_char('x');
        let rendered = app.rendered_input();
        assert_eq!(rendered.text, "[Pasted text #1 +2 lines]\nx");
        assert_eq!(rendered.cursor, rendered.text.chars().count());
        assert_eq!(app.input.iter().collect::<String>(), "one\ntwo\nthree\nx");
        assert!(app.input_height(80) >= 3);
    }

    #[test]
    fn click_row_maps_to_the_visible_expandable_card() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        app.nvim_preference = "internal".into();
        app.push_tool("first", Tone::Muted, Some("one".into()));
        app.push_tool("second", Tone::Muted, Some("two".into()));
        app.push_tool("third", Tone::Muted, Some("three".into()));
        {
            let mut terminal = Terminal::new(TestBackend::new(80, 19)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        // short lines: one display row each, top-anchored at chat y=0
        app.chat_press(1, 3);
        app.chat_release(0, 1, 3);
        assert!(app.viewer.is_none(), "a single click opens nothing");
        dblclick(&mut app, 1, 3);
        assert_eq!(app.viewer.as_ref().unwrap().text, "two");
        // a row past the last line (empty space) opens nothing new
        app.viewer = None;
        app.chat_last_click = None;
        let bottom = app.chat_area.height.saturating_sub(1) as u16;
        dblclick(&mut app, bottom, 3);
        assert!(app.viewer.is_none(), "clicking empty space opens nothing");
    }

    #[test]
    fn click_on_plain_text_opens_no_viewer_despite_bubble_drift() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        app.nvim_preference = "internal".into();
        app.push_user("一句比较长的用户输入，占据多行气泡，为下面的行制造偏移");
        app.push("当前模型未标注视觉能力，图片仍会随消息发送", Tone::Muted);
        app.push_tool("⚙ bash", Tone::Muted, Some("popup.png: PNG image data".into()));
        {
            let mut terminal = Terminal::new(TestBackend::new(80, 19)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        // the notice line: a plain line with no output. The old code walked a naive
        // wrap that ignored the bubble's border rows, resolved several rows too far
        // down, and opened the bash card's viewer instead.
        let notice_row = app
            .chat_rows
            .iter()
            .position(|r| {
                app.chat_lines()
                    .get(r.line)
                    .is_some_and(|l| l.text.starts_with("当前模型") && l.output.is_none())
            })
            .unwrap() as u16;
        app.chat_press(notice_row, 5);
        app.chat_release(0, notice_row, 5);
        assert!(app.viewer.is_none(), "plain notice must not open a viewer");
        dblclick(&mut app, notice_row, 5);
        assert!(app.viewer.is_none(), "double-clicking a notice still opens nothing");
        // the tool line still opens exactly its own output
        let tool_row = app
            .chat_rows
            .iter()
            .position(|r| {
                app.chat_lines()
                    .get(r.line)
                    .is_some_and(|l| l.text.starts_with("⚙ bash"))
            })
            .unwrap() as u16;
        app.chat_last_click = None;
        dblclick(&mut app, tool_row, 5);
        assert_eq!(app.viewer.as_ref().unwrap().text, "popup.png: PNG image data");
    }

    #[test]
    fn hello_configures_custom_keys_and_vim() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(
            json!({
                "type": "hello", "sessionId": "12345678", "model": "mock/mock",
                "mode": "ask", "editor": "vim", "keys": { "expandTools": "ctrl+u" }, "tools": []
            }),
            &mut serve,
        );
        assert!(app.keys.matches(
            Action::ExpandTools,
            crossterm::event::KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)
        ));
        assert!(app.vim);
        assert_eq!(app.edit_mode, EditMode::Insert);
        assert!(
            app.lines
                .iter()
                .any(|line| line.text.contains("Vim 已开启"))
        );
    }

    #[test]
    fn vim_status_bar_names_the_active_mode() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut app = App::new();
        app.vim = true;
        for (edit_mode, command, expected) in [
            (EditMode::Insert, None, "VIM INSERT"),
            (EditMode::Normal, None, "VIM NORMAL"),
            (EditMode::Normal, Some("wq"), "VIM COMMAND"),
        ] {
            app.edit_mode = edit_mode;
            app.vim_command = command.map(str::to_string);
            let mut terminal = Terminal::new(TestBackend::new(60, 10)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let buffer = terminal.backend().buffer();
            let row = |y| (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>();
            assert!(
                row(9).contains(expected),
                "missing {expected:?} in bottom row"
            );
            assert!(!row(0).contains("VIM"));
        }
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
    fn vim_j_k_move_the_input_cursor_without_scrolling_chat() {
        let mut app = App::new();
        app.vim = true;
        app.edit_mode = EditMode::Normal;
        app.input = "first\nsecond\nthird".chars().collect();
        app.cursor = 9; // second line, display column 3
        app.scroll_back = 4;

        assert!(app.normal_edit(KeyCode::Char('k')));
        assert_eq!(app.cursor, 3);
        assert!(app.normal_edit(KeyCode::Char('j')));
        assert_eq!(app.cursor, 9);
        assert!(app.normal_edit(KeyCode::Char('j')));
        assert_eq!(app.cursor, 16);
        assert!(app.normal_edit(KeyCode::Char('j')));
        assert_eq!(app.cursor, 16); // already on the last line
        assert!(app.normal_edit(KeyCode::Char('k')));
        assert_eq!(app.cursor, 9);
        assert_eq!(app.scroll_back, 4);

        app.input = "longline\nx\nend".chars().collect();
        app.cursor = 6;
        assert!(app.normal_edit(KeyCode::Char('j')));
        assert_eq!(app.cursor, 10); // short target line clamps to its end
    }

    #[test]
    fn hello_restores_session_cost() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(
            json!({
                "type": "hello",
                "sessionId": "12345678-abcd",
                "model": "mock/mock",
                "mode": "ask",
                "totalCostUsd": 1.25,
                "tools": []
            }),
            &mut serve,
        );
        assert_eq!(app.cost, 1.25);
    }

    #[test]
    fn multiline_input_cursor_matches_the_actual_wrapped_rows() {
        use ratatui::{
            Terminal,
            backend::{Backend, TestBackend},
        };

        let mut app = App::new();
        app.input = "one two three four five".chars().collect();
        app.cursor = app.input.len();
        let mut terminal = Terminal::new(TestBackend::new(14, 10)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();

        // rule + 2 text rows + info row; no pad/footer on a 10-row terminal
        assert_eq!(app.input_height(14), 4);
        assert_eq!(app.input_area.y, 5);
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(INPUT_X + 11, 7)
        );
        let buffer = terminal.backend().buffer();
        let row = |y| (0..14).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert_eq!(row(6), "▌ one two thre");
        assert_eq!(row(7), "▌ e four five ");
    }

    #[test]
    fn input_layout_handles_wide_chars_full_rows_and_left_movement() {
        let mut app = App::new();
        app.input = "ab中cd".chars().collect();
        app.cursor = app.input.len();
        let layout = app.input_layout(5);
        assert_eq!(layout.lines, vec!["ab中c", "d"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 1));
        app.cursor = 2;
        let layout = app.input_layout(5);
        assert_eq!((layout.cursor_row, layout.cursor_col), (0, 2));

        app.input = "1234".chars().collect();
        app.cursor = 4;
        let layout = app.input_layout(4);
        assert_eq!(layout.lines, vec!["1234", ""]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 0));
    }

    #[test]
    fn long_input_scrolls_the_hardware_cursor_into_view() {
        use ratatui::{
            Terminal,
            backend::{Backend, TestBackend},
        };

        let mut app = App::new();
        app.mode = "ask".into();
        app.model = "fan/opus".into();
        app.input = (1..=10)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .collect();
        app.cursor = app.input.len();
        let mut terminal = Terminal::new(TestBackend::new(20, 14)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(INPUT_X + 7, 11)
        );
        let buffer = terminal.backend().buffer();
        let row = |y| (0..20).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert!(row(4).starts_with("▌ line-3"));
        assert!(row(11).starts_with("▌ line-10"));
        assert!(row(12).starts_with("▌ ask · opus fan"), "info row: {}", row(12));
    }

    #[test]
    fn collapsed_paste_and_later_typing_share_the_same_layout() {
        let mut app = App::new();
        app.insert_paste("one\ntwo\nthree");
        app.insert_char('x');
        let layout = app.input_layout(20);
        assert_eq!(layout.lines.len(), 3);
        assert_eq!(layout.lines[2], "x");
        assert_eq!((layout.cursor_row, layout.cursor_col), (2, 1));
    }

    #[test]
    fn vim_insert_enter_adds_a_newline_without_submitting() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.vim = true;
        app.edit_mode = EditMode::Insert;
        app.input = "first".chars().collect();
        app.cursor = app.input.len();
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        assert!(serve.sent.is_empty());
        assert_eq!(app.input.iter().collect::<String>(), "first\n");
    }

    #[test]
    fn ctrl_s_submits_the_complete_draft_in_both_vim_modes() {
        for edit_mode in [EditMode::Insert, EditMode::Normal] {
            let mut app = App::new();
            let mut serve = Serve::mock();
            app.vim = true;
            app.edit_mode = edit_mode;
            app.input = "one\ntwo".chars().collect();
            app.cursor = app.input.len();
            app.key(
                crossterm::event::KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
                &mut serve,
            );
            assert_eq!(serve.sent.len(), 1);
            let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
            assert_eq!(sent["cmd"], "prompt");
            assert_eq!(sent["text"], "one\ntwo");
        }
    }

    #[test]
    fn vim_write_quit_commands_show_the_command_and_preserve_the_draft() {
        for command in ["wq", "wqa"] {
            let mut app = App::new();
            let mut serve = Serve::mock();
            app.vim = true;
            app.edit_mode = EditMode::Normal;
            app.input = "one\ntwo".chars().collect();
            app.cursor = app.input.len();
            let draft_height = app.input_height(80);
            for c in format!(":{command}").chars() {
                app.key(
                    crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                    &mut serve,
                );
            }
            assert!(serve.sent.is_empty());
            let layout = app.input_layout(80);
            assert_eq!(layout.lines, vec!["one", "two", &format!(":{command}")]);
            assert_eq!(
                (layout.cursor_row, layout.cursor_col),
                (2, command.len() + 1)
            );
            assert_eq!(app.input_height(80), draft_height + 1);
            app.key(
                crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut serve,
            );
            assert_eq!(serve.sent.len(), 1);
            let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
            assert_eq!(sent["text"], "one\ntwo");
        }
    }

    #[test]
    fn escaping_a_vim_command_does_not_clear_the_draft() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.vim = true;
        app.edit_mode = EditMode::Normal;
        app.input = "draft".chars().collect();
        app.cursor = app.input.len();
        for c in ":w".chars() {
            app.key(
                crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut serve,
            );
        }
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut serve,
        );
        assert_eq!(app.input.iter().collect::<String>(), "draft");
        assert_eq!(app.input_layout(80).lines, vec!["draft"]);
        assert!(serve.sent.is_empty());
    }

    #[test]
    fn vim_slash_command_toggles_modal_editing() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/vim", &mut serve);
        assert!(app.vim);
        assert_eq!(app.edit_mode, EditMode::Insert);
        assert!(app.lines.last().unwrap().text.contains("Vim 已开启"));
        assert!(app.lines.last().unwrap().text.contains("Ctrl+S 发送"));
        app.edit_mode = EditMode::Normal;
        app.run_slash("/vim", &mut serve);
        assert!(!app.vim);
        assert_eq!(app.edit_mode, EditMode::Insert);
        assert!(app.lines.last().unwrap().text.contains("Vim 已关闭"));
        assert!(app.lines.last().unwrap().text.contains("Enter 发送"));
        assert!(!app.lines.last().unwrap().text.contains("Enter 换行"));
    }

    #[test]
    fn ctrl_e_opens_the_current_draft_in_neovim_without_submitting() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.nvim = Some(Some(Neovim {
            executable: "nvim".into(),
        }));
        app.input = "one\ntwo".chars().collect();
        app.cursor = app.input.len();

        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &mut serve,
        );

        let request = app.nvim_request.as_ref().unwrap();
        assert_eq!(request.target, NvimTarget::Draft);
        assert_eq!(request.viewer.text, "one\ntwo");
        assert!(!request.read_only);
        assert!(serve.sent.is_empty());
    }

    #[test]
    fn external_draft_edit_returns_to_input_without_a_resolution_or_submission() {
        let mut app = App::new();
        let serve = Serve::mock();
        app.input = "old".chars().collect();
        app.cursor = app.input.len();

        app.finish_draft_nvim("old", "new\ncomplete");

        assert_eq!(app.input.iter().collect::<String>(), "new\ncomplete");
        assert_eq!(app.cursor, app.input.len());
        assert!(app.viewer.is_none());
        assert!(app.picker.is_none());
        assert!(app.edited.is_none());
        assert!(serve.sent.is_empty());
    }

    #[test]
    fn nvim_command_starts_an_empty_external_draft() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.nvim = Some(Some(Neovim {
            executable: "nvim".into(),
        }));

        app.run_slash("/nvim", &mut serve);

        let request = app.nvim_request.as_ref().unwrap();
        assert_eq!(request.target, NvimTarget::Draft);
        assert_eq!(request.viewer.text, "");
        assert!(serve.sent.is_empty());
    }

    #[test]
    fn missing_neovim_preserves_the_current_draft() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.nvim = Some(None);
        app.input = "keep me".chars().collect();
        app.cursor = app.input.len();

        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &mut serve,
        );

        assert!(app.nvim_request.is_none());
        assert_eq!(app.input.iter().collect::<String>(), "keep me");
        assert!(app.lines.last().unwrap().text.contains("未检测到 Neovim"));
        assert!(serve.sent.is_empty());
    }

    #[test]
    fn command_palette_filters_and_runs_the_selected_command() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &mut serve,
        );
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.kind, PickerKind::Command);
        assert!(picker.items.iter().any(|item| item.id == "/model"));
        assert!(picker.items.iter().any(|item| item.id == "/resume"));

        for c in "resu".chars() {
            app.key(
                crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut serve,
            );
        }
        let picker = app.picker.as_ref().unwrap();
        let visible = picker.filtered_items();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "/resume");

        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        assert!(app.picker.is_none());
        assert_eq!(serve.sent.len(), 1);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "sessions");
    }

    #[test]
    fn prompt_history_search_restores_without_submitting() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        for text in ["first", "second", "second"] {
            app.busy = false;
            app.input = text.chars().collect();
            app.cursor = app.input.len();
            app.send_prompt(&mut serve);
        }
        assert_eq!(app.prompt_history, vec!["first", "second"]);
        let sent_before = serve.sent.len();

        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            &mut serve,
        );
        assert_eq!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::History)
        );
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE),
            &mut serve,
        );
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );

        assert_eq!(app.input.iter().collect::<String>(), "first");
        assert_eq!(app.cursor, 5);
        assert!(app.picker.is_none());
        assert_eq!(serve.sent.len(), sent_before);
    }

    #[test]
    fn draft_stash_restores_collapsed_pastes_and_cursor() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.insert_paste("one\ntwo\nthree");
        app.insert_char('x');
        let original = app.input.clone();
        let original_cursor = app.cursor;
        let original_rendered = app.rendered_input().text;

        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT),
            &mut serve,
        );
        assert!(app.input.is_empty());
        assert!(app.pastes.is_empty());
        assert!(app.stashed_draft.is_some());

        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT),
            &mut serve,
        );
        assert_eq!(app.input, original);
        assert_eq!(app.cursor, original_cursor);
        assert_eq!(app.rendered_input().text, original_rendered);
        assert!(app.stashed_draft.is_none());
        assert!(serve.sent.is_empty());
    }

    #[test]
    fn at_file_line_range_inlines_only_the_selected_lines() {
        let path = crate::nvim::write_temp("one\ntwo\nthree\nfour").unwrap();
        let path = path.to_string_lossy().to_string();
        for (suffix, expected) in [
            ("#2-3", "two\nthree"),
            ("#L2-L3", "two\nthree"),
            ("#4", "four"),
        ] {
            let token = format!("@{path}{suffix}");
            let (expanded, warns) = App::expand_at(&format!("look {token}"), None);
            assert_eq!(expanded, format!("look {path}{suffix}:\n{expected}"));
            assert!(warns.is_empty());
        }
        let _ = fs::remove_file(path);
    }

    #[test]
    fn invalid_or_out_of_bounds_file_ranges_are_not_silently_expanded() {
        let path = crate::nvim::write_temp("one\ntwo").unwrap();
        let path = path.to_string_lossy().to_string();
        for suffix in ["#0", "#2-1", "#bad"] {
            let text = format!("keep @{path}{suffix}");
            let (expanded, warns) = App::expand_at(&text, None);
            assert_eq!(expanded, text);
            assert!(warns.is_empty());
        }
        let text = format!("keep @{path}#9");
        let (expanded, warns) = App::expand_at(&text, None);
        assert_eq!(expanded, text);
        assert_eq!(warns.len(), 1);
        let dir = format!("@{}#1-2", std::env::temp_dir().display());
        assert_eq!(App::expand_at(&dir, None).0, dir);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn unread_jump_bottom_preserves_the_reading_position() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut app = App::new();
        let mut serve = Serve::mock();
        for n in 0..20 {
            app.push(format!("line-{n:02}"), Tone::Text);
        }
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        app.scroll_back = 5;
        terminal.draw(|f| app.draw(f)).unwrap();
        let row = |terminal: &Terminal<TestBackend>, y| {
            let buffer = terminal.backend().buffer();
            (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>()
        };
        // scrolling up shows the pill row, shrinking the chat by one row
        // (the input block's info row costs one more than the old bare input)
        assert!(
            row(&terminal, 0).starts_with("line-08"),
            "unexpected first row: {:?}",
            row(&terminal, 0)
        );

        app.push("line-20", Tone::Text);
        app.push("line-21", Tone::Text);
        app.push("line-22", Tone::Text);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(row(&terminal, 0).starts_with("line-08"));
        // fresh backend: TestBackend's cell diff mangles wide chars when a label grows in place
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen = (0..12)
            .map(|y| (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.replace(' ', "").contains("3行新内容"),
            "screen:\n{screen}"
        );
        // the pill has its own row above the input, centered, and covers no chat text
        assert_eq!(app.jump_area.y, app.input_area.y - 1);
        assert!(row(&terminal, app.jump_area.y - 1).starts_with("line-"));

        app.key(
            crossterm::event::KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL),
            &mut serve,
        );
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen = (0..12)
            .map(|y| (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("line-22"));
        assert!(!screen.replace(' ', "").contains("行新内容"));
    }

    #[test]
    fn ctrl_t_opens_a_searchable_transcript_without_moving_chat() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.push_user("first prompt");
        app.push("answer", Tone::Text);
        app.push_user("second prompt");
        app.scroll_back = 7;

        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &mut serve,
        );

        let viewer = app.viewer.as_ref().unwrap();
        assert!(viewer.transcript);
        assert!(viewer.text.contains("first prompt"));
        assert!(viewer.text.contains("second prompt"));
        assert_eq!(viewer.prompt_rows, vec![0, 2]);
        assert_eq!(app.scroll_back, 7);
        assert!(serve.sent.is_empty());
    }

    #[test]
    fn focus_and_tool_errors_keep_failures_auditable() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(
            json!({
                "type": "tool_result", "call": {"name": "bash", "input": {"command": "fail"}},
                "isError": true, "ms": 3, "output": "first failure\nsecond failure"
            }),
            &mut serve,
        );
        app.frame(
            json!({
                "type": "tool_result", "call": {"name": "bash", "input": {"command": "ok"}},
                "isError": false, "ms": 1, "output": "successful output"
            }),
            &mut serve,
        );
        // a failure is one warning-colored line with a hint; the output waits in the viewer
        let rendered = app.chat_lines();
        let fail = rendered.iter().find(|l| l.text.contains("⚙ bash fail")).unwrap();
        assert_eq!(fail.tone, Tone::Warning);
        assert!(fail.text.ends_with("· 出错 · 双击查看"), "{}", fail.text);
        assert!(!fail.text.contains('\n'));
        assert!(!rendered.iter().any(|l| l.text.contains("first failure")));
        assert_eq!(
            app.lines[0].output.as_deref(),
            Some("first failure\nsecond failure")
        );

        app.thinking = "private reasoning".into();
        app.commit_stream();
        app.run_slash("/focus", &mut serve);
        let focused = app.chat_lines();
        assert!(!focused.iter().any(|line| line.text.contains("⚙ bash ok")));
        assert!(
            !focused
                .iter()
                .any(|line| line.text.contains("private reasoning"))
        );
        assert!(focused.iter().any(|line| line.text.contains("⚙ bash fail")));

        app.run_slash("/focus", &mut serve);
        assert!(
            app.chat_lines()
                .iter()
                .any(|line| line.text.contains("⚙ bash ok"))
        );
    }

    #[test]
    fn context_usage_appears_in_the_status_with_thresholds() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(
            json!({
                "type": "hello", "sessionId": "12345678", "model": "mock/mock", "mode": "ask",
                "contextWindow": 200, "contextTokens": 40, "tools": []
            }),
            &mut serve,
        );
        assert_eq!(app.context_tone(), Tone::Success);
        app.frame(
            json!({
                "type": "turn_end", "usage": {"input": 160, "output": 2},
                "contextWindow": 200, "contextTokens": 160
            }),
            &mut serve,
        );
        assert_eq!(app.context_tone(), Tone::Error);
        let mut terminal = Terminal::new(TestBackend::new(100, 8)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let status = (0..100)
            .map(|x| buffer[(x, 7)].symbol())
            .collect::<String>();
        assert!(status.contains("ctx 160/200 80%"));
    }

    #[test]
    fn full_access_dialog_renders_the_checkbox_and_button_on_one_row() {
        use ratatui::{Terminal, backend::TestBackend};
        for width in [39, 64, 100] {
            let mut app = App::new();
            app.full_access = Some(false);
            let mut term = Terminal::new(TestBackend::new(width, 30)).unwrap();
            term.draw(|f| app.draw(f)).unwrap();
            let y = app.full_access_buttons[0].0.y;
            let row: String = (0..width).map(|x| term.backend().buffer().content[y as usize * width as usize + x as usize].symbol()).collect();
            // a wide glyph's second cell renders as a space: "启 用", "☑ "
            let compact: String = row.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(compact.contains("☐") && compact.contains("启用"), "{width}: {row:?}");
            let toggle = app.full_access_buttons.iter().find(|(_, a)| *a == FullAccessAction::Toggle).unwrap().0;
            let enable = app.full_access_buttons.iter().find(|(_, a)| *a == FullAccessAction::Enable).unwrap().0;
            assert!(toggle.right() <= enable.x, "{width}: no overlap {:?} {:?}", toggle, enable);
            // the whole text fits on a normal terminal; a narrow one is cut, never wrapped over the button
            if width == 64 {
                assert!(compact.contains("并对自己的数据安全负责"), "{row:?}");
            }
        }
    }

    #[test]
    fn auto_is_short_for_mode_auto_and_the_ack_dialog_is_clickable() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.mode = "ask".into();
        // /auto (and /yolo) open the same acknowledgement as /mode auto; nothing is sent yet
        for cmd in ["/auto", "/mode auto"] {
            app.run_slash(cmd, &mut serve);
            assert_eq!(app.full_access, Some(false), "{cmd}");
            app.key(KeyCode::Esc.into(), &mut serve);
        }
        assert!(serve.sent.is_empty(), "no set_mode before the acknowledgement");
        // the checkbox row and the 启用 button are drawn as click targets
        app.run_slash("/auto", &mut serve);
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let target = |app: &App, a: FullAccessAction| app.full_access_buttons.iter().find(|(_, x)| *x == a).map(|(r, _)| *r).unwrap();
        let click = |app: &mut App, serve: &mut Serve, r: Rect| {
            app.handle(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: r.x + 2,
                    row: r.y,
                    modifiers: KeyModifiers::NONE,
                }),
                serve,
            );
        };
        let toggle = target(&app, FullAccessAction::Toggle);
        let enable = target(&app, FullAccessAction::Enable);
        assert!(toggle.y == enable.y && toggle.x < enable.x, "same row, 启用 on the right");
        // 启用 while unchecked is refused, like Enter (the acknowledgement is not one click)
        click(&mut app, &mut serve, enable);
        assert_eq!(app.full_access, Some(false));
        assert!(serve.sent.is_empty());
        // clicking the checkbox row ticks it; clicking 启用 enables
        click(&mut app, &mut serve, toggle);
        assert_eq!(app.full_access, Some(true), "clicking the row ticks the box");
        term.draw(|f| app.draw(f)).unwrap();
        let enable = target(&app, FullAccessAction::Enable);
        click(&mut app, &mut serve, enable);
        assert!(app.full_access.is_none());
        let sent: String = serve.sent.concat();
        assert!(sent.contains("\"set_mode\"") && sent.contains("\"auto\""), "{sent}");
        // already in auto: /auto says so instead of opening the dialog again
        app.mode = "auto".into();
        app.run_slash("/auto", &mut serve);
        assert!(app.full_access.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("已经是完全放行")));
    }

    #[test]
    fn commit_push_and_commit_pr_send_their_preset_prompt() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/commit-push", &mut serve);
        assert!(app.busy, "a turn starts, like a typed prompt");
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "prompt");
        let text = sent["text"].as_str().unwrap();
        assert!(text.starts_with("(/commit-push) Commit the current changes and push this branch."));
        // the rules that make it safe to run on a shared checkout
        for rule in ["git log --oneline", "git add <paths>", "never `git add -A`", "Never force-push", "Stay on the current branch", "中文 history"] {
            assert!(text.contains(rule), "missing: {rule}");
        }
        assert!(!text.contains("gh pr create"), "no pull request from /commit-push");
        // shown as the user's line (collapsed: it is long), like any preset prompt
        assert!(app.lines.iter().any(|l| l.tone == Tone::User && l.text.contains("粘贴")));
        assert!(app.current_prompt.as_deref().is_some_and(|p| p.starts_with("(/commit-push) Commit the current changes")));

        app.busy = false;
        app.run_slash("/commit-pr", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        let text = sent["text"].as_str().unwrap();
        assert!(text.starts_with("(/commit-pr) Commit the current changes and open a pull request"));
        for rule in ["gh pr create", "default branch", "PULL_REQUEST_TEMPLATE", "Never force-push", "install nothing", "Never merge"] {
            assert!(text.contains(rule), "missing: {rule}");
        }
        // extra words are not an error: the command ignores them
        app.busy = false;
        app.run_slash("/commit-push now", &mut serve);
        assert_eq!(serve.sent.len(), 3);
    }

    #[test]
    fn mode_picker_lists_all_modes_and_full_access_needs_the_risk_ack() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.mode = "ask".into();
        app.run_slash("/mode", &mut serve);
        let picker = app.picker.as_ref().expect("bare /mode opens the picker");
        assert_eq!(picker.kind, PickerKind::Mode);
        let ids: Vec<&str> = picker.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["plan", "confirm", "ask", "auto", "readonly"]);
        assert!(picker.items.iter().find(|i| i.id == "ask").unwrap().current, "ask is current");
        assert!(!picker.items.iter().find(|i| i.id == "auto").unwrap().current);

        // picking a normal mode sends set_mode and drops the picker
        fn pick(app: &mut App, serve: &mut Serve, id: &str) {
            let i = app.picker.as_ref().unwrap().items.iter().position(|x| x.id == id).unwrap();
            app.picker.as_mut().unwrap().sel = i;
            app.accept_picker(serve);
        }
        pick(&mut app, &mut serve, "confirm");
        assert!(app.picker.is_none());
        let sent: String = serve.sent.concat();
        assert!(sent.contains("\"set_mode\"") && sent.contains("\"confirm\""), "{sent}");

        // picking full access opens the acknowledgement, nothing sent yet
        serve.sent.clear();
        app.run_slash("/mode", &mut serve);
        pick(&mut app, &mut serve, "auto");
        assert!(app.full_access == Some(false), "dialog open, unchecked");
        assert!(!serve.sent.concat().contains("set_mode"), "nothing sent yet");

        // Enter without the checkbox is refused
        app.key(KeyCode::Enter.into(), &mut serve);
        assert!(app.full_access.is_some());
        assert!(!serve.sent.concat().contains("set_mode"));

        // space checks the box, Enter enables
        app.key(KeyCode::Char(' ').into(), &mut serve);
        assert_eq!(app.full_access, Some(true));
        app.key(KeyCode::Enter.into(), &mut serve);
        assert!(app.full_access.is_none());
        let sent: String = serve.sent.concat();
        assert!(sent.contains("\"set_mode\"") && sent.contains("\"auto\""), "{sent}");

        // typed /mode auto goes through the same acknowledgement
        serve.sent.clear();
        app.run_slash("/mode auto", &mut serve);
        assert!(app.full_access == Some(false));
        assert!(!serve.sent.concat().contains("set_mode"));
        app.key(KeyCode::Esc.into(), &mut serve);
        assert!(app.full_access.is_none(), "esc cancels");

        // picking plan turns plan on
        app.run_slash("/mode", &mut serve);
        pick(&mut app, &mut serve, "plan");
        assert!(serve.sent.concat().contains("set_plan"));
    }

    #[test]
    fn model_picker_filter_is_fuzzy_across_words() {
        let items = vec![
            PickItem { group: "智谱".into(), id: "zhipu-plan/glm-5.4".into(), label: "zhipu-plan/glm-5.4".into(), note: String::new(), warn: false, current: true },
            PickItem { group: "智谱".into(), id: "zhipu-plan/glm-5.3".into(), label: "zhipu-plan/glm-5.3".into(), note: String::new(), warn: false, current: false },
            PickItem { group: "apikeyfan".into(), id: "apikeyfan/claude-opus-5-5".into(), label: "apikeyfan/claude-opus-5-5".into(), note: String::new(), warn: false, current: false },
        ];
        let mut picker = Picker::new(PickerKind::Model, items);
        let ids = |p: &Picker| p.filtered_items().iter().map(|i| i.id.clone()).collect::<Vec<_>>();
        // subsequence: separators may be skipped
        picker.query = "glm54".into();
        assert_eq!(ids(&picker), vec!["zhipu-plan/glm-5.4"]);
        // plain substring still works, case-insensitive
        picker.query = "GLM-5.3".into();
        assert_eq!(ids(&picker), vec!["zhipu-plan/glm-5.3"]);
        // every word must match (id, label or note)
        picker.query = "afn opus".into();
        assert_eq!(ids(&picker), vec!["apikeyfan/claude-opus-5-5"]);
        picker.query = "afn glm".into();
        assert_eq!(ids(&picker), Vec::<String>::new());
        // notes are searched too
        picker.query = String::new();
        picker.items[1].note = "订阅套餐 Subscription".into();
        picker.query = "subscri".into();
        assert_eq!(ids(&picker), vec!["zhipu-plan/glm-5.3"]);
    }

    #[test]
    fn theme_picker_applies_the_saved_theme_immediately() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/theme", &mut serve);
        assert_eq!(app.picker.as_ref().map(|p| p.kind), Some(PickerKind::Theme));
        app.picker.as_mut().unwrap().sel = 1;
        app.accept_picker(&mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "set_theme");
        assert_eq!(sent["theme"], "light");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true, "data": {"theme": "light"}}),
            &mut serve,
        );
        assert!(app.theme.light);
    }

    #[test]
    fn mouse_command_emits_a_terminal_capture_request() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/mouse off", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "set_mouse");
        assert_eq!(sent["mouse"], false);
        assert_eq!(app.mouse_capture, Some(false));
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true, "data": {"mouse": false}}),
            &mut serve,
        );
        assert!(!app.mouse);
    }

    #[test]
    fn diff_command_opens_a_read_only_patch_viewer() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.scroll_back = 4;
        app.run_slash("/diff", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "diff");
        assert_eq!(sent["staged"], false);
        app.frame(
            json!({
                "type": "result", "id": sent["id"], "ok": true,
                "data": {"diff": "@@ -1 +1,2 @@\n old\n+new", "staged": false}
            }),
            &mut serve,
        );
        let viewer = app.viewer.as_ref().unwrap();
        assert!(viewer.read_only);
        assert!(viewer.text.contains("+new"));
        assert_eq!(app.scroll_back, 4);
    }

    #[test]
    fn btw_streams_only_into_a_separate_read_only_dialog() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/btw why did it fail?", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "btw");
        assert_eq!(sent["text"], "why did it fail?");
        assert!(app.viewer.as_ref().unwrap().read_only);
        assert!(app.lines.is_empty());

        app.frame(
            json!({
                "type": "btw_stream", "id": sent["id"],
                "event": {"type": "text", "delta": "side answer"}
            }),
            &mut serve,
        );
        assert_eq!(app.viewer.as_ref().unwrap().text, "side answer");
        assert!(app.lines.is_empty());
        app.frame(
            json!({
                "type": "result", "id": sent["id"], "ok": true,
                "data": {"text": "complete side answer"}
            }),
            &mut serve,
        );
        assert_eq!(app.viewer.as_ref().unwrap().text, "complete side answer");
        assert!(app.lines.is_empty());
    }

    #[test]
    fn ctrl_c_interrupts_while_a_vim_command_line_is_active() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.vim = true;
        app.edit_mode = EditMode::Normal;
        app.busy = true;
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
            &mut serve,
        );
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut serve,
        );
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "interrupt");
    }

    #[test]
    fn ctrl_c_on_an_empty_idle_input_asks_once_then_quits() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        let ctrl_c = crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let key = |c| crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        // with text, Ctrl+C only clears the input
        app.key(key('h'), &mut serve);
        app.key(ctrl_c, &mut serve);
        assert!(app.input.is_empty() && !app.quit);
        assert_eq!(app.work_text(), "idle", "clearing does not arm the exit");
        // first press on the empty input: the rule shows the hint, nothing quits
        app.key(ctrl_c, &mut serve);
        assert!(!app.quit);
        assert_eq!(app.work_text(), EXIT_HINT);
        // any other key cancels it
        app.key(key('x'), &mut serve);
        app.key(crossterm::event::KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &mut serve);
        assert_eq!(app.work_text(), "idle");
        // two presses in a row quit
        app.key(ctrl_c, &mut serve);
        app.key(ctrl_c, &mut serve);
        assert!(app.quit);
        // an expired first press only re-arms
        let mut app = App::new();
        app.exit_armed = Some(std::time::Instant::now() - EXIT_WINDOW);
        app.key(ctrl_c, &mut serve);
        assert!(!app.quit);
        assert_eq!(app.work_text(), EXIT_HINT);
        // while a turn runs, an empty-input Ctrl+C still interrupts and never quits
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        app.key(ctrl_c, &mut serve);
        app.key(ctrl_c, &mut serve);
        assert!(!app.quit);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "interrupt");
    }

    #[test]
    fn btw_marks_busy_and_interrupts_like_a_prompt() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/btw why?", &mut serve);
        assert!(app.busy);
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut serve,
        );
        let interrupt: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(interrupt["cmd"], "interrupt");
        let id: u64 = serve.sent[0].parse::<Value>().unwrap()["id"]
            .as_u64()
            .unwrap();
        app.frame(
            json!({ "type": "result", "id": id, "ok": false, "error": "aborted" }),
            &mut serve,
        );
        assert!(!app.busy);
    }

    #[test]
    fn btw_answer_reappears_when_the_viewer_was_closed() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/btw why?", &mut serve);
        let id: u64 = serve.sent[0].parse::<Value>().unwrap()["id"]
            .as_u64()
            .unwrap();
        app.viewer = None; // closed while the answer was still streaming
        app.frame(
            json!({
                "type": "result", "id": id, "ok": true,
                "data": {"text": "the answer"}
            }),
            &mut serve,
        );
        let viewer = app.viewer.expect("the answer must not be dropped");
        assert!(viewer.read_only);
        assert_eq!(viewer.text, "the answer");
        assert_eq!(serve.sent.len(), 1); // nothing entered the session
    }

    #[test]
    fn slash_search_stays_transcript_only() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.viewer = Some(Viewer::read_only("工作区 diff", "x"));
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut serve,
        );
        assert!(!app.viewer.as_ref().unwrap().search_input);
        app.viewer = Some(Viewer::transcript("Transcript", "x", vec![]));
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
            &mut serve,
        );
        assert!(app.viewer.as_ref().unwrap().search_input);
    }

    #[test]
    fn vertical_moves_never_rest_inside_a_collapsed_paste() {
        let mut app = App::new();
        app.input = "abc\npaste".chars().collect();
        app.pastes.push(PastedSpan {
            start: 4,
            end: 9,
            id: 1,
            extra_lines: 2,
            image: None,
        });
        app.cursor = 3;
        app.move_input_vertical(true);
        assert!(
            app.pastes
                .iter()
                .all(|p| app.cursor <= p.start || app.cursor >= p.end)
        );
        assert_eq!(app.cursor, 4);
    }

    #[test]
    fn a_range_touching_part_of_a_paste_consumes_the_whole_block() {
        let mut app = App::new();
        app.input = "abPASTEDcd".chars().collect();
        app.pastes.push(PastedSpan {
            start: 2,
            end: 8,
            id: 1,
            extra_lines: 0,
            image: None,
        });
        app.replace_input_range(3, 5, &['x']);
        assert_eq!(app.input.iter().collect::<String>(), "abxcd");
        assert!(app.pastes.is_empty());
    }

    #[test]
    fn vim_toggle_survives_a_serve_respawn() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/vim on", &mut serve);
        assert!(app.vim);
        app.frame(
            json!({
                "type": "hello", "sessionId": "12345678", "model": "mock/mock",
                "mode": "ask", "editor": "default", "tools": []
            }),
            &mut serve,
        );
        assert!(app.vim);
    }

    #[test]
    fn rewind_opens_a_filterable_timeline_and_sends_rewind_on_enter() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/rewind", &mut serve);
        assert_eq!(serve.sent.len(), 1);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "timeline");

        app.frame(json!({
            "type": "result", "id": sent["id"], "ok": true,
            "data": {"nodes": [
                {"entryIndex": 1, "ts": "2026-11-16T10:00:00Z", "firstLine": "fix the bug", "hasSnapshot": true},
                {"entryIndex": 9, "ts": "2026-11-16T11:00:00Z", "firstLine": "refactor it", "hasSnapshot": false}
            ]}
        }), &mut serve);
        assert!(app.picker.as_ref().unwrap().kind == PickerKind::Timeline);
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.items.len(), 2);
        // newest on top, cursor on it; labels keep the chronological #index
        assert!(
            picker.items[0].label.starts_with("#1 ·")
                && picker.items[0].label.contains("refactor it")
        );
        assert!(picker.items[0].note.contains("无代码快照"));
        assert!(
            picker.items[1].label.starts_with("#0 ·")
                && picker.items[1].label.contains("fix the bug")
        );
        assert_eq!(picker.sel, 0);

        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        assert!(app.picker.is_none());
        assert_eq!(serve.sent.len(), 2);
        let rew: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(rew["cmd"], "rewind");
        assert_eq!(
            rew["entryIndex"], 9,
            "Enter on the default cursor rewinds to the newest node"
        );
    }

    #[test]
    fn undo_redo_branch_report_restore_results_in_chat() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/undo", &mut serve);
        let id: u64 = serve.sent[0].parse::<Value>().unwrap()["id"]
            .as_u64()
            .unwrap();
        app.frame(json!({
            "type": "result", "id": id, "ok": true,
            "data": {"codeRestored": true, "restored": 12, "deleted": ["x.ts"], "kept": ["handmade.txt"]}
        }), &mut serve);
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("undo") && l.text.contains("x.ts"))
        );
        assert!(app.lines.iter().any(|l| l.text.contains("handmade")));

        app.run_slash("/redo", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "redo");
        app.run_slash("/branch", &mut serve);
        let sent3: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(sent3["cmd"], "branch");
    }

    #[test]
    fn back_and_forward_walk_the_session_history() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.session_full = "parent-0001".into();
        app.run_slash("/resume sub-0002", &mut serve);
        // the id is resolved by serve first; the switch follows its answer
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["cmd"], "session_resolve");
        assert_eq!(app.respawn, None);
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true, "data": {"id": "sub-0002"}}),
            &mut serve,
        );
        assert_eq!(app.respawn.take(), Some(Some("sub-0002".into())));
        app.session_full = "sub-0002".into(); // hello from the new serve
        app.run_slash("/back", &mut serve);
        assert_eq!(app.respawn.take(), Some(Some("parent-0001".into())));
        app.session_full = "parent-0001".into();
        app.run_slash("/back", &mut serve);
        assert_eq!(app.respawn, None, "nothing further back");
        app.run_slash("/forward", &mut serve);
        assert_eq!(app.respawn.take(), Some(Some("sub-0002".into())));
        app.session_full = "sub-0002".into();
        app.run_slash("/back", &mut serve);
        app.session_full = "parent-0001".into();
        // a fresh switch drops the forward entry, like a browser
        app.run_slash("/new", &mut serve);
        assert_eq!(app.respawn.take(), Some(None));
        app.session_full = "fresh-0003".into();
        app.run_slash("/forward", &mut serve);
        assert_eq!(app.respawn, None);
        app.run_slash("/back", &mut serve);
        assert_eq!(app.respawn.take(), Some(Some("parent-0001".into())));
    }

    #[test]
    fn fork_result_triggers_a_session_respawn() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/fork alt", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "fork");
        assert_eq!(sent["name"], "alt");
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({
                "type": "result", "id": id, "ok": true,
                "data": {"sessionId": "forked-123", "name": "alt"}
            }),
            &mut serve,
        );
        // a respawn is requested with the fork's session id, like /resume switching
        assert_eq!(app.respawn, Some(Some("forked-123".into())));
    }

    #[test]
    fn send_command_parses_and_reports_delivery() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/send abc123 测试跑完了，可以合并了", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "send");
        assert_eq!(sent["session"], "abc123");
        assert_eq!(sent["text"], "测试跑完了，可以合并了");

        // closed target: queued, delivered on its next open
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"to": "abc12345-x", "targetLive": null}}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.contains("已排队") && l.text.contains("abc12345")));

        // live target: shown within ~2s
        app.run_slash("/send abc123 再发一条", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"to": "abc12345-x", "targetLive": {"pid": 4242}}}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.contains("消息已发给会话")));

        // a target without a message is a usage hint, nothing sent
        serve.sent.clear();
        app.run_slash("/send abc123", &mut serve);
        assert!(serve.sent.is_empty());
        assert!(app.lines.iter().any(|l| l.text.contains("用法：/send")));

        // bare /send: sessions are listed, the picker's pick prefills the command
        let mut serve = Serve::mock();
        let sessions = json!({"type": "result", "ok": true, "data": {"sessions": [
            {"id": "ffff0000", "topic": "另一个会话", "at": "2026-11-23T10:00:00Z", "current": false},
            {"id": "current123", "topic": "本会话", "at": "2026-11-23T10:00:00Z", "current": true}
        ]}});
        let mut app = App::new();
        app.run_slash("/send", &mut serve);
        assert!(serve.sent[0].contains("\"cmd\":\"sessions\"") || serve.sent[0].contains("\"cmd\": \"sessions\""));
        let id: u64 = serde_json::from_str::<Value>(&serve.sent[0]).unwrap()["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": sessions["data"].clone()}),
            &mut serve,
        );
        let picker = app.picker.as_ref().expect("target picker open");
        assert_eq!(picker.kind, PickerKind::SendTarget);
        assert!(picker.items.iter().all(|i| !i.current), "the current session is not a target");
        app.picker.as_mut().unwrap().sel = 0;
        app.accept_picker(&mut serve);
        let typed: String = app.input.iter().collect();
        assert_eq!(typed, "/send ffff0000 ");
        assert_eq!(app.cursor, app.input.len());

        // history renders a received message in place
        let mut app = App::new();
        app.load_history(&json!({
            "items": [
                {"kind": "user", "text": "问点别的"},
                {"kind": "mail", "from": "ffff0000-1111-4222-8333-0123456789ab", "text": "测试跑完了"}
            ],
            "total": 2
        }));
        assert!(app.lines.iter().any(|l| l.text.contains("✉ 来自会话 0123456789ab 的消息")));
        assert!(app.lines.iter().any(|l| l.text.contains("测试跑完了")));
    }

    #[test]
    fn handoff_with_id_sends_session_and_note_and_does_not_switch() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/handoff abc123 next: add tests", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "handoff");
        assert_eq!(sent["session"], "abc123");
        assert_eq!(sent["note"], "next: add tests");
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"target": "abc12345-x", "brief": "b"}}),
            &mut serve,
        );
        assert_eq!(app.respawn, None);
        assert!(app.lines.iter().any(|l| l.text.contains("abc12345")));
        assert!(
            !app.lines.iter().any(|l| l.text.contains("另一个窗口")),
            "closed target: no warning"
        );
    }

    #[test]
    fn handoff_to_a_live_session_warns() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/handoff abc123", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {
                "target": "abc12345-x", "brief": "b", "queueId": "q1", "targetLive": {"pid": 4242}}}),
            &mut serve,
        );
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("另一个窗口") && l.text.contains("4242"))
        );
    }

    #[test]
    fn handoff_list_and_cancel_send_their_own_commands() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/handoff list", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "handoff_list");
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"handoffs": [
                {"queueId": "q1w2e3r4", "from": "aaaaaaaa-1", "target": "bbbbbbbb-2",
                 "queuedAt": "2026-11-20T09:00:00Z", "expired": true, "preview": "## Goal\nship it"}
            ]}}),
            &mut serve,
        );
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("q1w2e3r4") && l.text.contains("已过期"))
        );
        assert!(app.lines.iter().any(|l| l.text.contains("ship it")));

        app.run_slash("/handoff cancel q1w2", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent["cmd"], "handoff_cancel");
        assert_eq!(sent["queueId"], "q1w2");

        // bare cancel is a usage hint, not a command
        app.run_slash("/handoff cancel", &mut serve);
        assert_eq!(serve.sent.len(), 2);
    }

    #[test]
    fn bare_takeover_opens_a_picker_without_the_current_session_and_sends_on_pick() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/takeover", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "sessions");
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"sessions": [
                {"id": "me-1", "createdAt": "2026-11-20T10:00", "messages": 4, "cwd": "/w", "name": "", "current": true},
                {"id": "other-2", "createdAt": "2026-11-20T09:00", "messages": 9, "cwd": "/w", "name": "", "current": false}
            ]}}),
            &mut serve,
        );
        let picker = app.picker.as_ref().expect("picker");
        assert_eq!(picker.kind, PickerKind::TakeoverSource);
        assert_eq!(picker.items.len(), 1);
        app.accept_picker(&mut serve);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["cmd"], "takeover");
        assert_eq!(sent["session"], "other-2");
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"from": "other-2", "brief": "## Goal\nTAKEN"}}),
            &mut serve,
        );
        assert_eq!(app.respawn, None);
        assert!(app.lines.iter().any(|l| l.text.contains("TAKEN")));
    }

    #[test]
    fn sessions_picker_leads_with_topic() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/resume", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"sessions": [
                {"id": "abc12345-eeee", "createdAt": "2026-11-20T09:00", "ago": "3 小时前", "messages": 9,
                 "cwd": "/Users/lucas/tango", "name": "", "topic": "修复鼠标选区漂移的问题", "current": false},
                {"id": "fff67890-dddd", "createdAt": "2026-11-19T09:00", "messages": 0,
                 "cwd": "/tmp", "name": "", "topic": "", "current": false}
            ]}}),
            &mut serve,
        );
        let picker = app.picker.as_ref().expect("picker");
        let first = &picker.items[0];
        assert!(
            first.label.starts_with("修复鼠标选区漂移的问题"),
            "topic leads the label: {}",
            first.label
        );
        assert!(
            first.note.contains("tango"),
            "note carries cwd: {}",
            first.note
        );
        assert!(
            first.note.contains("9"),
            "note carries count: {}",
            first.note
        );
        assert!(
            first.note.contains("3 小时前"),
            "note carries relative time: {}",
            first.note
        );
        // a session with no first prompt falls back to the id, not an empty label;
        // without an ago field the note keeps the raw date
        let second = &picker.items[1];
        assert!(
            second.label.starts_with("fff67890"),
            "fallback keeps the id prefix: {}",
            second.label
        );
        assert!(
            second.note.contains("11-19"),
            "no ago field falls back to the raw date: {}",
            second.note
        );
    }

    #[test]
    fn session_picker_renders_two_line_rows() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.serve_cwd = Some("/Users/lucas/tango".into());
        app.run_slash("/resume", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"sessions": [
                {"id": "s-one", "createdAt": "2026-11-20T09:00", "ago": "3 小时前", "messages": 9,
                 "cwd": "/Users/lucas/tango", "name": "", "topic": "修复选区漂移", "current": false, "branch": "main"}
            ]}}),
            &mut serve,
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        // the topic and its meta line render on DIFFERENT screen rows
        let line_of = |needle: &str| -> Option<usize> {
            (0..30).find(|&y| {
                let row: String = terminal.backend().buffer().content[y * 100..(y + 1) * 100]
                    .iter()
                    .map(|c| c.symbol().to_string())
                    .collect();
                row.replace(' ', "").contains(needle)
            })
        };
        let topic_y = line_of("修复选区漂移").expect("topic on screen");
        let meta_y = line_of("条·").expect("meta line on screen");
        assert_ne!(
            topic_y, meta_y,
            "two-line layout: title and meta on separate rows"
        );
        assert!(meta_y > topic_y, "meta below the title");
    }

    #[test]
    fn session_picker_space_previews_in_viewer() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.nvim_preference = "builtin".into(); // previews open the built-in viewer
        app.serve_cwd = Some("/w".into());
        app.run_slash("/resume", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"sessions": [
                {"id": "prev-1", "createdAt": "x", "messages": 2, "cwd": "/w", "name": "",
                 "topic": "看看预览", "current": false, "branch": ""}
            ]}}),
            &mut serve,
        );
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut serve,
        );
        let sent: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent["cmd"], "session_preview");
        assert_eq!(sent["session"], "prev-1");
        let cmd_id = sent["id"].as_u64().unwrap();
        // serve replies with the digest → the read-only viewer shows it
        app.frame(
            json!({"type": "result", "id": cmd_id, "ok": true,
                   "data": {"preview": "❯ 看看预览\n• 好的，这是回复"}}),
            &mut serve,
        );
        let v = app.viewer.as_ref().expect("preview viewer opened");
        assert!(v.read_only);
        assert!(v.text.contains("看看预览"));
    }

    #[test]
    fn footer_hints_are_clickable() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.mode = "auto".into();
        app.serve_cwd = Some("/work/x".into());
        let mut term = Terminal::new(TestBackend::new(80, 40)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let (m, p) = (app.footer_mode_area, app.footer_palette_area);
        assert!(m.width > 0 && p.width > 0, "both hints have a rect");
        let b = term.backend().buffer();
        let at = |r: Rect| (r.x..r.x + r.width).map(|x| b[(x, r.y)].symbol()).collect::<String>();
        assert_eq!(at(m), "shift+tab mode");
        assert_eq!(at(p), "ctrl+p commands");
        let click = |app: &mut App, serve: &mut Serve, r: Rect| {
            app.handle(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: r.x + 1,
                    row: r.y,
                    modifiers: KeyModifiers::NONE,
                }),
                serve,
            );
        };
        // ctrl+p hint opens the command palette; a second click closes it
        click(&mut app, &mut serve, p);
        assert_eq!(app.picker.as_ref().map(|p| p.kind), Some(PickerKind::Command));
        click(&mut app, &mut serve, p);
        assert!(app.picker.is_none());
        assert!(serve.sent.is_empty(), "the palette hint does not talk to serve");
        // shift+tab hint → the mode picker on the current mode (the key cycles; a click asks)
        click(&mut app, &mut serve, m);
        let picker = app.picker.as_ref().expect("a click on the hint opens the picker");
        assert_eq!(picker.kind, PickerKind::Mode);
        assert_eq!(picker.items[picker.sel].id, "auto", "the current mode is selected");
        assert!(serve.sent.is_empty(), "nothing switches until a mode is picked");
        // a second click closes it; from the command palette it swaps to the mode picker
        click(&mut app, &mut serve, m);
        assert!(app.picker.is_none());
        click(&mut app, &mut serve, p);
        click(&mut app, &mut serve, m);
        assert_eq!(app.picker.as_ref().map(|p| p.kind), Some(PickerKind::Mode));
        assert!(serve.sent.is_empty());
    }

    #[test]
    fn short_session_is_the_uuid_tail() {
        assert_eq!(short_session("427f7386-2f24-4923-93f7-0ad2040a8182"), "0ad2040a8182");
        assert_eq!(short_session("a1b2c3d4e5f67890"), "a1b2c3d4e5f67890");
        assert_eq!(short_session("task-晨间摘要-2026-11-23-13-32-04"), "task-晨间摘要-2026-11-23-13-32-04");
    }

    #[test]
    fn resume_with_an_id_resolves_before_switching() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        // no match: an error line, the current serve stays (no respawn)
        app.run_slash("/resume /resume 427f7386", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "session_resolve");
        assert_eq!(sent["session"], "/resume 427f7386");
        assert_eq!(app.respawn, None, "nothing torn down before serve answers");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": false,
                   "error": "no session matches \"/resume 427f7386\" (tango --sessions)"}),
            &mut serve,
        );
        assert_eq!(app.respawn, None);
        let last = app.lines.last().unwrap();
        assert_eq!(last.tone, Tone::Error);
        assert!(last.text.contains("no session matches"), "{}", last.text);
        assert!(last.text.contains("当前会话不变"), "{}", last.text);
        // ambiguous prefix: same, an error and no switch
        app.run_slash("/resume 4", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": false,
                   "error": "ambiguous \"4\": 427f7386, 4a1b2c3d"}),
            &mut serve,
        );
        assert_eq!(app.respawn, None);
        assert!(app.lines.last().unwrap().text.contains("ambiguous"));
        // unique hit: switch to the full id serve resolved
        app.run_slash("/resume 427f", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
                   "data": {"id": "427f7386-2f24-4923-93f7-0ad2040a8182"}}),
            &mut serve,
        );
        assert_eq!(
            app.respawn,
            Some(Some("427f7386-2f24-4923-93f7-0ad2040a8182".into()))
        );
    }

    #[test]
    fn session_picker_ctrl_r_renames() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.serve_cwd = Some("/w".into());
        app.run_slash("/resume", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"sessions": [
                {"id": "ren-1", "createdAt": "x", "messages": 2, "cwd": "/w", "name": "",
                 "topic": "待重命名", "current": false, "branch": ""}
            ]}}),
            &mut serve,
        );
        // ctrl+r enters rename mode; typing edits the name; enter commits
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
            &mut serve,
        );
        for c in "新名字".chars() {
            app.key(
                crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut serve,
            );
        }
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        let sent: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent["cmd"], "session_rename");
        assert_eq!(sent["session"], "ren-1");
        assert_eq!(sent["name"], "新名字");
        assert!(app.picker.is_none(), "picker closes after renaming");
    }

    #[test]
    fn session_picker_project_and_branch_filters() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.serve_cwd = Some("/proj-a".into());
        app.serve_branch = "main".into();
        app.run_slash("/resume", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"sessions": [
                {"id": "a-main", "createdAt": "1", "messages": 1, "cwd": "/proj-a", "name": "",
                 "topic": "A主分支", "current": false, "branch": "main"},
                {"id": "a-dev", "createdAt": "2", "messages": 1, "cwd": "/proj-a", "name": "",
                 "topic": "A开发分支", "current": false, "branch": "dev"},
                {"id": "b-one", "createdAt": "3", "messages": 1, "cwd": "/proj-b", "name": "",
                 "topic": "B项目", "current": false, "branch": "main"}
            ]}}),
            &mut serve,
        );
        let visible = |app: &App| -> Vec<String> {
            app.picker
                .as_ref()
                .unwrap()
                .filtered_items()
                .iter()
                .map(|i| i.id.clone())
                .collect()
        };
        // default: this project only
        assert_eq!(visible(&app), vec!["a-main", "a-dev"]);
        // ctrl+b: only sessions on this branch
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            &mut serve,
        );
        assert_eq!(visible(&app), vec!["a-main"]);
        // ctrl+b again: back to project scope
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            &mut serve,
        );
        assert_eq!(visible(&app), vec!["a-main", "a-dev"]);
        // ctrl+a: every project
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &mut serve,
        );
        assert_eq!(visible(&app), vec!["a-main", "a-dev", "b-one"]);
    }

    #[test]
    fn bubble_mode_wraps_border_rows_and_maps() {
        // width 40: the bubble wraps text at 36 (2 border/pad columns each side)
        let texts = vec!["一二三四五六七八九十壹贰".to_string()];
        let flags = vec![true];
        let rows = wrap_chat_rows_mixed(&texts, 40, &flags, &[], &[], true);
        assert!(
            rows.iter()
                .filter(|r| r.user)
                .all(|r| display_width(&r.text) <= 36),
            "text wraps inside the border frame"
        );
        let mut rows = rows;
        layout_user_bubbles(&mut rows, &flags, 40, true);
        let first = rows.first().unwrap();
        assert!(first.border, "opens with a top border row");
        assert!(
            rows.last().unwrap().border,
            "closes with a bottom border row"
        );
        let x = rows.iter().find(|r| r.user).unwrap().x;
        assert!(x >= 2, "frame sits right of the margin: {x}");
        // mapping: interior starts at x + 2 (│ + space)
        let mut app = App::new();
        app.chat_inner_w = 40;
        app.chat_scroll_offset = 0;
        app.chat_rows = rows;
        let g = app
            .chat_rows
            .iter()
            .position(|r| r.user && !r.border)
            .unwrap() as u16;
        let hit = app.chat_line_hit(g, x as u16 + 2, 0).map(|(_, _, i)| i);
        assert_eq!(hit, Some(0), "first interior column maps to char 0");
        // border rows never leak into copied text
        app.sel_anchor_pos = Some((0, 0));
        app.sel_end_pos = Some((g, x as u16 + 4));
        let sel = app.selected_text(0).unwrap();
        assert!(sel.starts_with("一"), "borders excluded: {sel:?}");
        assert!(
            !sel.contains("\n\n"),
            "no blank lines from border rows: {sel:?}"
        );
    }

    #[test]
    fn adjacent_and_multiline_user_messages_get_exactly_one_frame_each() {
        // two adjacent user lines + one wrapping user line: every user line gets
        // exactly one top and one bottom border, nothing nested
        let texts = vec![
            "第一句".to_string(),
            "第二句".to_string(),
            "一二三四五六七八九十一二三四五六七八".to_string(),
        ];
        let flags = vec![true, true, true];
        let mut rows = wrap_chat_rows_mixed(&texts, 24, &flags, &[], &[], true);
        layout_user_bubbles(&mut rows, &flags, 24, true);
        let borders = rows.iter().filter(|r| r.border).count();
        let text_rows = rows.iter().filter(|r| r.user && !r.border).count();
        assert_eq!(
            borders, 6,
            "3 user lines → 3 frames → 6 border rows, got layout: {rows:?}"
        );
        assert_eq!(
            text_rows, 4,
            "wrapped third line takes 2 rows at width 24-4-margin"
        );
        // every user line's group starts with a top border and ends with a bottom
        for (li, _) in texts.iter().enumerate() {
            let group: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.line == li)
                .map(|(i, _)| i)
                .collect();
            assert!(
                rows[group[0]].border && rows[*group.last().unwrap()].border,
                "line {li} not fully framed: {rows:?}"
            );
        }
    }

    #[test]
    fn bubble_toggle_redraws_after_off_on_cycle() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/bubble off", &mut serve);
        app.push_user("第一句");
        app.run_slash("/bubble", &mut serve); // toggle back on
        app.push_user("第二句");
        {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        let row = app
            .chat_rows
            .iter()
            .find(|r| r.user && r.text.contains("第二句"))
            .expect("second user row");
        assert!(
            row.x > 0,
            "after toggling back on the bubble is right-positioned"
        );
        assert!(
            app.chat_rows.iter().any(|r| r.border),
            "frame rows present again"
        );
    }

    #[test]
    fn bubble_mode_toggles_between_right_and_fullwidth_left() {
        let mut rows = wrap_chat_rows(&["用户的一句话".to_string(), "agent 回复".to_string()], 40);
        let flags = vec![true, false];
        layout_user_bubbles(&mut rows, &flags, 40, true);
        assert!(
            rows[0].user && rows[0].x > 0,
            "default: right-positioned bubble"
        );
        let mut rows = wrap_chat_rows(&["用户的一句话".to_string()], 40);
        layout_user_bubbles(&mut rows, &[true], 40, false);
        assert!(
            rows[0].user && rows[0].x == 0,
            "claude-code mode: full-width, left, still user-styled"
        );
        // the slash command flips it and persists through serve
        let mut app = App::new();
        let mut serve = Serve::mock();
        assert!(app.user_bubble, "default is the right bubble");
        app.run_slash("/bubble off", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "set_bubble");
        assert_eq!(sent["bubble"], false);
        assert!(!app.user_bubble);
        app.run_slash("/bubble", &mut serve);
        assert!(app.user_bubble, "bare /bubble toggles back on");
    }

    fn mouse(app: &mut App, serve: &mut Serve, kind: MouseEventKind, column: u16, row: u16) {
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }),
            serve,
        );
    }

    fn input_screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
        use ratatui::{Terminal, backend::TestBackend};
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let b = t.backend().buffer();
        (0..h)
            .map(|y| (0..w).map(|x| b[(x, y)].symbol().to_string()).collect())
            .collect()
    }

    #[test]
    fn dragging_in_the_input_box_selects_and_copies_that_text() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.input = "hello brave world".chars().collect();
        app.cursor = app.input.len();
        input_screen(&mut app, 40, 12);
        let y = app.input_text_top(); // first draft row inside the block
        let x0 = INPUT_X;
        mouse(
            &mut app,
            &mut serve,
            MouseEventKind::Down(MouseButton::Left),
            x0 + 6,
            y,
        );
        mouse(
            &mut app,
            &mut serve,
            MouseEventKind::Drag(MouseButton::Left),
            x0 + 11,
            y,
        );
        mouse(
            &mut app,
            &mut serve,
            MouseEventKind::Up(MouseButton::Left),
            x0 + 11,
            y,
        );
        assert_eq!(app.input_sel, Some((6, 11)));
        assert_eq!(app.input_raw_text(6, 11), "brave");
        assert!(
            app.toast.as_ref().is_some_and(|(t, _)| t == "已复制 5 个字符到剪贴板")
        );
        // the selection stays highlighted until the next key
        let b = {
            use ratatui::{Terminal, backend::TestBackend};
            let mut t = Terminal::new(TestBackend::new(40, 12)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            t.backend().buffer().clone()
        };
        use ratatui::style::Modifier;
        let rev = |x: u16| b[(x0 + x, y)].modifier.contains(Modifier::REVERSED);
        assert!(!rev(5) && rev(6) && rev(10) && !rev(11));
        app.key(KeyCode::Char('x').into(), &mut serve);
        assert_eq!(app.input_sel, None);
    }

    #[test]
    fn clicking_in_the_input_box_moves_the_cursor_there() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.input = "第一行\nabc 中文".chars().collect();
        app.cursor = app.input.len();
        input_screen(&mut app, 40, 12);
        let y = app.input_text_top();
        let click = |app: &mut App, serve: &mut Serve, x: u16, y: u16| {
            app.input_last_click = None; // separate clicks, never a double-click
            mouse(app, serve, MouseEventKind::Down(MouseButton::Left), x, y);
            mouse(app, serve, MouseEventKind::Up(MouseButton::Left), x, y);
        };
        click(&mut app, &mut serve, INPUT_X + 3, y); // right half of 一 (columns 2-3)
        assert_eq!(app.cursor, 1);
        click(&mut app, &mut serve, INPUT_X + 5, y + 1); // the 中 after "abc "
        assert_eq!(app.cursor, 8);
        click(&mut app, &mut serve, INPUT_X + 30, y); // past the end of row 1
        assert_eq!(app.cursor, 3, "before the newline");
        assert_eq!(app.input_sel, None);
        assert!(
            app.toast.is_none(),
            "a click copies nothing"
        );
    }

    #[test]
    fn double_click_in_the_input_box_selects_all_and_copies_the_raw_text() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.input = "see ".chars().collect();
        app.cursor = app.input.len();
        app.insert_paste("line one\nline two\nline three");
        let screen = input_screen(&mut app, 60, 12);
        let y = app.input_text_top();
        assert!(screen[y as usize].contains("[Pasted text #"), "{screen:#?}");
        for _ in 0..2 {
            mouse(
                &mut app,
                &mut serve,
                MouseEventKind::Down(MouseButton::Left),
                2,
                y,
            );
            mouse(
                &mut app,
                &mut serve,
                MouseEventKind::Up(MouseButton::Left),
                2,
                y,
            );
        }
        let total = app.rendered_input().text.chars().count();
        assert_eq!(app.input_sel, Some((0, total)), "everything is selected");
        let raw: String = app.input.iter().collect();
        assert!(raw.contains("line three"));
        let n = raw.chars().filter(|c| *c != '\n').count();
        assert!(
            app.toast.as_ref().is_some_and(|(t, _)| *t == format!("已复制 {n} 个字符到剪贴板")),
            "the paste label expands to the full text on the clipboard"
        );
    }

    #[test]
    fn ctrl_c_clears_text_first_and_interrupts_only_an_empty_input() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        app.input = "draft".chars().collect();
        app.cursor = 5;
        app.key(KeyCode::Char('c').into(), &mut serve);
        assert_eq!(app.input.iter().collect::<String>(), "draftc", "plain c just types");
        let ctrl_c = crossterm::event::KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        );
        app.key(ctrl_c, &mut serve);
        assert!(app.input.is_empty(), "first ctrl+c clears the text");
        assert!(
            !serve.sent.iter().any(|m| m.contains("interrupt")),
            "no interrupt while text was present"
        );
        app.key(ctrl_c, &mut serve);
        assert!(
            serve.sent.iter().any(|m| m.contains("interrupt")),
            "second ctrl+c interrupts the turn"
        );
    }

    #[test]
    fn up_down_in_the_input_walk_history_and_restore_the_draft() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        for t in ["first", "second", "third"] {
            app.input = t.chars().collect();
            app.send_prompt(&mut serve);
        }
        assert_eq!(app.prompt_history, vec!["first", "second", "third"]);
        // type an unsent draft, then walk back through history
        app.input = "ABC".chars().collect();
        app.cursor = 3;
        let text = |app: &App| app.input.iter().collect::<String>();
        let up = |app: &mut App| app.history_up();
        let down = |app: &mut App| app.history_down();
        assert!(up(&mut app) && up(&mut app) && up(&mut app));
        assert_eq!(app.history_nav, Some(0));
        assert_eq!(text(&app), "first");
        assert!(up(&mut app), "stays at the oldest");
        assert_eq!(app.history_nav, Some(0));
        assert!(down(&mut app));
        assert_eq!(text(&app), "second");
        // down past the newest entry lands back on the unsent draft
        assert!(down(&mut app) && down(&mut app));
        assert_eq!(app.history_nav, None);
        assert_eq!(text(&app), "ABC");
        assert_eq!(app.cursor, 3);
        // the user's exact story: recall XYZ over draft ABC, edit, send
        app.prompt_history.push("XYZ".into());
        app.input = "ABC".chars().collect();
        app.cursor = 3;
        assert!(up(&mut app));
        assert_eq!(text(&app), "XYZ");
        for c in "O".chars() {
            app.insert_char(c);
        }
        app.send_prompt(&mut serve);
        assert_eq!(app.prompt_history.last().map(String::as_str), Some("XYZO"));
        assert_eq!(text(&app), "ABC", "the draft comes back after sending");
        assert_eq!(app.history_nav, None, "navigation ended by the send");
        // esc while browsing (turn done, so esc is not interrupt): draft back
        app.busy = false;
        app.input = "keep".chars().collect();
        app.cursor = 4;
        assert!(up(&mut app));
        assert_eq!(text(&app), "XYZO");
        app.key(KeyCode::Esc.into(), &mut serve);
        assert_eq!(text(&app), "keep");
    }

    #[test]
    fn up_down_in_a_multiline_input_move_the_cursor_until_the_edge() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.prompt_history.push("old".into());
        app.input = "one\ntwo\nthree".chars().collect();
        app.cursor = app.input.len(); // end of line 3
        assert!(app.history_up());
        assert_eq!(app.cursor_line(), (1, 3), "climbed to line 2");
        assert_eq!(app.history_nav, None, "not history yet");
        assert!(app.history_up());
        assert_eq!(app.cursor_line(), (0, 3));
        assert!(app.history_up(), "now it recalls history");
        assert!(app.history_nav.is_some());
        assert!(app.history_down());
        assert_eq!(
            app.cursor_line(),
            (0, 3),
            "down enters the recalled text first"
        );
        // empty history + empty input: the key is not consumed
        let mut fresh = App::new();
        assert!(!fresh.history_up());
        assert!(!fresh.history_down());
    }

    #[test]
    fn thinking_lines_get_a_two_column_gutter_and_wrap_narrower() {
        let texts = vec![
            "普通一行".to_string(),
            format!("{THINK_ICON} 思考内容比较长需要折行思考内容比较长需要折行"),
        ];
        let flags = vec![false, true];
        let rows = wrap_chat_rows_flagged(&texts, 12, &flags);
        assert!(!rows[0].think);
        assert!(rows.iter().skip(1).all(|r| r.think), "all rows of the thinking line");
        assert!(
            rows.iter().skip(1).all(|r| display_width(&r.text) <= 10),
            "wraps at 12-2 for the gutter: {:?}",
            rows.iter().map(|r| r.text.as_str()).collect::<Vec<_>>()
        );
        // click mapping skips the "▍ " gutter
        let mut app = App::new();
        app.chat_rows = vec![ChatRow {
            line: 0,
            cs: 0,
            ce: 4,
            text: "汉字汉字".into(),
            x: 0,
            user: false,
            think: true,
            border: false,
            wordmark: false,
        }];
        assert_eq!(app.chat_line_hit(0, 1, 0).map(|(_, _, i)| i), Some(0), "gutter clamps to 0");
        assert_eq!(app.chat_line_hit(0, 2, 0).map(|(_, _, i)| i), Some(0), "text starts at col 2");
        assert_eq!(app.chat_line_hit(0, 4, 0).map(|(_, _, i)| i), Some(1), "汉 spans 2-3");
    }

    #[test]
    fn long_bash_commands_show_a_one_line_brief_and_interrupts_are_warnings() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        let long = format!("for p in a b; do\n  curl {}\ndone", "x".repeat(300));
        app.handle(
            Event::Server(
                0,
                serde_json::json!({"type":"tool_result","call":{"id":"t9","name":"bash","input":{"command":long}},"ms":7,"isError":true,"output":"/a\n[interrupted]"}),
            ),
            &mut serve,
        );
        let line = app.lines.last().unwrap();
        assert_eq!(line.text, "⚙ bash for p in a b; do…（共 3 行） · 7ms · 已中断");
        assert!(line.tone == Tone::Warning, "an interrupt is not an error");
        assert_eq!(line.call.as_deref(), Some(long.as_str()));
        // the viewer shows the whole command above the output
        let text = App::line_viewer_text(line).unwrap();
        assert!(text.starts_with("$ for p in a b; do\n  curl xxx"), "{text}");
        assert!(text.ends_with("/a\n[interrupted]"), "{text}");
        // drawn, the line is refitted to the chat width: always exactly one row
        for w in [40u16, 60, 100, 150] {
            let mut t = Terminal::new(TestBackend::new(w, 12)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            let shown = app.chat_lines().last().unwrap().text.clone();
            assert!(display_width(&shown) <= w as usize, "{w}: {shown}");
            assert!(shown.ends_with("…（共 3 行） · 7ms · 已中断"), "{w}: {shown}");
            assert_eq!(app.chat_rows.iter().filter(|r| r.text.starts_with("⚙")).count(), 1);
        }
        let long_one = format!("echo {}", "y".repeat(300));
        app.handle(
            Event::Server(
                0,
                serde_json::json!({"type":"tool_result","call":{"id":"t8","name":"bash","input":{"command":long_one}},"ms":3,"isError":false,"output":"y"}),
            ),
            &mut serve,
        );
        let mut t = Terminal::new(TestBackend::new(80, 12)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let shown = app.chat_lines().last().unwrap().text.clone();
        assert!(display_width(&shown) <= 80 && shown.ends_with("y… · 3ms"), "{shown}");
        // fit_call: short calls stay whole, CJK counts 2 columns
        assert_eq!(fit_call("ls", 10), "ls");
        assert_eq!(fit_call("一二三四五六七八九十", 12), "一二三四五…");
        // a real failure stays red; old sessions' [exit null] reads as interrupted
        assert!(!App::interrupted_output(Some("boom\n[exit 1]")));
        assert!(App::interrupted_output(Some("[exit null]")));
        // the transcript keeps the full command
        app.open_transcript();
        let t = &app.viewer.as_ref().unwrap().text;
        assert!(t.contains("  $   curl xxx"), "{t}");
    }

    #[test]
    fn running_gear_cycles_color_and_settles_when_the_tool_finishes() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        app.handle(
            Event::Server(
                0,
                serde_json::json!({"type":"stream","event":{"type":"tool_call_start","id":"t1","name":"bash"}}),
            ),
            &mut serve,
        );
        assert_eq!(app.lines.last().unwrap().text, "⚙ bash …");
        assert!(app.running_tools.contains("t1"));
        {
            let mut t = Terminal::new(TestBackend::new(30, 8)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            let b = t.backend().buffer();
            // frame is frozen at 0 under test: the wave's bright first step,
            // in 24-bit or the 256-color cube
            let fg = b[(0, 0)].fg;
            assert!(
                matches!(fg, Color::Indexed(123) | Color::Rgb(120, 225, 240)),
                "running gear is lit: {fg:?}"
            );
        }
        app.handle(
            Event::Server(
                0,
                serde_json::json!({"type":"tool_result","call":{"id":"t1","name":"bash","input":{"command":"ls"}},"ms":5,"isError":false,"output":"x"}),
            ),
            &mut serve,
        );
        assert!(app.running_tools.is_empty(), "the result retires the id");
        // the running line froze INTO the summary: one ⚙ line, not two
        let gears: Vec<&str> = app
            .lines
            .iter()
            .filter(|l| l.text.starts_with("⚙"))
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(gears, vec!["⚙ bash ls · 5ms"], "{gears:?}");
        assert_eq!(app.lines.last().unwrap().output.as_deref(), Some("x"));
        assert_eq!(app.lines.last().unwrap().tool_id, None);
        {
            let mut t = Terminal::new(TestBackend::new(30, 8)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            let b = t.backend().buffer();
            let gear_row = app.chat_rows.iter().position(|r| r.text.starts_with("⚙")).unwrap();
            assert_eq!(
                b[(0, gear_row as u16)].fg,
                Color::DarkGray,
                "finished gears settle back to the muted tone"
            );
        }
        // and the pure cycle steps through the wave as frames advance
        assert_eq!(gear_color(0, true), gear_color(8, true), "period 8 frames");
        assert_ne!(gear_color(0, true), gear_color(2, true), "color moves");
        assert_eq!(gear_color(7, false), Color::DarkGray, "idle is flat");
    }

    #[test]
    fn thinking_bar_wave_flows_down_and_freezes_when_idle() {
        // not animating: flat dim
        assert_eq!(thinking_bar_color(0, 0, false), Color::DarkGray);
        assert_eq!(thinking_bar_color(99, 5, false), Color::DarkGray);
        // animating: the wave flows downward — a row's color now reappears one row
        // below on the next frame
        for f in 0..16u128 {
            for r in 0..7usize {
                assert_eq!(
                    thinking_bar_color(f, r, true),
                    thinking_bar_color(f.wrapping_add(1), r + 1, true),
                    "frame {f} row {r}"
                );
            }
        }
        // the wave is periodic in rows and covers the bright head
        let wave: Vec<Color> = (0..8).map(|r| thinking_bar_color(0, r, true)).collect();
        assert_eq!(wave[0], think_wave()[0]);
        assert!(wave.contains(&think_wave()[4]), "the bright head is in the wave");
        // both palettes share the shape: dim ends, one bright peak, mirrored
        for w in [&THINK_WAVE, &THINK_WAVE_256] {
            assert_eq!(w[3], w[5]);
            assert_eq!(w[1], w[7]);
            assert_ne!(w[4], w[3]);
        }
        assert_eq!(thinking_bar_color(8, 0, true), thinking_bar_color(0, 0, true));
    }

    #[test]
    fn long_thinking_is_kept_whole_and_collapses_with_an_expand_hint() {
        let mut app = App::new();
        let body: String = (0..10).map(|i| format!("第{i}步推理")).collect::<Vec<_>>().join("\n");
        app.thinking = body.clone();
        app.commit_stream();
        let line = app.lines.last().unwrap();
        assert_eq!(line.text, format!("{THINK_ICON} {body}"), "no 400-char truncation");
        let shown = app.chat_lines().last().unwrap().text.clone();
        assert_eq!(shown.lines().count(), 3, "3 preview lines, the hint rides after the …: {shown}");
        assert!(shown.lines().last().unwrap().ends_with("… 双击展开思考（还有 42 字 · 共 10 行）"), "{shown}");
        // a second long block: only the count, glued to the …
        app.thinking = body.clone();
        app.commit_stream();
        let second = app.chat_lines().last().unwrap().text.clone();
        assert!(!second.contains("双击"), "taught once, not on every block: {second}");
        assert!(second.ends_with("…（还有 42 字 · 共 10 行）"), "{second}");
        app.lines.pop();
        assert!(!shown.contains("第9步推理"));
        app.lines.last_mut().unwrap().expanded = true; // what a double-click does
        let shown = app.chat_lines().last().unwrap().text.clone();
        assert!(shown.contains("第9步推理") && shown.contains("双击收起思考"), "{shown}");
        app.lines.last_mut().unwrap().expanded = false;
        assert!(!app.chat_lines().last().unwrap().text.contains("第9步推理"));
    }

    #[test]
    fn short_thinking_shows_in_full_without_a_hint() {
        let mut app = App::new();
        app.thinking = "想一下".into();
        app.commit_stream();
        assert_eq!(app.chat_lines().last().unwrap().text, format!("{THINK_ICON} 想一下"));
        assert_eq!(display_width(THINK_ICON), 2, "one emoji, two cells");
        app.ascii = true;
        assert_eq!(app.chat_lines().last().unwrap().text, "* 想一下");
    }

    #[test]
    fn clipboard_command_sets_and_persists_the_copy_path() {
        use crate::clipboard::{Mode, mode, set_mode};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/clipboard osc52", &mut serve);
        assert_eq!(mode(), Mode::Osc52);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!((sent["cmd"].as_str(), sent["clipboard"].as_str()), (Some("set_clipboard"), Some("osc52")));
        app.run_slash("/clipboard bogus", &mut serve);
        assert_eq!(mode(), Mode::Osc52, "bad arg changes nothing");
        assert_eq!(serve.sent.len(), 1);
        app.run_slash("/clipboard", &mut serve);
        assert!(app.lines.last().unwrap().text.contains("剪贴板：osc52"));
        set_mode(Mode::Auto);
    }

    #[test]
    fn ascii_mode_plainifies_emoji() {
        assert_eq!(asciify("❓ 选 · ➕ 并入 · 📋 排队 · 📂 目录 · 🎉✅"), "? 选 · + 并入 · # 排队 · > 目录 · *v");
        assert_eq!(asciify("中文 ⚙ ❯ ▍"), "中文 ⚙ ❯ ▍", "non-emoji symbols stay");
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/ascii", &mut serve);
        assert!(app.ascii);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!((sent["cmd"].as_str(), sent["ascii"].as_bool()), (Some("set_ascii"), Some(true)));
        app.push("➕ 已并入", Tone::Success);
        assert_eq!(app.chat_lines().last().unwrap().text, "+ 已并入");
        app.run_slash("/ascii off", &mut serve);
        assert!(!app.ascii);
        app.run_slash("/ascii maybe", &mut serve);
        assert!(!app.ascii, "bad arg changes nothing");
    }

    #[test]
    fn thinking_rows_render_with_the_bar_and_the_text_indented() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.push(format!("{THINK_ICON} 这是思考"), Tone::Thinking);
        app.busy = true;
        let mut t = Terminal::new(TestBackend::new(30, 10)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let b = t.backend().buffer();
        let row: String = (0..30).map(|x| b[(x, 0)].symbol().to_string()).collect();
        // wide chars leave blank continuation cells — compare with spaces removed
        let tight = row.replace(' ', "");
        assert!(tight.starts_with("▍🤖"), "{row:?}");
        assert!(tight.contains("这是思考"), "{row:?}");
        assert_eq!(b[(1, 0)].symbol(), " ", "one space gutter pad");
        assert!(app.chat_rows.iter().all(|r| r.think));
        // the icon is upright, the reasoning italic
        let icon_x = (0..30).find(|&x| b[(x, 0)].symbol() == THINK_ICON).unwrap();
        assert!(!b[(icon_x, 0)].modifier.contains(Modifier::ITALIC), "🤖 not italic");
        let text_x = (0..30).find(|&x| b[(x, 0)].symbol() == "这").unwrap();
        assert!(b[(text_x, 0)].modifier.contains(Modifier::ITALIC), "reasoning stays italic");
    }

    #[test]
    fn status_session_id_click_copies_to_clipboard() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.session_full = "a1b2c3d4e5f67890".into();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let area = app.session_id_area;
        assert!(area.width > 0, "the sess span is a click target");
        assert_eq!(area.y, 23, "on the status row");
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(MouseButton::Left),
                column: area.x + 2,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert!(
            app.toast.as_ref().is_some_and(|(t, _)| t.contains("已复制会话 id") && t.contains("a1b2c3d4e5f67890")),
            "click confirms the copy"
        );
    }

    #[test]
    fn clicking_the_model_in_the_info_row_opens_the_model_picker_on_the_current_model() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.mode = "auto".into();
        app.model = "apikeyfan/claude-opus-5-5".into();
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let area = app.model_area;
        let b = terminal.backend().buffer();
        let under: String = (area.x..area.x + area.width).map(|x| b[(x, area.y)].symbol()).collect();
        assert_eq!(under, "claude-opus-5-5 apikeyfan", "the target covers exactly the model");
        let click = |app: &mut App, serve: &mut Serve, x: u16| {
            app.handle(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(MouseButton::Left),
                    column: x,
                    row: area.y,
                    modifiers: KeyModifiers::NONE,
                }),
                serve,
            );
        };
        let click_at = |app: &mut App, serve: &mut Serve, x: u16, y: u16| {
            app.handle(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: crossterm::event::MouseEventKind::Down(MouseButton::Left),
                    column: x,
                    row: y,
                    modifiers: KeyModifiers::NONE,
                }),
                serve,
            );
        };
        // the mode label beside it opens the mode picker instead, on the current mode
        let mode = app.mode_area;
        assert_eq!((mode.x, mode.y, mode.width), (INPUT_X, area.y, 4), "covers `auto`");
        click(&mut app, &mut serve, mode.x + 1);
        assert!(serve.sent.iter().all(|s| !s.contains("\"models\"")));
        let p = app.picker.take().expect("mode picker opens");
        assert!(p.kind == PickerKind::Mode);
        assert_eq!(p.items[p.sel].id, "auto", "current mode is selected");
        // the " · " between them is neither
        click(&mut app, &mut serve, mode.x + mode.width + 1);
        assert!(app.picker.is_none() && serve.sent.is_empty());
        click(&mut app, &mut serve, area.x + 3);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["cmd"], "models");
        app.frame(
            json!({"type":"result","id":sent["id"],"ok":true,"data":{"current":"apikeyfan/claude-opus-5-5","models":[
                {"id":"zhipu-plan/glm-5.4","platform":"zhipu","keyed":true},
                {"id":"apikeyfan/claude-opus-5-5","platform":"apikeyfan","keyed":true}
            ]}}),
            &mut serve,
        );
        let p = app.picker.as_ref().expect("model picker opens");
        assert!(p.kind == PickerKind::Model);
        assert_eq!(p.items[p.sel].id, "apikeyfan/claude-opus-5-5", "current model is selected");
        // a second click on the model closes it again
        terminal.draw(|f| app.draw(f)).unwrap();
        click(&mut app, &mut serve, area.x + 3);
        assert!(app.picker.is_none(), "second click closes the model picker");
        // clicking the mode while the model picker is open swaps to the mode picker
        app.run_slash("/model", &mut serve);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        let models = json!({"type":"result","id":sent["id"],"ok":true,"data":{"current":"apikeyfan/claude-opus-5-5","models":[
            {"id":"zhipu-plan/glm-5.4","platform":"zhipu","keyed":true},
            {"id":"apikeyfan/claude-opus-5-5","platform":"apikeyfan","keyed":true}
        ]}});
        app.frame(models.clone(), &mut serve);
        terminal.draw(|f| app.draw(f)).unwrap();
        click(&mut app, &mut serve, mode.x + 1);
        assert!(app.picker.as_ref().is_some_and(|p| p.kind == PickerKind::Mode), "mode replaces model");
        terminal.draw(|f| app.draw(f)).unwrap();
        click(&mut app, &mut serve, mode.x + 1);
        assert!(app.picker.is_none(), "second click closes the mode picker");
        // …and the other way round
        click(&mut app, &mut serve, mode.x + 1);
        terminal.draw(|f| app.draw(f)).unwrap();
        let n = serve.sent.len();
        click(&mut app, &mut serve, area.x + 3);
        assert!(app.picker.is_none() && serve.sent.len() == n + 1, "model click asks for the model list");
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["cmd"], "models");
        let mut models = models;
        models["id"] = sent["id"].clone();
        app.frame(models, &mut serve);
        // a click on a model row in the picker picks it
        terminal.draw(|f| app.draw(f)).unwrap();
        let b = terminal.backend().buffer();
        let pop = app.picker_area;
        let row = (pop.y..pop.y + pop.height)
            .find(|&y| (pop.x..pop.x + pop.width).map(|x| b[(x, y)].symbol()).collect::<String>().contains("glm-5.4"))
            .expect("glm-5.4 row drawn");
        click_at(&mut app, &mut serve, pop.x + 6, row);
        assert!(app.picker.is_none(), "picking closes the picker");
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["cmd"], "set_model");
        assert_eq!(sent["modelId"], "zhipu-plan/glm-5.4");
    }

    #[test]
    fn name_command_renames_current_session_and_shows_in_status() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.session_full = "cur-1234-abcd".into();
        app.run_slash("/name 修复气泡的会话", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "rename");
        assert_eq!(sent["name"], "修复气泡的会话");
        assert!(
            app.lines.iter().any(|l| l.text.contains("已重命名")),
            "chat confirms the rename"
        );
        // the status bar carries the name once set (hello's sessionName updates it too)
        app.session_name = "修复气泡的会话".into();
        assert!(app.status_segments_text().contains("修复气泡的会话"));
        // bare /name shows the current name instead of renaming
        let before = serve.sent.len();
        app.run_slash("/name", &mut serve);
        assert_eq!(serve.sent.len(), before, "bare /name sends nothing");
        assert!(app.lines.iter().any(|l| l.text.contains("修复气泡的会话")));
    }

    #[test]
    fn plain_resume_picker_is_unaffected_by_a_finished_transfer_pick() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/resume", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        let id = sent["id"].as_u64().unwrap();
        app.frame(
            json!({"type": "result", "id": id, "ok": true, "data": {"sessions": [
                {"id": "me-1", "createdAt": "x", "messages": 1, "cwd": "/w", "name": "", "current": true}
            ]}}),
            &mut serve,
        );
        assert_eq!(app.picker.as_ref().unwrap().kind, PickerKind::Session);
    }

    #[test]
    fn history_renders_handoff_and_takeover_briefs_in_place() {
        let mut app = App::new();
        app.load_history(&json!({
            "items": [
                {"kind": "user", "text": "first"},
                {"kind": "handoff", "via": "handoff", "from": "abcdef1234", "text": "## Goal\nship it"},
                {"kind": "handoff", "via": "takeover", "from": "fedcba9876", "text": "taken work"}
            ],
            "total": 3
        }));
        let texts: Vec<&str> = app.lines.iter().map(|l| l.text.as_str()).collect();
        let first = texts.iter().position(|t| t.contains("first")).unwrap();
        let recv = texts
            .iter()
            .position(|t| t.contains("收到会话 abcdef12"))
            .unwrap();
        let took = texts
            .iter()
            .position(|t| t.contains("已接管会话 fedcba98"))
            .unwrap();
        assert!(first < recv && recv < took);
    }

    #[test]
    fn tree_result_opens_a_read_only_viewer_with_indented_ancestry() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/tree", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "tree");
        app.frame(json!({
            "type": "result", "id": sent["id"], "ok": true,
            "data": {"nodes": [
                {"sessionId": "root-1", "name": "", "createdAt": "2026-11-16T09:00:00Z", "forkOf": null},
                {"sessionId": "fork-9", "name": "alt", "createdAt": "2026-11-16T10:00:00Z",
                 "forkOf": {"sessionId": "root-1", "entryIndex": 4}}
            ]}
        }), &mut serve);
        let viewer = app.viewer.as_ref().unwrap();
        assert!(viewer.read_only);
        assert!(viewer.title.contains("分叉"));
        assert!(viewer.text.contains("root-1"));
        assert!(viewer.text.contains("fork-9"));
    }

    #[test]
    fn find_opens_a_picker_and_acts_by_session() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/find MAGIC", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "find");
        assert_eq!(sent["text"], "MAGIC");
        app.frame(json!({
            "type": "result", "id": sent["id"], "ok": true,
            "data": {"hits": [
                {"sessionId": "current-id", "entryIndex": 3, "ts": "2026-11-16T10:00:00Z", "firstLine": "MAGIC here", "snippet": "…MAGIC here…", "orphaned": false},
                {"sessionId": "other-id", "entryIndex": 7, "ts": "2026-11-16T11:00:00Z", "firstLine": "MAGIC there", "snippet": "…MAGIC there…", "orphaned": true}
            ]}
        }), &mut serve);
        assert!(app.picker.as_ref().unwrap().kind == PickerKind::Find);
        // pick the OTHER session → respawn-resume it, no rewind sent
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut serve,
        );
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        assert_eq!(app.respawn, Some(Some("other-id".into())));
        // pick the CURRENT session → rewind to that entry
        app.respawn = None;
        app.picker = None;
        app.session = "current-id".into();
        app.run_slash("/find MAGIC", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        app.frame(json!({
            "type": "result", "id": sent2["id"], "ok": true,
            "data": {"hits": [
                {"sessionId": "current-id", "entryIndex": 3, "ts": "t", "firstLine": "MAGIC", "snippet": "s", "orphaned": false}
            ]}
        }), &mut serve);
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        let rew: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(rew["cmd"], "rewind");
        assert_eq!(rew["entryIndex"], 3);
        assert_eq!(rew["include"], true, "I4: find recovery is inclusive");
    }

    #[test]
    fn transcript_export_copy_rename_recap_wire_up() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        // transcript → read-only viewer with orphan markers
        app.run_slash("/transcript", &mut serve);
        let t1: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(t1["cmd"], "transcript");
        app.frame(json!({"type": "result", "id": t1["id"], "ok": true, "data": {"text": "⟲ user: lost future"}}), &mut serve);
        assert!(app.viewer.as_ref().unwrap().read_only);
        assert!(app.viewer.as_ref().unwrap().text.contains("lost future"));
        // export → chat line with the path
        app.run_slash("/export md", &mut serve);
        let t2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(t2["cmd"], "export");
        app.frame(json!({"type": "result", "id": t2["id"], "ok": true, "data": {"path": "/tmp/x.md", "format": "md"}}), &mut serve);
        assert!(app.lines.iter().any(|l| l.text.contains("/tmp/x.md")));
        // rename → command sent
        app.run_slash("/rename my-session", &mut serve);
        let t3: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(t3["cmd"], "rename");
        assert_eq!(t3["name"], "my-session");
        // recap → uses the btw streaming dialog
        app.run_slash("/recap", &mut serve);
        let t4: Value = serde_json::from_str(&serve.sent[3]).unwrap();
        assert_eq!(t4["cmd"], "recap");
        // copy with no assistant replies yet: no panic, nothing sent
        app.run_slash("/copy", &mut serve);
        assert_eq!(serve.sent.len(), 4);
    }

    #[test]
    fn context_command_opens_a_breakdown_viewer() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/context", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "context");
        app.frame(json!({
            "type": "result", "id": sent["id"], "ok": true,
            "data": {"system": 900, "skills": 120, "tools": 1500, "history": 3000, "total": 5520, "window": 20000, "pct": 27}
        }), &mut serve);
        let viewer = app.viewer.as_ref().unwrap();
        assert!(viewer.read_only);
        assert!(viewer.text.contains("system"));
        assert!(viewer.text.contains("history"));
        assert!(viewer.text.contains("27%"));
        assert!(viewer.text.contains("5520"));
    }

    #[test]
    fn queue_dialog_click_buttons_and_keyboard_paths() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        app.local_queue = vec![
            QueuedPrompt::text_only("第一问"),
            QueuedPrompt::text_only("第二问"),
            QueuedPrompt::text_only("第三问"),
        ];

        // /queue opens the clickable dialog; buttons are laid out at draw time
        app.run_slash("/queue", &mut serve);
        assert!(app.queue_dialog);
        {
            use ratatui::{Terminal, backend::TestBackend};
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        assert!(
            app.queue_buttons.len() >= 8,
            "2 per item + 2 footer buttons"
        );

        // click [删] on item 1 (find its Remove(0) rect)
        let (del0, _): (Rect, QueueAction) = app
            .queue_buttons
            .iter()
            .find(|(_, act)| *act == QueueAction::Remove(0))
            .cloned()
            .unwrap();
        let cx = del0.x + 2;
        let cy = del0.y;
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(MouseButton::Left),
                column: cx,
                row: cy,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert_eq!(
            app.local_queue,
            vec![
                QueuedPrompt::text_only("第二问"),
                QueuedPrompt::text_only("第三问")
            ]
        );
        {
            use ratatui::{Terminal, backend::TestBackend};
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        assert!(app.lines.iter().any(|l| l.text.contains("已移除排队 #1")));

        // click [并入] on (new) item 1 → supplement sent, item leaves the queue
        let (sup0, _): (Rect, QueueAction) = app
            .queue_buttons
            .iter()
            .find(|(_, act)| *act == QueueAction::Supplement(0))
            .cloned()
            .unwrap();
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(MouseButton::Left),
                column: sup0.x + 2,
                row: sup0.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert_eq!(app.local_queue, vec![QueuedPrompt::text_only("第三问")]);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "supplement");
        assert_eq!(sent["text"], "第二问");
        {
            use ratatui::{Terminal, backend::TestBackend};
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }

        // click [全部并入] → the rest rides the running turn, queue empties
        let (all, _): (Rect, QueueAction) = app
            .queue_buttons
            .iter()
            .find(|(_, act)| *act == QueueAction::SupplementAll)
            .cloned()
            .unwrap();
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(MouseButton::Left),
                column: all.x + 2,
                row: all.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert!(app.local_queue.is_empty());
        assert!(!app.queue_dialog, "an empty queue closes the dialog");
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["text"], "第三问");
        // queue more and reopen
        app.local_queue = vec![QueuedPrompt::text_only("x")];
        app.run_slash("/queue", &mut serve);
        {
            use ratatui::{Terminal, backend::TestBackend};
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }

        // click [清空]
        let (clr, _): (Rect, QueueAction) = app
            .queue_buttons
            .iter()
            .find(|(_, act)| *act == QueueAction::ClearAll)
            .cloned()
            .unwrap();
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(MouseButton::Left),
                column: clr.x + 2,
                row: clr.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert!(app.local_queue.is_empty());
        assert!(!app.queue_dialog, "clearing closes it too");

        // Esc closes while items remain
        app.local_queue = vec![QueuedPrompt::text_only("y")];
        app.run_slash("/queue", &mut serve);
        assert!(app.queue_dialog);
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut serve,
        );
        assert!(!app.queue_dialog);
    }
    #[test]
    fn queue_dialog_is_fully_keyboard_driven() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        app.local_queue = ["一", "二", "三", "四"].map(QueuedPrompt::text_only).to_vec();
        app.run_slash("/queue", &mut serve);
        let key = |app: &mut App, serve: &mut Serve, code| {
            app.key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE), serve)
        };
        key(&mut app, &mut serve, KeyCode::Down);
        key(&mut app, &mut serve, KeyCode::Char('d'));
        assert_eq!(app.local_queue, ["一", "三", "四"].map(QueuedPrompt::text_only).to_vec());
        assert!(app.input.is_empty(), "dialog keys never type into the input");
        key(&mut app, &mut serve, KeyCode::Char('3'));
        key(&mut app, &mut serve, KeyCode::Enter);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!((sent["cmd"].as_str(), sent["text"].as_str()), (Some("supplement"), Some("四")));
        assert_eq!(app.queue_sel, 1, "selection clamps after the last row leaves");
        key(&mut app, &mut serve, KeyCode::Up);
        key(&mut app, &mut serve, KeyCode::Up);
        assert_eq!(app.queue_sel, 0);
        assert!(app.queue_dialog, "items left: still open");
        key(&mut app, &mut serve, KeyCode::Char('a'));
        assert!(app.local_queue.is_empty());
        assert!(!app.queue_dialog, "merge all empties the queue and closes the dialog");
        // merging the last item one by one closes it as well
        app.local_queue = ["五", "六"].map(QueuedPrompt::text_only).to_vec();
        app.run_slash("/queue", &mut serve);
        key(&mut app, &mut serve, KeyCode::Enter);
        assert!(app.queue_dialog);
        key(&mut app, &mut serve, KeyCode::Enter);
        assert!(app.local_queue.is_empty());
        assert!(!app.queue_dialog);
        // q closes while items remain
        app.local_queue = vec![QueuedPrompt::text_only("x")];
        app.run_slash("/queue", &mut serve);
        key(&mut app, &mut serve, KeyCode::Char('q'));
        assert!(!app.queue_dialog);
    }

    #[test]
    fn usage_opens_a_report_viewer_and_insights_reports_path() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/usage", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "usage");
        app.frame(json!({
            "type": "result", "id": sent["id"], "ok": true,
            "data": {"totalCostUsd": 0.1234, "byModel": [
                {"model": "mock/mock", "requests": 7, "input": 1000, "output": 400, "costUsd": 0.12}
            ], "bySession": [
                {"id": "abcdef12", "name": "demo", "costUsd": 0.12, "messages": 9}
            ]}
        }), &mut serve);
        let viewer = app.viewer.as_ref().unwrap();
        assert!(viewer.read_only);
        assert!(viewer.text.contains("mock/mock"));
        assert!(viewer.text.contains("7"));
        assert!(viewer.text.contains("0.1234"));

        app.run_slash("/insights", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "insights");
        app.frame(json!({"type": "result", "id": sent2["id"], "ok": true, "data": {"path": "/tmp/ins.html"}}), &mut serve);
        assert!(app.lines.iter().any(|l| l.text.contains("/tmp/ins.html")));
    }

    #[test]
    fn timestamps_toggle_prefixes_lines_and_density_controls_tool_detail() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.push_tool(
            "⚙ bash echo hi",
            Tone::Muted,
            Some("line one\nline two\n".into()),
        );
        app.push_tool("⚙ bash bad", Tone::Warning, Some("boom\ntrace".into()));

        // density 1 (default): a failed call stays one line too, its output in the viewer
        let lines = app.chat_lines();
        assert!(
            lines.iter().all(|l| !l.text.contains("boom")),
            "no inline error block at density 1"
        );

        // density 0: compact — no output preview even for errors
        app.run_slash("/density 0", &mut serve);
        let lines = app.chat_lines();
        assert!(
            lines.iter().all(|l| !l.text.contains("boom")),
            "density 0 hides previews"
        );
        assert!(
            lines.iter().any(|l| l.text.starts_with("⚙")),
            "tool names stay"
        );

        // density 2: full — successful tools also show output
        app.run_slash("/density 2", &mut serve);
        let lines = app.chat_lines();
        assert!(
            lines.iter().any(|l| l.text.contains("line one")),
            "density 2 shows success output"
        );

        // back to default; timestamps off by default
        app.run_slash("/density", &mut serve);
        app.run_slash("/timestamps", &mut serve);
        let lines = app.chat_lines();
        assert!(
            lines.iter().any(|l| {
                let t = l.text.strip_prefix('[');
                t.is_some_and(|rest| rest.starts_with(char::is_numeric))
            }),
            "timestamps prefix [HH:MM"
        );
        assert!(app.lines.iter().any(|l| l.text.contains("时间戳")));
    }

    #[test]
    fn autocompact_shows_and_sets_threshold() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        // bare: reads current value from status-like result
        app.run_slash("/autocompact", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "status");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true, "data": {"autocompact": 0.5}}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.contains("50%")));

        app.run_slash("/autocompact 60%", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "set_autocompact");
        assert_eq!(sent2["value"], "60%");
        app.frame(
            json!({"type": "result", "id": sent2["id"], "ok": true, "data": {"autocompact": 0.6}}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.contains("60")));

        app.run_slash("/autocompact off", &mut serve);
        let sent3: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(sent3["value"], "off");
    }

    #[test]
    fn statusline_template_replaces_known_placeholders_and_off_restores() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.model = "m1".into();
        app.mode = "ask".into();
        app.session = "sess9".into();
        app.context_tokens = 1000;
        app.context_window = 4000;
        app.cost = 0.5;
        let default = app.status_segments_text();
        assert!(default.contains("sess9"), "no template: default status");
        assert!(!default.contains("m1"), "model moved to the input block: {default}");

        app.statusline = Some("{model}|{mode}|{pct}%|{unknown}".into());
        let text = app.status_segments_text();
        assert!(text.contains("m1|ask"), "placeholders replaced: {text}");
        assert!(text.contains("25%"), "pct computed: {text}");
        assert!(
            text.contains("{unknown}"),
            "unknown placeholders stay verbatim: {text}"
        );

        app.run_slash("/statusline off", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "set_statusline");
        assert_eq!(sent["template"], serde_json::Value::Null);
        app.run_slash("/statusline", &mut serve);
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("{model}") && l.text.contains("{ctx}"))
        );
    }

    #[test]
    fn preset_save_uses_current_input_and_picker_fills_without_sending() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.input = "固定开场白文本".chars().collect();
        app.cursor = app.input.len();
        app.run_slash("/preset save open", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "preset_save");
        assert_eq!(sent["name"], "open");
        assert_eq!(sent["text"], "固定开场白文本");
        // input is cleared after saving as a confirmation
        assert!(app.input.is_empty());

        // bare /preset opens a picker from preset_list
        app.run_slash("/preset", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "preset_list");
        app.frame(
            json!({
                "type": "result", "id": sent2["id"], "ok": true,
                "data": {"presets": [{"name": "open", "preview": "固定开场白文本"}]}
            }),
            &mut serve,
        );
        assert!(app.picker.as_ref().unwrap().kind == PickerKind::Preset);
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        let get: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(get["cmd"], "preset_get");
        app.frame(
            json!({
                "type": "result", "id": get["id"], "ok": true, "data": {"text": "固定开场白文本"}
            }),
            &mut serve,
        );
        // the preset fills the input but nothing is sent
        assert_eq!(app.input.iter().collect::<String>(), "固定开场白文本");
        assert_eq!(serve.sent.len(), 3);

        // /preset <name> fills directly
        app.run_slash("/preset open", &mut serve);
        let sent4: Value = serde_json::from_str(&serve.sent[3]).unwrap();
        assert_eq!(sent4["cmd"], "preset_get");
        assert_eq!(sent4["name"], "open");
    }

    #[test]
    fn timestamps_are_recorded_at_push_time_not_render_time() {
        let mut app = App::new();
        app.push("old line", Tone::Muted); // timestamps OFF: recorded ts still lands on the line
        app.timestamps = true;
        app.push("new line", Tone::Muted);
        let lines = app.chat_lines();
        let old = lines.iter().find(|l| l.text.contains("old line")).unwrap();
        let new = lines.iter().find(|l| l.text.contains("new line")).unwrap();
        // lines pushed before enabling show their TRUE recorded push time (not render time)
        assert!(
            old.text.starts_with('['),
            "recorded ts exists even if pushed while disabled"
        );
        assert!(
            new.text.starts_with('['),
            "line pushed while enabled gets a prefix"
        );
        // the prefix is stable across renders (recorded, not recomputed)
        let again = app.chat_lines();
        let new2 = again.iter().find(|l| l.text.contains("new line")).unwrap();
        assert_eq!(new.text, new2.text);
    }

    /// `/word` tokens in `text`: a slash followed by [a-z_-], at a token start
    fn slash_words(text: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let b = text.as_bytes();
        for (i, _) in text.match_indices('/') {
            let start_ok = i == 0 || matches!(b[i - 1], b'`' | b' ' | b'(' | b'"' | b'|');
            let word: String = text[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_lowercase() || *c == '_' || *c == '-')
                .collect();
            if start_ok && !word.is_empty() {
                out.insert(format!("/{word}"));
            }
        }
        out
    }

    /// commands run_slash actually dispatches: its `"/x" | "/y" =>` match arms
    fn handled_commands() -> BTreeSet<String> {
        let src = include_str!("app.rs");
        let body = src.split("    fn run_slash(").nth(1).unwrap();
        let body = body.split("\n    fn ").next().unwrap();
        body.lines()
            .map(str::trim)
            .filter(|l| l.starts_with("\"/") && l.contains("=>"))
            .flat_map(|l| slash_words(l.split("=>").next().unwrap()))
            .collect()
    }

    #[test]
    fn slash_commands_stay_in_sync_with_help_and_docs() {
        let handled = handled_commands();
        assert!(
            handled.len() > 40 && handled.contains("/help"),
            "parser broke: {handled:?}"
        );
        let fmt = |s: &BTreeSet<String>| s.iter().cloned().collect::<Vec<_>>().join(" ");

        // 1. /help + Tab-completion: every handled command (aliases too) is its own SLASH entry
        let slash: BTreeSet<String> = SLASH
            .iter()
            .map(|(c, _)| c.split(' ').next().unwrap().to_string())
            .collect();
        let missing: BTreeSet<_> = handled.difference(&slash).cloned().collect();
        let stale: BTreeSet<_> = slash.difference(&handled).cloned().collect();
        assert!(
            missing.is_empty(),
            "add to SLASH in tui/src/app.rs: {}",
            fmt(&missing)
        );
        assert!(
            stale.is_empty(),
            "remove from SLASH (no handler): {}",
            fmt(&stale)
        );

        // 2. handbook reference section: complete and nothing stale
        let handbook = include_str!("../../docs/handbook.md");
        let reference = handbook.split("## Slash-command reference").nth(1).unwrap();
        let firsts: BTreeSet<String> = reference
            .lines()
            .filter(|l| l.starts_with("| `/"))
            .flat_map(|l| slash_words(l.split(" | ").next().unwrap()))
            .collect();
        let missing: BTreeSet<_> = handled.difference(&firsts).cloned().collect();
        let stale: BTreeSet<_> = firsts.difference(&handled).cloned().collect();
        assert!(
            missing.is_empty(),
            "document in docs/handbook.md reference: {}",
            fmt(&missing)
        );
        assert!(
            stale.is_empty(),
            "remove from docs/handbook.md reference: {}",
            fmt(&stale)
        );

        // 3. README's command list: may be a subset, but must not name removed commands
        let readme = include_str!("../../README.md");
        let list = readme
            .split("TUI commands:")
            .nth(1)
            .unwrap()
            .split("\n\n")
            .next()
            .unwrap();
        let stale: BTreeSet<_> = slash_words(list).difference(&handled).cloned().collect();
        assert!(
            stale.is_empty(),
            "remove from README.md command list: {}",
            fmt(&stale)
        );
    }

    #[test]
    fn tab_completion_reaches_single_word_commands() {
        let mut app = App::new();
        app.input = "/ta".chars().collect();
        app.cursor = app.input.len();
        app.refresh_popup();
        let popup = app.popup.as_ref().expect("completion popup");
        assert!(
            popup.items.iter().any(|i| i == "/tasks"),
            "{:?}",
            popup.items
        );
    }

    #[test]
    fn import_routes_to_serve_and_reports_imported_and_skipped() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        // bad / missing source: usage, nothing sent
        app.run_slash("/import", &mut serve);
        app.run_slash("/import cursor", &mut serve);
        assert!(serve.sent.is_empty());
        assert!(app.lines.iter().any(|l| l.text.contains("usage: /import")));

        app.run_slash("/import claude-code", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "import");
        assert_eq!(sent["source"], "claude-code");
        assert!(sent.get("path").is_none());
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true, "data": {
                "imported": ["bash:git status", "path:src"],
                "skipped": [{"raw": "Dangerous(*)", "reason": "unknown tool"}]}}),
            &mut serve,
        );
        let all: String = app
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("已导入 2 条") && all.contains("+ bash:git status"),
            "{all}"
        );
        assert!(
            all.contains("Dangerous(*)") && all.contains("unknown tool"),
            "{all}"
        );

        // codex takes an optional config.toml path; other sources ignore extras
        app.run_slash("/import codex ./c.toml", &mut serve);
        let codex: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(codex["path"], "./c.toml");
        app.run_slash("/import grok extra", &mut serve);
        let grok: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert!(grok.get("path").is_none());
        app.frame(
            json!({"type": "result", "id": grok["id"], "ok": false, "error": "import grok: ENOENT"}),
            &mut serve,
        );
        assert!(
            app.lines
                .last()
                .unwrap()
                .text
                .contains("import grok: ENOENT")
        );
    }

    #[test]
    fn permissions_command_lists_adds_and_removes() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/permissions", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "permissions_list");
        app.frame(json!({
            "type": "result", "id": sent["id"], "ok": true,
            "data": {"allow": [{"rule": "bash:rm", "note": "allow bash commands with this prefix"}]}
        }), &mut serve);
        assert!(app.picker.as_ref().unwrap().kind == PickerKind::Permissions);
        assert!(
            app.picker
                .as_ref()
                .unwrap()
                .items
                .iter()
                .any(|i| i.id == "bash:rm")
        );
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut serve,
        );

        app.run_slash("/permissions add bash:git push", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "permissions_add");
        assert_eq!(sent2["rule"], "bash:git push");

        app.run_slash("/permissions rm bash:rm", &mut serve);
        let sent3: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(sent3["cmd"], "permissions_remove");
        assert_eq!(sent3["rule"], "bash:rm");
    }

    #[test]
    fn editor_uses_dollar_editor_and_multiline_switches_enter() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        unsafe {
            std::env::set_var("EDITOR", "cat");
            std::env::remove_var("VISUAL");
        }
        app.run_slash("/editor", &mut serve);
        let req = app.nvim_request.as_ref().expect("editor request queued");
        assert_eq!(req.editor.executable, "cat");
        assert_eq!(req.target, NvimTarget::Draft);
        unsafe {
            std::env::remove_var("EDITOR");
        }

        // /editor without EDITOR/VISUAL reports and keeps the draft
        app.nvim_request = None;
        app.run_slash("/editor", &mut serve);
        assert!(app.nvim_request.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("$EDITOR")));

        // /multiline: default Enter sends; after toggling it inserts a newline
        let mut app2 = App::new();
        app2.input = "hi".chars().collect();
        app2.cursor = 2;
        app2.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        assert_eq!(serve.sent.len(), 1, "default: Enter sends");

        app2.run_slash("/multiline", &mut serve);
        app2.input = "hi".chars().collect();
        app2.cursor = 2;
        app2.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        assert_eq!(serve.sent.len(), 1, "toggled: Enter does not send");
        assert_eq!(app2.input.iter().collect::<String>(), "hi\n");
    }

    #[test]
    fn a_leading_file_path_is_a_prompt_not_a_slash_command() {
        let dir = std::env::temp_dir().join("tango-path-prompt");
        let _ = std::fs::create_dir_all(dir.join("sub dir"));
        let file = dir.join("发文 手册.md");
        std::fs::write(&file, "x").unwrap();
        let f = file.to_string_lossy().to_string();
        let d = dir.to_string_lossy().to_string();

        // bare path → read it; path + text → "文件：" line then the text
        assert_eq!(file_prompt(&f).unwrap(), format!("文件：{f}\n请阅读这个文件，理解它的内容。"));
        assert_eq!(file_prompt(&format!("{f}  总结一下")).unwrap(), format!("文件：{f}\n总结一下"));
        // quoted and backslash-escaped (drag and drop) forms, and a directory
        assert_eq!(leading_path(&format!("'{f}' 看看")).unwrap().0, f);
        assert_eq!(leading_path(&f.replace(' ', "\\ ")).unwrap().0, f);
        assert!(file_prompt(&d).unwrap().starts_with(&format!("目录：{d}\n")));
        // real commands, missing paths and plain text are left alone
        assert!(file_prompt("/help").is_none());
        assert!(file_prompt("/model qwen").is_none());
        assert!(file_prompt("/no/such/file.md 看看").is_none());
        assert!(file_prompt("看看 /tmp").is_none(), "a path later in the text is not touched");

        // end to end: Enter sends a prompt, no "unknown command"
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.input = format!("{f} 总结一下").chars().collect();
        app.cursor = app.input.len();
        app.key(crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut serve);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["cmd"], "prompt");
        assert_eq!(sent["text"], format!("文件：{f}\n总结一下"));
        assert!(!app.lines.iter().any(|l| l.text.contains("unknown command")));

        // a path-looking command that does not exist gets a file-not-found message
        app.run_slash("/users/nobody/x.md", &mut serve);
        assert!(app.lines.iter().any(|l| l.text.contains("找不到文件或目录")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trust_request_asks_once_and_a_yes_restarts_serve() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.session_full = "sess-1".into();
        app.frame(
            json!({"type": "trust_request", "root": "/repo", "keys": ["hooks"],
            "files": ["/repo/.tango/config.json"]}),
            &mut serve,
        );
        assert!(app.picker.as_ref().unwrap().kind == PickerKind::Trust);
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("/repo/.tango/config.json"))
        );
        app.picker = None;
        app.run_slash("/trust yes", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "set_trust");
        assert_eq!(sent["trusted"], true);
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
            "data": {"trusted": true, "root": "/repo", "gated": ["hooks"], "reload": true}}),
            &mut serve,
        );
        assert_eq!(app.restart.as_ref().map(|r| r.0.as_str()), Some("sess-1"));
        assert!(app.quit);
    }

    #[test]
    fn trust_picker_enter_after_hello_sends_set_trust() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(json!({"type": "hello", "sessionId": "sess-9", "model": "m", "mode": "ask",
            "cwd": "/repo", "protocol": 1, "tools": []}), &mut serve);
        app.frame(json!({"type": "trust_request", "root": "/repo", "keys": ["hooks"], "files": []}), &mut serve);
        assert!(app.picker.as_ref().map(|p| p.kind) == Some(PickerKind::Trust), "picker open");
        let before = serve.sent.len();
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        let cmds: Vec<String> = serve.sent[before..].iter()
            .map(|x| serde_json::from_str::<Value>(x).unwrap()["cmd"].as_str().unwrap_or("").to_string())
            .collect();
        assert!(cmds.contains(&"set_trust".to_string()), "sent: {cmds:?}");
    }

    #[test]
    fn trust_status_without_gated_keys_does_not_ask() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/trust", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "trust_status");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
            "data": {"root": "/repo", "trusted": null, "gated": [], "line": "配置：全局 ✓ · 项目 ✓"}}), &mut serve);
        assert!(app.picker.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("全局 ✓")));
        app.run_slash("/trust maybe", &mut serve);
        assert_eq!(serve.sent.len(), 1, "bad argument: usage only");
    }

    #[test]
    fn config_keymap_cd_pwd_add_dir_wire_up() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/config", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "config_show");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
            "data": {"config": {"theme": "dark", "extraDirs": ["/x"]}, "path": "/home/config.json"}}), &mut serve);
        assert!(
            app.viewer
                .as_ref()
                .unwrap()
                .text
                .contains("/home/config.json")
        );
        app.run_slash("/keymap", &mut serve);
        assert!(app.viewer.as_ref().unwrap().text.contains("expandTools"));
        app.run_slash("/pwd", &mut serve);
        let sent3: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent3["cmd"], "pwd");
        app.frame(
            json!({"type": "result", "id": sent3["id"], "ok": true, "data": {"cwd": "/w"}}),
            &mut serve,
        );
        app.run_slash("/cd sub", &mut serve);
        let sent4: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(sent4["cmd"], "cd");
        assert_eq!(sent4["dir"], "sub");
        app.run_slash("/cd", &mut serve);
        let sent5: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent5["cmd"], "cwds");
    }

    #[test]
    fn missing_cwd_opens_a_picker_and_a_typed_path_switches() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(
            json!({"type": "cwd_missing", "cwd": "/gone", "dirs": ["/Users/lucas/tango", "/tmp"]}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.contains("工作目录不存在：/gone")));
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.kind, PickerKind::Cwd);
        assert_eq!(picker.items[0].id, "/Users/lucas/tango");
        assert_eq!(picker.items.last().unwrap().id, "__input__");
        app.accept_picker(&mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "cd");
        assert_eq!(sent["dir"], "/Users/lucas/tango");

        app.frame(
            json!({"type": "cwd_missing", "cwd": "/gone", "dirs": ["/tmp"]}),
            &mut serve,
        );
        app.picker.as_mut().unwrap().query = "/Users/lucas/other".into();
        app.accept_picker(&mut serve);
        let sent2: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent2["dir"], "/Users/lucas/other");

        app.frame(
            json!({"type": "cwd_missing", "cwd": "/gone", "dirs": ["/tmp"]}),
            &mut serve,
        );
        let last = app.picker.as_ref().unwrap().items.len() - 1;
        app.picker.as_mut().unwrap().sel = last;
        app.accept_picker(&mut serve);
        assert!(app.picker.is_none());
        assert_eq!(app.input.iter().collect::<String>(), "/cd ");
    }

    #[test]
    fn init_and_doctor_wire_up() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/init", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "init");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true, "data": {"path": "/w/AGENTS.md"}}), &mut serve);
        assert!(app.lines.iter().any(|l| l.text.contains("AGENTS.md")));

        app.run_slash("/doctor", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "doctor");
        app.frame(
            json!({"type": "result", "id": sent2["id"], "ok": true,
            "data": {"checks": [
                {"name": "git", "ok": true, "detail": "repo detected"},
                {"name": "model key", "ok": false, "detail": "no key"}
            ]}}),
            &mut serve,
        );
        let v = app.viewer.as_ref().unwrap();
        assert!(v.text.contains("✓"));
        assert!(v.text.contains("✗"));
        assert!(v.text.contains("no key"));
    }

    #[test]
    fn debug_zip_reports_path() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/debug-zip", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "debug_zip");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true, "data": {"path": "/w/d.zip", "format": "zip"}}), &mut serve);
        assert!(app.lines.iter().any(|l| l.text.contains("/w/d.zip")));
    }
    #[test]
    fn at_file_expansion_follows_the_serve_cwd_after_cd() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        // serve moved to /tmp-at/sub via hello; the file lives there, not in the TUI process cwd
        let base = std::env::temp_dir().join("tango-at-cd");
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("target.txt"), "content-from-sub\n").unwrap();
        app.frame(
            json!({"type": "hello", "sessionId": "1", "model": "m", "mode": "ask",
            "cwd": sub.to_str().unwrap(), "tools": []}),
            &mut serve,
        );
        app.input = "review @target.txt".chars().collect();
        app.cursor = app.input.len();
        app.send_prompt(&mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert!(
            sent["text"].to_string().contains("content-from-sub"),
            "@target.txt must resolve against the serve cwd, got: {}",
            sent["text"]
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn full_width_space_in_the_input_is_a_token_boundary_not_a_panic() {
        // a pasted HK01 headline: 「崛起　香港」 holds U+3000, whitespace 3 bytes wide
        let text = "23日，于承忠博士于香港01发文《来稿｜深圳靠「试错」崛起\u{3000}香港「敢拼」的勇气去了哪里？》，文章写到：";
        // bracketed paste: the whole text lands, then the popup refresh scans the token
        let mut app = App::new();
        app.insert_paste(text);
        app.refresh_popup();
        assert!(app.popup.is_none());
        // typed one char at a time (an IME commit): every key refreshes the popup as well
        let mut app = App::new();
        let mut serve = Serve::mock();
        for c in text.chars() {
            app.key(
                crossterm::event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
                &mut serve,
            );
        }
        let s: String = app.input.iter().collect();
        assert_eq!(s, text);
        assert_eq!(app.cursor, app.input.len());
        // after the full-width space the token is the rest of the line
        let (tok, start, end) = app.token_at_cursor();
        assert_eq!(&s[start..end], "香港「敢拼」的勇气去了哪里？》，文章写到：");
        assert_eq!(tok, s[start..end]);
        // before it, the token ends at the full-width space
        app.cursor = 5;
        let (tok, start, end) = app.token_at_cursor();
        assert_eq!(&s[start..end], "23日，于承忠博士于香港01发文《来稿｜深圳靠「试错」崛起");
        assert_eq!(tok, s[start..end]);
    }

    #[test]
    fn at_file_popup_after_multi_byte_whitespace_replaces_the_token() {
        let mut app = App::new();
        let base = std::env::temp_dir().join("tango-fw-space");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("target.txt"), "content\n").unwrap();
        app.serve_cwd = Some(base.clone());
        // an NBSP (2 bytes) earlier in the line, a full-width space right before the token
        let text = "深圳\u{00a0}靠「试错」崛起\u{3000}@targ";
        app.input = text.chars().collect();
        app.cursor = app.input.len();
        app.refresh_popup();
        let items = app.popup.as_ref().map(|p| p.items.clone());
        assert_eq!(items, Some(vec!["@target.txt".to_string()]));
        app.accept_popup();
        assert_eq!(
            app.input.iter().collect::<String>(),
            "深圳\u{00a0}靠「试错」崛起\u{3000}@target.txt"
        );
        assert_eq!(app.cursor, app.input.len());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn lsp_calls_summarize_op_path_line_and_symbol() {
        let s = |v: Value| App::call_summary(&v);
        assert_eq!(s(json!({"op": "definition", "path": "src/a.ts", "line": 12, "symbol": "add"})), "definition · src/a.ts:12 add");
        assert_eq!(s(json!({"op": "workspace_symbols", "path": "src/a.ts", "query": "Lsp"})), "workspace_symbols · src/a.ts Lsp");
        assert_eq!(s(json!({"op": "symbols", "path": "src/a.ts"})), "symbols · src/a.ts");
        assert_eq!(s(json!({"path": "src/a.ts"})), "src/a.ts", "other tools unchanged");
    }

    #[test]
    fn fuzzy_files_ranks_name_matches_before_path_matches() {
        let files: Vec<String> = [
            "src/core/tools/search.ts",
            "tests/search-tools.test.ts",
            "src/serve.ts",
            "docs/handbook.md",
            "src/core/context.ts",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // substring of the name first (shorter path wins), then subsequences
        assert_eq!(fuzzy_files("search", &files), vec!["src/core/tools/search.ts", "tests/search-tools.test.ts"]);
        assert_eq!(fuzzy_files("SRV", &files), vec!["src/serve.ts"], "case-insensitive subsequence of the name");
        assert_eq!(fuzzy_files("coctx", &files), vec!["src/core/context.ts"], "subsequence across directories");
        assert!(fuzzy_files("zzz", &files).is_empty());
        assert!(fuzzy_files("", &files).is_empty());
    }

    #[test]
    fn at_file_popup_finds_repo_files_by_fuzzy_name() {
        let base = std::env::temp_dir().join(format!("tango-fuzzy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src/deep/er")).unwrap();
        std::fs::write(base.join("src/deep/er/handler.rs"), "x").unwrap();
        std::fs::write(base.join("hand.txt"), "x").unwrap();
        std::fs::write(base.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(base.join("ignored")).unwrap();
        std::fs::write(base.join("ignored/handout.rs"), "x").unwrap();
        let ok = Command::new("git").args(["init", "-q"]).current_dir(&base).status().is_ok_and(|s| s.success());
        if !ok {
            return; // no git on this machine
        }
        let mut app = App::new();
        app.serve_cwd = Some(base.clone());
        app.input = "look at @hand".chars().collect();
        app.cursor = app.input.len();
        app.refresh_popup();
        let items = app.popup.as_ref().map(|p| p.items.clone()).unwrap_or_default();
        // the directory's own prefix match first, then the nested file; git-ignored files stay out
        assert_eq!(items, vec!["@hand.txt".to_string(), "@src/deep/er/handler.rs".to_string()]);
        app.popup.as_mut().unwrap().sel = 1;
        app.accept_popup();
        assert_eq!(app.input.iter().collect::<String>(), "look at @src/deep/er/handler.rs");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn shift_tab_cycles_modes_without_a_dialog() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.mode = "ask".into();
        let ask = app.mode_color();
        let shift_tab = || crossterm::event::KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        let sent = |serve: &Serve, i: usize| -> Value { serde_json::from_str(&serve.sent[i]).unwrap() };
        // ask → plan: plan goes on over the permission mode, at once, no picker
        app.key(shift_tab(), &mut serve);
        assert!(app.picker.is_none(), "no dialog");
        assert_eq!((sent(&serve, 0)["cmd"].as_str(), sent(&serve, 0)["on"].as_bool()), (Some("set_plan"), Some(true)));
        assert!(app.plan_mode);
        assert_eq!(app.mode, "ask", "plan leaves the permission mode alone");
        assert_ne!(app.mode_color(), ask, "plan has its own accent");
        // plan → confirm: plan off, then the mode
        app.key(shift_tab(), &mut serve);
        assert_eq!(sent(&serve, 1)["on"], false);
        assert_eq!((sent(&serve, 2)["cmd"].as_str(), sent(&serve, 2)["mode"].as_str()), (Some("set_mode"), Some("confirm")));
        assert!(!app.plan_mode);
        // confirm → readonly → ask: quick presses keep stepping before serve answers
        app.key(shift_tab(), &mut serve);
        app.key(shift_tab(), &mut serve);
        assert_eq!(sent(&serve, 3)["mode"], "readonly");
        assert_eq!(sent(&serve, 4)["mode"], "ask");
        assert_eq!(app.mode, "ask");
        assert_eq!(serve.sent.len(), 5);
        // auto is never stepped into; from auto the next press is plan
        app.mode = "auto".into();
        app.key(shift_tab(), &mut serve);
        assert_eq!(sent(&serve, 5)["cmd"], "set_plan");
        assert!(app.full_access.is_none(), "no full-access dialog either");
    }

    #[test]
    fn a_refused_mode_switch_resyncs_from_serve() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.mode = "ask".into();
        app.plan_mode = true;
        app.key(crossterm::event::KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &mut serve);
        let set_mode: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        app.frame(json!({"type": "result", "id": set_mode["id"], "ok": false, "error": "nope"}), &mut serve);
        let status: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(status["cmd"], "status");
        app.frame(
            json!({"type": "result", "id": status["id"], "ok": true, "data": {"mode": "ask", "plan": true}}),
            &mut serve,
        );
        assert_eq!((app.mode.as_str(), app.plan_mode), ("ask", true));
    }

    #[test]
    fn mode_accents_are_distinct_and_plan_wins() {
        for theme in [Theme::dark(), Theme::light()] {
            let c = |m: &str, p| theme.mode_color(m, p);
            let all = [c("ask", false), c("auto", false), c("readonly", false), c("ask", true)];
            for (i, a) in all.iter().enumerate() {
                for b in &all[i + 1..] {
                    assert_ne!(a, b);
                }
            }
            assert_eq!(c("auto", true), c("ask", true));
        }
        assert_ne!(Theme::dark().input_bg(), Theme::light().input_bg());
    }

    #[test]
    fn input_block_pads_and_shows_the_footer_only_on_tall_terminals() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut app = App::new();
        app.mode = "auto".into();
        app.model = "apikeyfan/claude-opus-5-5".into();
        app.serve_cwd = Some("/work/x".into());
        app.serve_branch = "tango/x".into();
        let text = |t: &Terminal<TestBackend>, y: u16| {
            let b = t.backend().buffer();
            (0..80).map(|x| b[(x, y)].symbol()).collect::<String>()
        };

        let mut tall = Terminal::new(TestBackend::new(80, 40)).unwrap();
        tall.draw(|f| app.draw(f)).unwrap();
        // rule, 1 text row, info row — no blank pads around the draft
        assert_eq!(app.input_area.height, 3);
        let top = app.input_area.y;
        assert_eq!(app.input_text_top(), top + 1);
        assert!(text(&tall, top + 2).starts_with("▌ auto · claude-opus-5-5 apikeyfan"));
        let footer = text(&tall, top + 3);
        assert!(footer.starts_with("/work/x:tango/x"), "footer: {footer}");
        assert!(footer.contains("shift+tab mode"), "footer: {footer}");
        let b = tall.backend().buffer();
        assert_eq!(b[(0, top + 1)].fg, app.theme.mode_color("auto", false));
        assert_eq!(b[(40, top + 1)].bg, app.theme.input_bg());

        let mut short = Terminal::new(TestBackend::new(80, 20)).unwrap();
        short.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.input_area.height, 3, "same block on short terminals");
        assert_eq!(app.input_area.y + 3 + 1, 20, "no footer under 24 rows");
    }

    #[test]
    fn plan_command_toggles_and_shows_badge() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/plan", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "set_plan");
        assert_eq!(sent["on"], true);
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true, "data": {"plan": true}}),
            &mut serve,
        );
        assert!(app.plan_mode);
        let text: String = app
            .input_info_line(80)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(text.starts_with("plan · "), "input block shows plan mode: {text}");
        app.run_slash("/plan", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["on"], false);
    }

    #[test]
    fn effort_command_sets_level() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/effort high", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "set_effort");
        assert_eq!(sent["level"], "high");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true, "data": {"effort": "high"}}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.contains("high")));
    }

    #[test]
    fn effort_completion_offers_what_the_current_model_takes() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        // until serve has said otherwise: the commonest levels, so the old "h → high" still works
        app.clear_input();
        type_str(&mut app, &mut serve, "/effort h");
        app.key(KeyCode::Tab.into(), &mut serve);
        assert_eq!(typed(&app), "/effort high");
        // GLM-5.4: low high max, high in use
        app.efforts = ["low", "high", "max"].iter().map(|s| s.to_string()).collect();
        app.effort = "high".into();
        app.clear_input();
        type_str(&mut app, &mut serve, "/effort ");
        let pop = app.popup.as_ref().unwrap();
        assert_eq!(pop.items, vec!["low", "high (当前)", "max", "off"]);
        assert_eq!(
            pop.descs,
            vec!["轻度推理", "增强推理", "最深推理", "不发参数 · 用服务商的默认"]
        );
        // a level this model does not take is not offered
        app.clear_input();
        type_str(&mut app, &mut serve, "/effort me");
        assert!(app.popup.is_none(), "medium is not a level of GLM-5.4");
        // (a lone "x" still fuzzy-matches "max": the subsequence rule every picker has)
        app.clear_input();
        type_str(&mut app, &mut serve, "/effort ul");
        assert!(app.popup.is_none(), "ultra is not one either");
        app.clear_input();
        type_str(&mut app, &mut serve, "/effort ma");
        app.key(KeyCode::Tab.into(), &mut serve);
        assert_eq!(typed(&app), "/effort max");
        // a model with nothing to choose: off is all there is
        app.efforts = vec![];
        app.clear_input();
        type_str(&mut app, &mut serve, "/effort l");
        assert!(app.popup.is_none());
        app.clear_input();
        type_str(&mut app, &mut serve, "/effort o");
        app.key(KeyCode::Tab.into(), &mut serve);
        assert_eq!(typed(&app), "/effort off");
    }

    #[test]
    fn effort_enter_runs_the_highlighted_level() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.efforts = ["low", "high", "max"].iter().map(|s| s.to_string()).collect();
        // "/effort h" + Enter: "h" is no level, the highlighted row (high) is what runs
        type_str(&mut app, &mut serve, "/effort h");
        app.key(KeyCode::Enter.into(), &mut serve);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!((sent["cmd"].as_str(), sent["level"].as_str()), (Some("set_effort"), Some("high")));
        assert!(app.popup.is_none() && typed(&app).is_empty());
        // the open menu: ↓ ↓ from "low" lands on "max" (low, high, max, off), Enter sets it
        type_str(&mut app, &mut serve, "/effort ");
        app.key(KeyCode::Down.into(), &mut serve);
        app.key(KeyCode::Down.into(), &mut serve);
        app.key(KeyCode::Enter.into(), &mut serve);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!((sent["cmd"].as_str(), sent["level"].as_str()), (Some("set_effort"), Some("max")));
        // /mode too
        type_str(&mut app, &mut serve, "/mode re");
        app.key(KeyCode::Enter.into(), &mut serve);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!((sent["cmd"].as_str(), sent["mode"].as_str()), (Some("set_mode"), Some("readonly")));
        // a level typed in full with the menu closed runs as typed
        type_str(&mut app, &mut serve, "/effort low");
        app.key(KeyCode::Enter.into(), &mut serve);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["level"], "low");
    }

    #[test]
    fn model_picker_sets_the_reasoning_level_with_the_arrows() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.model = "zhipu-plan/glm-5.4".into();
        app.effort = "high".into();
        app.remember_effort();
        // bare /model asks serve for the list; the answer carries each model's levels
        app.run_slash("/model", &mut serve);
        let asked: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        let models = json!({"type": "result", "id": asked["id"], "ok": true, "data": {"current": "zhipu-plan/glm-5.4", "models": [
            {"id": "zhipu-plan/glm-5.4", "platform": "zhipu-plan", "keyed": true, "contextWindow": 200000,
             "efforts": ["low", "high", "max"]},
            {"id": "apikeyfan/claude-opus-5-5", "platform": "apikeyfan", "keyed": true, "contextWindow": 200000,
             "efforts": ["low", "medium", "high", "xhigh", "max"]},
            {"id": "x/gemma", "platform": "x", "keyed": true, "contextWindow": 0, "efforts": []}
        ]}});
        app.frame(models, &mut serve);
        let shown = |app: &App, id: &str| app.picker.as_ref().unwrap().effort_shown(id).map(str::to_string);
        // the level in use beside the current model, off beside the others, nothing on a switch-only model
        assert_eq!(shown(&app, "zhipu-plan/glm-5.4"), Some("high".into()));
        assert_eq!(shown(&app, "apikeyfan/claude-opus-5-5"), Some("off".into()));
        assert_eq!(shown(&app, "x/gemma"), None);
        // → on the current row: high → max, and stays at max; ← ← ← back to off
        app.key(KeyCode::Right.into(), &mut serve);
        assert_eq!(shown(&app, "zhipu-plan/glm-5.4"), Some("max".into()));
        app.key(KeyCode::Right.into(), &mut serve);
        assert_eq!(shown(&app, "zhipu-plan/glm-5.4"), Some("max".into()));
        for _ in 0..3 {
            app.key(KeyCode::Left.into(), &mut serve);
        }
        assert_eq!(shown(&app, "zhipu-plan/glm-5.4"), Some("off".into()));
        // ↓ to Claude: → from off is its first level, ×3 its third (that model's own list)
        app.key(KeyCode::Down.into(), &mut serve);
        for _ in 0..3 {
            app.key(KeyCode::Right.into(), &mut serve);
        }
        assert_eq!(shown(&app, "apikeyfan/claude-opus-5-5"), Some("high".into()));
        // ↓ to a model without levels: the arrows do nothing; Enter switches without an effort
        app.key(KeyCode::Down.into(), &mut serve);
        app.key(KeyCode::Right.into(), &mut serve);
        assert_eq!(shown(&app, "x/gemma"), None);
        app.key(KeyCode::Enter.into(), &mut serve);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!((sent["cmd"].as_str(), sent["modelId"].as_str()), (Some("set_model"), Some("x/gemma")));
        assert!(sent.get("effort").is_none(), "nothing chosen beside it: no effort field");
    }

    #[test]
    fn model_picker_enter_switches_model_and_level_together_and_the_input_block_shows_it() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.model = "zhipu-plan/glm-5.4".into();
        app.efforts = ["low", "high", "max"].iter().map(|s| s.to_string()).collect();
        // bare /model asks serve for the list; the answer carries each model's levels
        app.run_slash("/model", &mut serve);
        let asked: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        app.frame(json!({"type": "result", "id": asked["id"], "ok": true, "data": {"current": "zhipu-plan/glm-5.4", "models": [
            {"id": "zhipu-plan/glm-5.4", "platform": "zhipu-plan", "keyed": true, "contextWindow": 200000,
             "efforts": ["low", "high", "max"]}
        ]}}), &mut serve);
        // → → from off: low, high
        for _ in 0..2 {
            app.key(KeyCode::Right.into(), &mut serve);
        }
        app.key(KeyCode::Enter.into(), &mut serve);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(
            (sent["cmd"].as_str(), sent["modelId"].as_str(), sent["effort"].as_str()),
            (Some("set_model"), Some("zhipu-plan/glm-5.4"), Some("high"))
        );
        assert!(app.picker.is_none());
        let info = |app: &App| {
            app.input_info_line(80).spans.iter().map(|s| s.content.to_string()).collect::<String>()
        };
        assert!(!info(&app).contains("effort"), "still off until serve answers: {}", info(&app));
        let answer = |data: Value| json!({"type": "result", "id": sent["id"], "ok": true, "data": data});
        app.frame(answer(json!({"model": "zhipu-plan/glm-5.4", "contextWindow": 200000,
                               "effort": "high", "efforts": ["low", "high", "max"]})), &mut serve);
        assert_eq!(app.effort, "high");
        assert!(info(&app).ends_with("glm-5.4 zhipu-plan · effort high"), "{}", info(&app));
        // kept for this model: the level is still there after an /effort round trip through serve
        app.run_slash("/effort max", &mut serve);
        let set: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        app.frame(json!({"type": "result", "id": set["id"], "ok": true, "data": {"effort": "max", "efforts": ["low", "high", "max"]}}), &mut serve);
        assert!(info(&app).ends_with(" · effort max"), "{}", info(&app));
        // …and gone once it is off
        app.run_slash("/effort off", &mut serve);
        let off: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        app.frame(json!({"type": "result", "id": off["id"], "ok": true,
                         "data": {"effort": "off", "efforts": ["low", "high", "max"]}}), &mut serve);
        assert!(app.effort_memory.is_empty());
        assert!(info(&app).ends_with("glm-5.4 zhipu-plan"), "{}", info(&app));
    }

    #[test]
    fn effort_levels_follow_the_model_and_a_dropped_level_is_announced() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.effort = "medium".into();
        // switching to a model without "medium": serve turns it off and says which was dropped
        app.run_slash("/model mock/glm", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "set_model");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
                   "data": {"model": "mock/glm", "contextWindow": 200, "effort": "off",
                            "efforts": ["low", "high", "max"], "effortDropped": "medium"}}),
            &mut serve,
        );
        assert_eq!(app.model, "mock/glm");
        assert_eq!(app.effort, "off");
        assert_eq!(app.efforts, vec!["low", "high", "max"]);
        let text = app.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        assert!(text.contains("推理力度 medium 对 mock/glm 不适用，已关闭 · 可选 low · high · max · off"), "{text}");
        // a level the model does not take: serve refuses it and the reason is shown
        app.run_slash("/effort medium", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!((sent["cmd"].as_str(), sent["level"].as_str()), (Some("set_effort"), Some("medium")));
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": false,
                   "error": "effort \"medium\": mock/glm takes low | high | max | off"}),
            &mut serve,
        );
        assert_eq!(app.effort, "off", "a refused level changes nothing");
        assert!(app.lines.iter().any(|l| l.text.contains("takes low | high | max | off")), "{text}");
        // bare /effort asks status: the level in use and what there is to pick
        app.run_slash("/effort", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(sent["cmd"], "status");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
                   "data": {"model": "mock/glm", "mode": "ask", "effort": "max", "efforts": ["low", "high", "max"]}}),
            &mut serve,
        );
        assert_eq!(app.effort, "max");
        assert!(app.lines.iter().any(|l| l.text == "推理力度：max · 可选 low · high · max · off"));
    }

    #[test]
    fn review_commands_send() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/review", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "review");
        app.run_slash("/security-review", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "security_review");
    }

    #[test]
    fn memory_and_remember_wire_up() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/remember 用户偏好中文", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "remember");
        assert_eq!(sent["text"], "用户偏好中文");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
            "data": {"remembered": "用户偏好中文", "path": "/h/AGENTS.md"}}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.contains("已记住")));

        app.run_slash("/memory", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "memory");
        app.frame(
            json!({"type": "result", "id": sent2["id"], "ok": true,
            "data": {"text": "- 用户偏好中文\n", "path": "/h/AGENTS.md"}}),
            &mut serve,
        );
        let v = app.viewer.as_ref().unwrap();
        assert!(v.text.contains("用户偏好中文"));
    }

    #[test]
    fn skills_command_opens_picker() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/skills", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "skills");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
            "data": {"skills": [
                {"name": "global-demo", "description": "全局演示", "path": "/h/skills/global-demo/SKILL.md", "scope": "global"},
                {"name": "proj-demo", "description": "项目", "path": "/w/.tango/skills/proj-demo/SKILL.md", "scope": "project"}
            ]}}), &mut serve);
        assert!(app.picker.as_ref().unwrap().kind == PickerKind::Skills);
        assert!(app.picker.as_ref().unwrap().items.len() == 2);
    }

    #[test]
    fn goal_commands_wire_up() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/goal 完成示例", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "goal");
        assert_eq!(sent["text"], "完成示例");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
            "data": {"goal": "完成示例", "maxRounds": 5}}),
            &mut serve,
        );
        assert!(app.goal_text.is_some());
        let spans = app.status_spans(200);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("goal"), "badge shows: {text}");
        app.run_slash("/goal off", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["text"], "off");
        app.frame(
            json!({"type": "result", "id": sent2["id"], "ok": true, "data": {"goal": null}}),
            &mut serve,
        );
        assert!(app.goal_text.is_none());
    }

    #[test]
    fn loop_command_parses_interval_and_prompt() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/loop 10m 检查构建", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "loop");
        assert_eq!(sent["everySec"], 600);
        assert_eq!(sent["prompt"], "检查构建");
        app.run_slash("/loop off", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["off"], true);
    }

    #[test]
    fn worktree_command_family() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/worktree", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "worktree_list");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
            "data": {"worktrees": [{"name": "exp1", "path": "/w/x", "branch": "tango/exp1", "dirty": 2, "createdAt": "t"}]}}), &mut serve);
        assert!(app.picker.as_ref().unwrap().kind == PickerKind::Worktree);
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut serve,
        );
        app.run_slash("/worktree new exp2", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "worktree_create");
        assert_eq!(sent2["name"], "exp2");
        app.run_slash("/worktree merge exp1", &mut serve);
        let sent3: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(sent3["cmd"], "worktree_merge");
        app.run_slash("/worktree rm exp1", &mut serve);
        let sent4: Value = serde_json::from_str(&serve.sent[3]).unwrap();
        assert_eq!(sent4["cmd"], "worktree_remove");
        // bare subcommands print usage instead of silently listing worktrees
        let sent_before = serve.sent.len();
        app.run_slash("/worktree new", &mut serve);
        assert_eq!(serve.sent.len(), sent_before, "bare new sends nothing");
        assert!(
            app.lines
                .last()
                .is_some_and(|l| l.text.contains("usage: /worktree new")),
            "bare new prints its usage"
        );
        app.run_slash("/worktree rm", &mut serve);
        app.run_slash("/worktree merge", &mut serve);
        assert_eq!(serve.sent.len(), sent_before, "bare rm/merge send nothing");
        assert!(app
            .lines
            .last()
            .is_some_and(|l| l.text.contains("usage: /worktree merge")));
    }

    #[test]
    fn bg_tasks_stop_wire_up() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/bg 后台检查", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "bg");
        assert_eq!(sent["prompt"], "后台检查");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true, "data": {"task": {"id": "abc", "status": "running"}}}), &mut serve);
        assert!(app.lines.iter().any(|l| l.text.contains("abc")));

        app.run_slash("/tasks", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "tasks");
        app.frame(
            json!({"type": "result", "id": sent2["id"], "ok": true, "data": {"tasks": [
                {"id": "abc", "status": "done", "model": "m", "prompt": "后台检查", "sessionId": "sess1"}
            ]}}), &mut serve);
        let v = app.viewer.as_ref().unwrap();
        assert!(v.text.contains("done"));
        assert!(v.text.contains("后台检查"));

        app.run_slash("/stop abc", &mut serve);
        let sent3: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(sent3["cmd"], "stop_task");
        assert_eq!(sent3["id"], "abc");
    }

    #[test]
    fn agents_show_and_set() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/agents", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "agents_show");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
                "data": {"roles": {"subagent": "mock/flash"}, "driver": "mock/mock"}}),
            &mut serve,
        );
        let v = app.viewer.as_ref().unwrap();
        assert!(v.text.contains("mock/flash"));
        assert!(v.text.contains("driver"));

        app.run_slash("/agents subagent mock/flash", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "agents_set");
        assert_eq!(sent2["role"], "subagent");
        assert_eq!(sent2["model"], "mock/flash");
    }

    #[test]
    fn subagent_command_sends() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        // the old name is not offered by completion but still works, same protocol command
        app.input = "/sub".chars().collect();
        app.cursor = app.input.len();
        app.refresh_popup();
        let offered = app.popup.as_ref().map(|p| p.items.clone()).unwrap_or_default();
        assert!(!offered.iter().any(|i| i == "/subtask"), "{offered:?}");
        app.input.clear();
        app.cursor = 0;
        app.popup = None;
        app.run_slash("/subtask 旧名", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "subtask");
        assert_eq!(sent["prompt"], "旧名");
        serve.sent.clear();
        app.run_slash("/subagent 修个测试", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "subtask");
        assert_eq!(sent["prompt"], "修个测试");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
                "data": {"task": {"id": "s1", "status": "running", "worktreeName": "bg-1"}}}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.contains("s1")));
        let tone_of = |app: &App, needle: &str| app.lines.iter().find(|l| l.text.contains(needle)).map(|l| l.tone);
        assert_eq!(tone_of(&app, "已派出"), Some(Tone::Subagent));
        // the write-back notice gets the subagent color; other notices stay muted
        app.frame(json!({"type": "notice", "text": "⑂ subagent bg-1 完成：3+2=5"}), &mut serve);
        app.frame(json!({"type": "notice", "text": "other status"}), &mut serve);
        assert_eq!(tone_of(&app, "3+2=5"), Some(Tone::Subagent));
        assert_eq!(tone_of(&app, "other status"), Some(Tone::Muted));
        // …and keeps it after /resume, while the main agent's replies stay plain text
        let mut app = App::new();
        app.load_history(&json!({"total": 3, "items": [
            {"kind": "user", "text": "问"},
            {"kind": "assistant", "text": "⑂ subagent bg-1 完成：3+2=5"},
            {"kind": "assistant", "text": "主代理回答"}
        ]}));
        assert_eq!(tone_of(&app, "3+2=5"), Some(Tone::Subagent));
        assert_ne!(tone_of(&app, "主代理回答"), Some(Tone::Subagent));
    }

    #[test]
    fn mcp_command_lists_servers() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/mcp", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "mcp_list");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
                "data": {"servers": [{"name": "demo", "connected": true}]}}),
            &mut serve,
        );
        let v = app.viewer.as_ref().unwrap();
        assert!(v.text.contains("demo"));
        assert!(v.text.contains("已连接"));
    }

    #[test]
    fn mcp_list_shows_addresses_errors_and_the_catalog() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/mcp", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true, "data": {
                "servers": [
                    {"name": "demo", "connected": true, "target": "node x.mjs --stdio"},
                    {"name": "github", "connected": false, "target": "https://api.githubcopilot.com/mcp/", "error": "POST …: HTTP 401 missing required Authorization header"}
                ],
                "catalog": [
                    {"id": "github", "description": "Repos, issues, pull requests", "auth": "a GitHub PAT in the environment: export GITHUB_PAT=…"},
                    {"id": "context7", "description": "Library docs", "auth": "none"}
                ]
            }}),
            &mut serve,
        );
        let v = app.viewer.as_ref().unwrap();
        assert!(v.text.contains("https://api.githubcopilot.com/mcp/"), "remote servers show their URL");
        assert!(v.text.contains("HTTP 401"), "and why they did not connect");
        assert!(v.text.contains("/mcp add context7") && v.text.contains("Library docs"));
        assert!(v.text.contains("需要：a GitHub PAT"), "what the user must provide");
        assert!(v.text.contains("✓ 已添加"), "the configured one is marked");
        assert!(!v.text.contains("/mcp add github\n  "), "…but still listed under its add line");
    }

    #[test]
    fn mcp_add_sends_the_name_and_tells_what_is_left() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        let chat_text = |a: &App| a.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        app.run_slash("/mcp add context7", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!((sent["cmd"].as_str(), sent["name"].as_str()), (Some("mcp_add"), Some("context7")));
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true, "data": {"added": "context7", "auth": "none"}}), &mut serve);
        assert!(chat_text(&app).contains("已添加 MCP context7"));
        assert!(chat_text(&app).contains("/restart"));
        app.run_slash("/mcp add github", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true, "data": {"added": "github", "auth": "a GitHub PAT in the environment: export GITHUB_PAT=…"}}), &mut serve);
        assert!(chat_text(&app).contains("还需要：a GitHub PAT"));
        // bare "add" and an unknown name don't blow up
        app.run_slash("/mcp add", &mut serve);
        assert!(chat_text(&app).contains("usage: /mcp add"));
        app.run_slash("/mcp add nope", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        app.frame(json!({"type": "result", "id": sent["id"], "ok": false, "error": "mcp_add: not in the catalog (context7, deepwiki, github, sentry)"}), &mut serve);
        assert!(chat_text(&app).contains("not in the catalog"));
    }

    #[test]
    fn plugins_command_lists() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/plugins", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "plugins_list");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
                "data": {"plugins": [{"name": "demo", "description": "测试", "enabled": true}]}}),
            &mut serve,
        );
        assert!(app.viewer.as_ref().unwrap().text.contains("demo"));
    }

    #[test]
    fn notice_frames_render_as_chat_lines() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(
            json!({"type": "notice", "text": "⚠ 另一个会话正在此目录运行"}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.contains("另一个会话")));
    }

    #[test]
    fn copy_confirmation_is_a_toast_not_a_chat_line_and_expires() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let before = app.lines.len();
        app.toast("已复制 66 个字符到剪贴板");
        assert_eq!(app.lines.len(), before, "nothing lands in the chat");
        let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        // wide chars occupy two cells (the second is blank): compare the symbols without spaces
        let row0: String = (0..60)
            .map(|x| term.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(
            row0.starts_with("      "),
            "right-aligned, not at the left: {row0:?}"
        );
        assert!(
            row0.replace(' ', "").ends_with("✓已复制66个字符到剪贴板"),
            "top-right: {row0:?}"
        );
        // expired: gone on the next draw
        app.toast = Some((
            "old".into(),
            std::time::Instant::now() - std::time::Duration::from_millis(TOAST_MS as u64 + 10),
        ));
        term.draw(|f| app.draw(f)).unwrap();
        let row0: String = (0..60)
            .map(|x| term.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(!row0.contains("old"));
        assert!(app.toast.is_none());
    }

    #[test]
    fn enter_runs_the_highlighted_command_of_the_completion() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        let key = |app: &mut App, serve: &mut Serve, c: KeyCode| app.key(c.into(), serve);
        // "/mo" matches /model, /mode and /mouse; Enter runs the first, as typed in full
        app.input = "/mo".chars().collect();
        app.cursor = app.input.len();
        app.refresh_popup();
        let p = app.popup.as_ref().unwrap();
        assert!(p.kind == PopKind::Slash);
        assert_eq!(p.sel, 0);
        assert_eq!(p.items[0].split(' ').next(), Some("/model"));
        key(&mut app, &mut serve, KeyCode::Enter);
        assert!(app.popup.is_none(), "the menu closes");
        assert!(app.input.is_empty(), "and the partial word went with it");
        assert!(
            serve.sent.iter().any(|l| l.contains("\"models\"")),
            "/model asked serve for the model list: {:?}",
            serve.sent
        );
        assert!(
            !serve.sent.iter().any(|l| l.contains("\"prompt\"")),
            "nothing was sent as a prompt"
        );
        assert!(
            !app.lines.iter().any(|l| l.text.contains("unknown command")),
            "/mo never ran as typed"
        );

        // ↓ first: Enter runs that row instead (bare /mode opens the mode picker)
        app.input = "/mo".chars().collect();
        app.cursor = app.input.len();
        app.refresh_popup();
        key(&mut app, &mut serve, KeyCode::Down);
        assert_eq!(app.popup.as_ref().unwrap().sel, 1);
        assert_eq!(
            app.popup.as_ref().unwrap().items[1].split(' ').next(),
            Some("/mode")
        );
        key(&mut app, &mut serve, KeyCode::Enter);
        assert!(
            app.picker
                .as_ref()
                .is_some_and(|p| p.kind == PickerKind::Mode),
            "the second row ran"
        );
        assert!(app.popup.is_none() && app.input.is_empty());

        // Tab still only completes: the input holds the whole word, nothing runs yet
        app.picker = None;
        app.input = "/mo".chars().collect();
        app.cursor = app.input.len();
        app.refresh_popup();
        key(&mut app, &mut serve, KeyCode::Tab);
        assert_eq!(app.input.iter().collect::<String>(), "/model ");
        assert!(app.popup.is_none());

        // an exact command runs as typed: the menu is not even open then
        app.clear_input();
        app.input = "/help".chars().collect();
        app.cursor = app.input.len();
        app.refresh_popup();
        assert!(app.popup.is_none());
        key(&mut app, &mut serve, KeyCode::Enter);
        assert!(
            app.lines.iter().any(|l| l.text.contains("/model")),
            "/help printed the command list"
        );

        // Esc still only closes
        app.input = "/mo".chars().collect();
        app.cursor = app.input.len();
        app.refresh_popup();
        key(&mut app, &mut serve, KeyCode::Esc);
        assert!(app.popup.is_none());
        assert_eq!(
            app.input.iter().collect::<String>(),
            "/mo",
            "the partial word stays to be edited"
        );
    }

    #[test]
    fn clicking_a_command_row_of_the_completion_runs_it() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.input = "/mo".chars().collect();
        app.cursor = app.input.len();
        app.refresh_popup();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        // the second drawn row is /mode: a click on it runs that one
        let row_y = app.popup_area.y + 1 + 1;
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 3,
                row: row_y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert!(
            app.picker
                .as_ref()
                .is_some_and(|p| p.kind == PickerKind::Mode)
        );
        assert!(app.popup.is_none() && app.input.is_empty());
    }

    #[test]
    fn a_bare_resume_launch_opens_the_session_picker_once_hello_arrives() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.start_picker = true;
        let hello = json!({"type": "hello", "session": "abc", "model": "m/m", "mode": "ask"});
        app.frame(hello.clone(), &mut serve);
        let asked: Vec<String> = serve
            .sent
            .iter()
            .filter(|l| l.contains("\"sessions\""))
            .cloned()
            .collect();
        assert_eq!(asked.len(), 1, "hello asked serve for the session list");
        assert!(!app.start_picker, "and only once");
        app.frame(hello, &mut serve);
        assert_eq!(
            serve
                .sent
                .iter()
                .filter(|l| l.contains("\"sessions\""))
                .count(),
            1
        );
        // the list's reply opens the picker, as /resume does
        let req: Value = serde_json::from_str(&asked[0]).unwrap();
        app.frame(
            json!({"type": "result", "id": req["id"], "ok": true, "data": {"sessions": [
                {"id": "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee", "createdAt": "2026-11-01T10:00", "ago": "昨天",
                 "branch": "", "messages": 4, "cwd": "/w", "name": "", "topic": "earlier", "current": false},
                {"id": "11111111-2222-3333-4444-555555555555", "createdAt": "2026-11-27T10:00", "ago": "刚刚",
                 "branch": "", "messages": 0, "cwd": "/w", "name": "", "topic": "", "current": true}
            ]}}),
            &mut serve,
        );
        assert!(app.picker.is_some(), "the session picker is open");
        // without the launch flag a hello opens nothing
        let mut plain = App::new();
        let mut serve2 = Serve::mock();
        plain.frame(
            json!({"type": "hello", "session": "abc", "model": "m/m", "mode": "ask"}),
            &mut serve2,
        );
        assert!(plain.picker.is_none());
        assert!(!serve2.sent.iter().any(|l| l.contains("\"sessions\"")));
    }

    #[test]
    fn history_lists_prompts_oldest_first_and_enter_jumps_to_the_chosen_one() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        // five prompts with long answers between them, so the chat scrolls well past a screen
        for i in 0..5 {
            app.push_user(&format!("第{i}问"));
            app.push((i as u8 as char).to_string().repeat(800), Tone::Text);
        }
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.scroll_back == 0, "starts at the bottom");
        assert!(
            app.chat_rows.len() > app.chat_area.height as usize * 2,
            "the chat really scrolls"
        );

        app.run_slash("/history", &mut serve);
        let p = app.popup.as_ref().expect("the list opens");
        assert!(p.kind == PopKind::History);
        assert_eq!(
            p.items,
            ["第0问", "第1问", "第2问", "第3问", "第4问"],
            "oldest first"
        );
        assert_eq!(p.sel, 4, "the newest is selected");
        term.draw(|f| app.draw(f)).unwrap();
        let screen = term.backend().to_string();
        assert!(screen.contains("输入历史"), "{screen}");
        assert!(screen.contains("第4问"), "the newest prompt is in the list");
        // 5 prompts, 6 rows: the list is not scrolled, so the oldest is in view as well
        assert!(screen.contains("第0问"), "{screen}");
        // the list sits where the slash completion does: right above the input rule
        assert!(app.popup_area.width == 80 && app.popup_area.height > 0);

        let key = |app: &mut App, serve: &mut Serve, c: KeyCode| app.key(c.into(), serve);
        // ↑ ×4: to the oldest, and no further
        for _ in 0..5 {
            key(&mut app, &mut serve, KeyCode::Up);
        }
        assert_eq!(
            app.popup.as_ref().unwrap().sel,
            0,
            "↑ moves to older prompts, not past the first"
        );
        key(&mut app, &mut serve, KeyCode::Down);
        assert_eq!(app.popup.as_ref().unwrap().sel, 1);
        key(&mut app, &mut serve, KeyCode::Enter);
        assert!(app.popup.is_none(), "Enter closes the list");
        assert!(app.scroll_back > 0, "the chat scrolled back to that prompt");
        // the chosen prompt's first row is the top row of the chat (the ↓ pill took a row)
        term.draw(|f| app.draw(f)).unwrap();
        let top_line = app.top_row_line();
        let prompt_lines: Vec<usize> = app
            .tone_rows()
            .iter()
            .enumerate()
            .filter(|(_, t)| **t == Tone::User)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(top_line, prompt_lines[1], "第1问 is at the top of the view");
        assert!(serve.sent.is_empty(), "jumping sends nothing");
        // jumping again from a scrolled chat lands just as exactly
        app.run_slash("/history", &mut serve);
        key(&mut app, &mut serve, KeyCode::Up);
        key(&mut app, &mut serve, KeyCode::Enter);
        term.draw(|f| app.draw(f)).unwrap();
        assert_eq!(
            app.top_row_line(),
            prompt_lines[3],
            "第3问 is at the top of the view"
        );

        // Esc only closes
        app.run_slash("/history", &mut serve);
        key(&mut app, &mut serve, KeyCode::Esc);
        assert!(app.popup.is_none());
        assert!(serve.sent.is_empty());
    }

    #[test]
    fn history_with_no_prompts_says_so_and_the_status_emoji_opens_the_list() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/history", &mut serve);
        assert!(app.popup.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("还没有输入记录")));

        app.push_user("唯一的一问");
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let status = app.status_segments_text();
        assert!(
            status.starts_with(" 🕘 "),
            "the emoji leads the status bar: {status}"
        );
        let area = app.history_area;
        assert_eq!(area.width, 4, "the emoji and its padding, two columns wide");
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x + 1,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        let p = app
            .popup
            .as_ref()
            .expect("a click on the emoji opens the list");
        assert!(p.kind == PopKind::History);
        assert_eq!(p.items, ["唯一的一问"]);
        // a click on a row jumps to it
        term.draw(|f| app.draw(f)).unwrap();
        let row_y = app.popup_area.y + 1;
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 4,
                row: row_y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert!(app.popup.is_none(), "the click on the row accepted it");
        // ASCII mode spells the emoji as a letter
        app.ascii = true;
        assert!(app.status_segments_text().starts_with(" H "));
    }

    #[test]
    fn a_narrow_status_bar_drops_the_history_emoji_before_the_cost() {
        let mut app = App::new();
        app.context_window = 1_000_000;
        app.context_tokens = 500_000;
        app.session = "abcdef123456".into();
        let wide = app.status_segments_text_at(60);
        assert!(wide.starts_with(" 🕘  $0.0000 "), "{wide}");
        let tight = app.status_segments_text_at(12);
        assert!(tight.contains("$0.0000"), "{tight}: the cost stays");
        assert!(
            !tight.contains('🕘'),
            "{tight}: the emoji is only a shortcut"
        );
    }

    fn diag_frame(file: &str, line: u64, severity: &str, message: &str) -> Value {
        json!({"type": "diagnostics", "file": file, "problems": [
            {"line": line, "col": 3, "severity": severity, "message": message, "code": "2322"}
        ]})
    }

    #[test]
    fn diagnostics_frames_feed_the_problems_list_and_the_status_badge() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        assert!(
            app.status_segments_text().find('✗').is_none(),
            "no badge before any diagnostics"
        );
        app.frame(diag_frame("src/b.ts", 4, "warning", "unused x"), &mut serve);
        app.frame(diag_frame("src/a.ts", 12, "error", "bad type"), &mut serve);
        // the newest list for a file replaces its older one
        app.frame(diag_frame("src/a.ts", 2, "error", "another"), &mut serve);
        let status = app.status_segments_text();
        assert!(status.contains(" ✗1 ⚠1 /problems "), "{status}");

        app.run_slash("/problems", &mut serve);
        assert!(app.problems_dialog);
        let rows: Vec<String> = app.problem_rows().into_iter().map(|(t, _)| t).collect();
        assert_eq!(rows.len(), 2, "files by name");
        assert!(
            rows[0].starts_with("src/a.ts:2:3  ✗ another"),
            "{}",
            rows[0]
        );
        assert!(
            rows[1].starts_with("src/b.ts:4:3  ⚠ unused x"),
            "{}",
            rows[1]
        );

        // an empty list drops its file; with the last one gone, so is the badge
        app.frame(
            json!({"type": "diagnostics", "file": "src/a.ts", "problems": []}),
            &mut serve,
        );
        assert_eq!(app.problem_count(), 1);
        app.frame(
            json!({"type": "diagnostics", "file": "src/b.ts", "problems": []}),
            &mut serve,
        );
        assert!(app.problems.is_empty());
        assert!(!app.status_segments_text().contains('✗'));
    }

    #[test]
    fn problems_dialog_enter_fills_a_fix_request_and_esc_only_closes() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(diag_frame("src/a.ts", 12, "error", "bad type"), &mut serve);
        app.frame(diag_frame("src/b.ts", 3, "error", "second"), &mut serve);
        app.run_slash("/problems", &mut serve);
        let key = |app: &mut App, serve: &mut Serve, c: KeyCode| app.key(c.into(), serve);
        key(&mut app, &mut serve, KeyCode::Down);
        key(&mut app, &mut serve, KeyCode::Enter);
        assert!(!app.problems_dialog);
        let draft: String = app.input.iter().collect();
        assert_eq!(draft, "请修复 @src/b.ts#3：second");
        // Esc closes without touching the draft; a closed list ignores its keys
        app.run_slash("/problems", &mut serve);
        key(&mut app, &mut serve, KeyCode::Esc);
        assert!(!app.problems_dialog);
        key(&mut app, &mut serve, KeyCode::Char('x'));
        assert_eq!(
            app.problem_count(),
            2,
            "x only clears while the list is open"
        );
        app.run_slash("/problems", &mut serve);
        key(&mut app, &mut serve, KeyCode::Char('x'));
        assert_eq!(app.problem_count(), 0, "x clears the list while it is open");
        assert!(!app.status_segments_text().contains('✗'));
        assert!(serve.sent.is_empty(), "nothing was sent to serve");
    }

    #[test]
    fn a_narrow_status_bar_keeps_the_error_count_of_the_problems_badge() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(diag_frame("src/a.ts", 12, "error", "bad type"), &mut serve);
        app.frame(diag_frame("src/b.ts", 3, "warning", "unused"), &mut serve);
        // 20 columns cannot hold " ✗1 ⚠1 /problems ": the badge keeps its error count only
        let narrow = app.status_segments_text_at(20);
        assert!(narrow.contains(" ✗1 "), "{narrow}");
        assert!(!narrow.contains("/problems"), "{narrow}");
        assert!(!narrow.contains('⚠'), "{narrow}");
        assert_eq!(
            app.status_segments_text_at(40)
                .contains(" ✗1 ⚠1 /problems "),
            true
        );
    }

    #[test]
    fn problems_badge_click_opens_the_list() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(diag_frame("src/a.ts", 12, "error", "bad type"), &mut serve);
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let area = app.problems_area;
        assert!(area.width > 0, "the badge is on the status bar");
        mouse(
            &mut app,
            &mut serve,
            MouseEventKind::Down(MouseButton::Left),
            area.x + 1,
            area.y,
        );
        assert!(app.problems_dialog);
        // the dialog is a modal: the screen behind it is dimmed, and its rows are clickable
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.modal_rect.is_some());
        let screen = term.backend().to_string();
        assert!(
            screen.contains("Problems (1)"),
            "the title counts the problems"
        );
        assert!(
            screen.contains("src/a.ts:12:3"),
            "the row names file, line and column"
        );
        assert!(screen.contains("bad type"), "and the server's message");
        assert!(
            screen.contains("enter 填入输入框"),
            "the hint row names the keys"
        );
        assert_eq!(app.problems_rows.len(), 1);
        let (row_rect, row) = app.problems_rows[0];
        assert_eq!(row, 0);
        // the wheel moves the selection instead of scrolling the chat
        let before = app.scroll_back;
        mouse(
            &mut app,
            &mut serve,
            MouseEventKind::ScrollDown,
            row_rect.x + 2,
            row_rect.y,
        );
        assert_eq!(app.problems_sel, 0, "one row: the selection cannot move");
        assert_eq!(app.scroll_back, before, "the chat behind did not scroll");
    }

    #[test]
    fn retry_frames_show_attempt_progress() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(
            json!({"type": "retry", "attempt": 1, "max": 3, "reason": "connection reset [ECONNRESET]",
                "delayMs": 1000, "text": "⟳ 请求失败（connection reset [ECONNRESET]），1 秒后第 1/3 次重试…"}),
            &mut serve,
        );
        app.frame(json!({"type": "retry", "attempt": 2, "max": 3, "reason": "HTTP 503"}), &mut serve);
        assert!(app.lines.iter().any(|l| l.text.contains("第 1/3 次重试")));
        assert!(app.lines.iter().any(|l| l.text.contains("retry 2/3: HTTP 503")));
    }

    #[test]
    fn a_modal_dialog_dims_the_rest_of_the_screen_in_both_themes() {
        use crate::dialog::DialogPalette;
        use ratatui::{Terminal, backend::TestBackend};
        for theme in [Theme::dark(), Theme::light()] {
            let mut app = App::new();
            app.theme = theme;
            app.permission = Some(Permission {
                request: 1,
                name: "bash".into(),
                detail: "git push origin main".into(),
                reason: "publishes to a remote".into(),
                brief: String::new(),
            });
            let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
            term.draw(|f| app.draw(f)).unwrap();
            let keep = app.modal_rect.expect("the permission dialog is modal");
            let p = DialogPalette::for_theme(theme);
            let b = term.backend().buffer();
            let mut undimmed = 0;
            let mut surface = 0;
            for y in 0..30u16 {
                for x in 0..100u16 {
                    let bg = b[(x, y)].bg;
                    if keep.contains(Position::new(x, y)) {
                        if bg == p.surface {
                            surface += 1;
                        }
                    } else if bg != p.scrim {
                        // the right half of a wide glyph (the 🕘 in the status bar) keeps
                        // the style of its left cell: the terminal paints both, and the
                        // frame diff never sends the blank cell after a wide symbol
                        let half_of_wide = x > 0 && display_width(b[(x - 1, y)].symbol()) == 2;
                        if !half_of_wide {
                            undimmed += 1;
                        }
                    }
                }
            }
            assert_eq!(undimmed, 0, "every cell outside the dialog is on the scrim");
            assert!(surface > 0, "the dialog keeps its own surface");
        }
    }

    #[test]
    fn nothing_is_dimmed_without_a_modal_and_not_behind_the_completion_popup() {
        use crate::dialog::DialogPalette;
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let scrim = DialogPalette::for_theme(app.theme).scrim;
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.modal_rect.is_none());
        assert_ne!(term.backend().buffer()[(0, 0)].bg, scrim);
        // typing "/mo" opens the command completion: a helper at the cursor, not a modal
        type_str(&mut app, &mut serve, "/mo");
        assert!(app.popup.is_some(), "the completion list is open");
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.modal_rect.is_none());
        assert_ne!(term.backend().buffer()[(0, 0)].bg, scrim, "the conversation stays as it is");
        // while the viewer, say, is a modal
        app.clear_input();
        app.popup = None;
        app.viewer = Some(Viewer::read_only("看看", "正文"));
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.modal_rect.is_some());
        assert_eq!(term.backend().buffer()[(0, 0)].bg, scrim);
    }

    #[test]
    fn every_dialog_hint_key_is_a_clickable_button() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let perm = |request| Permission {
            request,
            name: "bash".into(),
            detail: "rm -rf build".into(),
            reason: "deletes files".into(),
            brief: String::new(),
        };
        let click = |app: &mut App, serve: &mut Serve, r: Rect| {
            app.handle(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: r.x + r.width - 1, // the label end counts too
                    row: r.y,
                    modifiers: KeyModifiers::NONE,
                }),
                serve,
            );
        };
        let button = |app: &App, code: KeyCode| {
            app.hint_buttons.iter().find(|(_, k)| k.code == code).map(|(r, _)| *r)
        };
        // permission: "n 拒绝" denies, like pressing n
        app.permission = Some(perm(1));
        term.draw(|f| app.draw(f)).unwrap();
        for code in [KeyCode::Char('y'), KeyCode::Char('n'), KeyCode::Char('a'), KeyCode::Char('c')] {
            assert!(button(&app, code).is_some(), "{code:?} has a button");
        }
        let r = button(&app, KeyCode::Char('n')).unwrap();
        click(&mut app, &mut serve, r);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!((sent["request"].as_u64(), sent["allow"].as_bool()), (Some(1), Some(false)));
        assert!(app.permission.is_none());
        // "y 允许" allows
        app.permission = Some(perm(2));
        term.draw(|f| app.draw(f)).unwrap();
        let r = button(&app, KeyCode::Char('y')).unwrap();
        click(&mut app, &mut serve, r);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!((sent["request"].as_u64(), sent["allow"].as_bool()), (Some(2), Some(true)));
        // a viewer's "esc 关闭" closes it; with no dialog open nothing is clickable
        app.viewer = Some(Viewer::read_only("看看", "正文"));
        term.draw(|f| app.draw(f)).unwrap();
        let r = button(&app, KeyCode::Esc).unwrap();
        click(&mut app, &mut serve, r);
        assert!(app.viewer.is_none());
        term.draw(|f| app.draw(f)).unwrap();
        assert!(app.hint_buttons.is_empty());
        // pickers too: "esc 取消" closes the command palette
        app.open_command_palette();
        term.draw(|f| app.draw(f)).unwrap();
        let r = button(&app, KeyCode::Esc).unwrap();
        click(&mut app, &mut serve, r);
        assert!(app.picker.is_none());
    }

    #[test]
    fn permission_dialog_shows_brief_and_scrolls_full_command() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        let mut cmd = String::from("cat >> docs/handbook.md <<'EOF'");
        for i in 0..40 {
            cmd.push_str(&format!("\n第 {i} 行内容内容内容内容"));
        }
        cmd.push_str("\nEOF");
        app.permission = Some(Permission {
            request: 1,
            name: "bash".into(),
            detail: cmd,
            reason: "deletes files".into(),
            brief: "向 docs/handbook.md 追加 40 行文本（首行：第 0 行内容）".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let screen = |t: &mut Terminal<TestBackend>| -> String {
            t.backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol().to_string())
                .collect::<String>()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect()
        };
        let first = screen(&mut terminal);
        assert!(first.contains("追加40行文本"), "brief leads the dialog");
        assert!(first.contains("第0行"), "first command rows visible");
        assert!(!first.contains("第39行"), "tail hidden before scrolling");
        // ↓↓ scrolls to the end of the preview
        for _ in 0..40 {
            app.key(
                crossterm::event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                &mut serve,
            );
        }
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(
            screen(&mut terminal).contains("第39行"),
            "scrolling reveals the tail"
        );
    }

    #[test]
    fn queue_auto_append_diverts_busy_enter_to_supplement() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        // default: busy enter queues
        app.busy = true;
        app.input = "排队的话".chars().collect();
        app.key(
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                crossterm::event::KeyEventKind::Press,
            ),
            &mut serve,
        );
        assert_eq!(app.local_queue.len(), 1, "default queues");
        // enable append mode
        app.run_slash("/queue auto append", &mut serve);
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("auto append") && l.text.contains("已开启"))
        );
        app.busy = true;
        app.input = "并入当前回合".chars().collect();
        app.key(
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                crossterm::event::KeyEventKind::Press,
            ),
            &mut serve,
        );
        let sent: Value = serde_json::from_str(&serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["cmd"], "supplement");
        assert_eq!(sent["text"], "并入当前回合");
        assert!(app.local_queue.len() == 1, "nothing new queued");
        assert!(app.input.is_empty());
        // off restores queueing
        app.run_slash("/queue auto off", &mut serve);
        app.busy = true;
        app.input = "又一句".chars().collect();
        app.key(
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                crossterm::event::KeyEventKind::Press,
            ),
            &mut serve,
        );
        assert_eq!(app.local_queue.len(), 2, "off queues again");
    }

    #[test]
    fn clicking_a_user_bubble_opens_the_prompt_menu() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.push_user("帮我看看这个");
        app.chat_inner_w = 80;
        app.chat_scroll_offset = 0;
        app.chat_rows = wrap_chat_rows(&["❯ 帮我看看这个".to_string()], 80);
        {
            let mut terminal = Terminal::new(TestBackend::new(80, 19)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        // click (press+release, no drag) inside the bubble
        let x = app
            .chat_rows
            .iter()
            .find(|r| r.user && !r.border)
            .unwrap()
            .x as u16;
        app.chat_press(0, x + 3);
        app.chat_release(0, 0, x + 3);
        assert_eq!(app.prompt_menu, None, "single click does not open the menu");
        dblclick(&mut app, 0, x + 3);
        assert_eq!(app.prompt_menu, Some(0), "menu opens anchored to the line");
        {
            let mut terminal = Terminal::new(TestBackend::new(80, 19)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        assert_eq!(app.prompt_menu_buttons.len(), 4, "four clickable menu rows");
        let _ = serve;
    }

    #[test]
    fn prompt_menu_actions_copy_resend_quote_and_edit() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.push_user("第一行\n第二行");
        app.prompt_menu = Some(0);
        // 1 复制原文: ⏎ restored, ❯ stripped — the flattened form is what chat holds
        app.prompt_menu_copy(&mut serve);
        assert!(
            app.toast.as_ref().is_some_and(|(t, _)| t.contains("已复制") && t.contains("第一行")),
            "copy confirms with the original text"
        );
        // 3 原样重发: submits the restored original
        app.prompt_menu = Some(0);
        app.prompt_menu_resend(&mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "prompt");
        assert_eq!(sent["text"], "第一行\n第二行");
        // 4 引用: preloads a quote head into the input
        app.prompt_menu = Some(0);
        app.prompt_menu_quote();
        let typed: String = app.input.iter().collect();
        assert!(typed.starts_with("关于「第一行"), "{typed}");
        assert!(app.prompt_menu.is_none(), "actions close the menu");
        // 2 编辑重发 asks the timeline first, then editlast with that entry
        app.prompt_menu = Some(0);
        app.prompt_menu_edit(&mut serve);
        let sent: Value = serde_json::from_str(&serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["cmd"], "timeline");
        let tid = sent["id"].as_u64().unwrap();
        // the timeline reply carries the turn (matched by first line)…
        app.frame(
            json!({"type": "result", "id": tid, "ok": true, "data": {"nodes": [
                {"entryIndex": 3, "ts": "t", "firstLine": "第一行第二行", "hasSnapshot": false, "orphaned": false}
            ]}}),
            &mut serve,
        );
        // …editlast goes straight to that entry…
        let sent: Value = serde_json::from_str(&serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["cmd"], "editlast");
        assert_eq!(sent["entryIndex"], 3);
        let eid = sent["id"].as_u64().unwrap();
        // …and the rewound prompt comes back into the input box
        app.frame(
            json!({"type": "result", "id": eid, "ok": true,
                   "data": {"prompt": "第一行\n第二行", "turnDropped": 1}}),
            &mut serve,
        );
        let typed: String = app.input.iter().collect();
        assert_eq!(typed, "第一行\n第二行");
    }

    #[test]
    fn adjacent_bubble_top_border_renders_as_top() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let long = "请执行这条建议：这可以写成一篇问陈国基的评论，主线是「正日政府做什么，原址留什么」。动笔前先按 SOP 补核汶川纪念馆、防灾减灾日、克拉玛依遇难人数、上海胶州路这几项我凭记忆写的事实。";
        app.push_user("短消息");
        app.push_user(long);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        // find the screen row of the second bubble's first text row; the row
        // above must be a TOP border (╭…╮), not the bottom glyphs
        let rows: Vec<String> = (0..30)
            .map(|y| {
                terminal.backend().buffer().content[y * 120..(y + 1) * 120]
                    .iter()
                    .map(|c| c.symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        let text_y = rows
            .iter()
            .position(|r| r.replace(' ', "").contains("请执行这条建议"))
            .expect("second bubble text on screen");
        let above = &rows[text_y - 1];
        assert!(above.contains('╭'), "top border above the text: {above:?}");
        assert!(
            !above.contains('╰'),
            "no bottom corner on the top border: {above:?}"
        );
        // and every bubble still closes
        let bottom = &rows[text_y + 2];
        assert!(
            bottom.contains('╰'),
            "bottom border closes the frame: {bottom:?}"
        );
    }

    #[test]
    fn choice_panel_has_an_explain_row_that_asks_without_retiring() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.stream = "怎么处理？?choice\n1. 直接改\n2. 先看测试".into();
        app.commit_stream();
        {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        // the panel carries one extra built-in row beyond the model's options
        let explain_idx = app.choices.as_ref().unwrap().options.len();
        assert_eq!(
            app.choice_rows.len(),
            explain_idx + 1,
            "explain row rendered"
        );
        // picking it sends an explanation request and re-arms nothing yet…
        app.pick_choice(&mut serve, explain_idx);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "prompt");
        let text = sent["text"].as_str().unwrap().to_string();
        assert!(text.contains("解释"), "{text}");
        assert!(
            text.contains("?choice"),
            "the model is asked to re-present the block: {text}"
        );
        // …and the choice stays parked in todo for the re-armed panel
        assert_eq!(app.todos.len(), 1, "the ask is not answered by explaining");
    }

    #[test]
    fn alt_enter_supplements_a_running_turn_before_newline_eats_it() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        app.input = "补充说明".chars().collect();
        app.key(
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::ALT,
                crossterm::event::KeyEventKind::Press,
            ),
            &mut serve,
        );
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap_or(json!(null));
        assert_eq!(
            sent["cmd"], "supplement",
            "alt+enter while busy must supplement, not newline (default Newline binding is alt+enter)"
        );
        assert!(app.input.is_empty());
        // idle alt+enter stays a newline (the rebindable Action::Newline default)
        app.busy = false;
        app.input = "a".chars().collect();
        app.cursor = 1; // typing position, not the default 0
        app.key(
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::ALT,
                crossterm::event::KeyEventKind::Press,
            ),
            &mut serve,
        );
        let typed: String = app.input.iter().collect();
        assert_eq!(typed, "a\n", "idle alt+enter inserts a newline");
    }

    #[test]
    fn editlast_arms_confirms_and_preloads_the_original_prompt() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        // first press: confirmation notice, nothing sent (code rollback is destructive)
        app.key(
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('e'),
                KeyModifiers::ALT,
                crossterm::event::KeyEventKind::Press,
            ),
            &mut serve,
        );
        assert!(serve.sent.is_empty(), "first press only arms");
        assert!(app.editlast_armed);
        assert!(app.lines.iter().any(|l| l.text.contains("再按一次")));
        // a different key disarms
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut serve,
        );
        assert!(!app.editlast_armed, "esc cancels the arming");
        // second press after re-arming sends the command
        app.key(
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('e'),
                KeyModifiers::ALT,
                crossterm::event::KeyEventKind::Press,
            ),
            &mut serve,
        );
        app.key(
            crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('e'),
                KeyModifiers::ALT,
                crossterm::event::KeyEventKind::Press,
            ),
            &mut serve,
        );
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "editlast");
        let rid = sent["id"].as_u64().unwrap();
        // the rewound prompt comes back verbatim into the input box
        app.frame(
            json!({"type": "result", "id": rid, "ok": true,
                   "data": {"prompt": "原来的句子，含\n换行", "turnDropped": 1}}),
            &mut serve,
        );
        let typed: String = app.input.iter().collect();
        assert_eq!(typed, "原来的句子，含\n换行");
        assert!(!app.editlast_armed);
    }

    #[test]
    fn permission_c_copies_without_answering() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.permission = Some(Permission {
            request: 1,
            name: "bash".into(),
            detail: "rm -rf build/".into(),
            reason: "deletes files".into(),
            brief: "删除（递归+强制）：build/".into(),
        });
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            &mut serve,
        );
        assert!(app.permission.is_some(), "modal stays open after copy");
        assert!(app.toast.as_ref().is_some_and(|(t, _)| t.contains("命令已复制")));
        assert!(serve.sent.is_empty(), "no answer was sent");
    }

    #[test]
    fn find_existing_file_path_click_targets() {
        let dir = std::env::temp_dir().join("tango-click");
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real-file.txt");
        std::fs::write(&real, "x").unwrap();
        // found when the click lands on the path itself, with trailing punctuation
        // and a :line suffix tolerated
        let line = format!("see {} failed", real.display());
        let on_path = 4 + real.display().to_string().chars().count() / 2;
        assert_eq!(
            find_existing_file_path(&line, on_path),
            Some(real.display().to_string())
        );
        // click anywhere else on the line → no match, even though the path exists
        assert_eq!(find_existing_file_path(&line, 1), None);
        assert_eq!(
            find_existing_file_path(&line, line.chars().count()),
            None,
            "click past the end of the line"
        );
        // ~ expansion
        let home = std::env::var("HOME").unwrap_or_default();
        let homefile = std::fs::canonicalize(&real).ok(); // skip if not under home
        if homefile.is_none() {
            let hf = std::path::Path::new(&home).join("tango-click-home-probe.txt");
            std::fs::write(&hf, "x").unwrap();
            let l2 = format!("open ~/{} now", hf.file_name().unwrap().to_string_lossy());
            let mid = 6 + hf.file_name().unwrap().to_string_lossy().chars().count() / 2;
            assert!(
                find_existing_file_path(&l2, mid).is_some(),
                "home expansion works"
            );
        }
        // not a file → no match, no panic
        assert_eq!(find_existing_file_path("no paths here at all", 3), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrap_chat_rows_tracks_char_ranges() {
        let texts = vec![
            "short".to_string(),
            "alpha beta gamma delta".to_string(), // wraps at width 11
            "一很长的中文字符串没有任何空格所以只能硬切".to_string(), // CJK hard-split
        ];
        let rows = wrap_chat_rows(&texts, 11);
        // row 0 = whole short line
        assert_eq!(rows[0].text, "short");
        assert_eq!(rows[0].ce, 5);
        // line 1 word-wraps; every row's concatenation restores the full text
        let l1: String = rows
            .iter()
            .filter(|r| r.line == 1)
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(l1, "alpha beta gamma delta");
        // CJK hard-split also restores exactly
        let l2: String = rows
            .iter()
            .filter(|r| r.line == 2)
            .map(|r| r.text.as_str())
            .collect();
        assert_eq!(l2, texts[2]);
        // char ranges are contiguous and in-bounds
        let mut prev = 0usize;
        for r in rows.iter().filter(|r| r.line == 1) {
            assert_eq!(r.cs, prev);
            prev = r.ce;
        }
    }

    #[test]
    fn char_selection_copies_exact_dragged_text() {
        let mut app = App::new();
        app.chat_inner_w = 40;
        app.chat_scroll_offset = 0;
        app.chat_rows = wrap_chat_rows(
            &[
                "the quick brown fox jumps".to_string(),
                "second line here".to_string(),
            ],
            40,
        );
        // drag from row 0 col 4 to row 0 col 9 → exactly "quick"
        app.sel_anchor_pos = Some((0, 4));
        app.sel_end_pos = Some((0, 9));
        assert_eq!(app.selected_text(0).unwrap(), "quick");
        // across rows: row 0 col 20 → row 1 col 6 → "jumps" + "\n" + "second"
        app.sel_anchor_pos = Some((0, 20));
        app.sel_end_pos = Some((1, 6));
        assert_eq!(app.selected_text(0).unwrap(), "jumps\nsecond");
        // reverse drag normalizes
        app.sel_anchor_pos = Some((1, 6));
        app.sel_end_pos = Some((0, 20));
        assert_eq!(app.selected_text(0).unwrap(), "jumps\nsecond");
        // release clears and reports char count
        app.sel_anchor_pos = Some((0, 4));
        app.sel_end_pos = Some((0, 9));
        app.chat_release(0, 0, 9);
        assert!(app.sel_anchor_pos.is_none());
        assert!(app.toast.as_ref().is_some_and(|(t, _)| t.contains("已复制 5 个字符")));
    }

    #[test]
    fn chat_viewer_opens_only_on_release_over_path() {
        let mut app = App::new();
        let dir = std::env::temp_dir().join("tango-press");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("target.md");
        std::fs::write(&f, "CONTENT").unwrap();
        // mirror the real geometry: banner row 0, prompt row 1, width 200
        app.chat_inner_w = 200;
        app.chat_scroll_offset = 0;
        app.chat_rows = wrap_chat_rows(
            &[
                "tango serve banner".to_string(),
                format!("❯ 看下 {}", f.display()),
                "看到了文件内容".to_string(),
            ],
            200,
        );
        // press alone never opens the viewer — it starts a selection
        app.chat_press(1, 30);
        assert!(
            app.viewer.is_none(),
            "mouse down must not open the file viewer"
        );
        assert_eq!(app.sel_anchor_pos, Some((1, 30)));
        // a single click (release, no second press) still opens nothing
        app.chat_release(0, 1, 30);
        assert!(app.viewer.is_none(), "single click must not open the viewer");
        // double-click on the path → viewer opens, selection cleared
        dblclick(&mut app, 1, 30);
        let v = app.viewer.as_ref().expect("viewer opened on double-click");
        assert!(v.read_only);
        assert!(v.title.contains("target.md"));
        assert_eq!(v.text, "CONTENT");
        assert!(app.sel_anchor_pos.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chat_release_off_path_opens_no_viewer() {
        let mut app = App::new();
        let dir = std::env::temp_dir().join("tango-offpath");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("cfg.json");
        std::fs::write(&f, "{}").unwrap();
        app.chat_inner_w = 200;
        app.chat_scroll_offset = 0;
        app.chat_rows = wrap_chat_rows(&[format!("已改好：不动 {}", f.display())], 200);
        // click (press+release) on the leading text, not the path
        app.chat_press(0, 3);
        app.chat_release(0, 0, 3);
        assert!(
            app.viewer.is_none(),
            "release off the path must not open the viewer"
        );
        // even a double-click off the path opens nothing
        dblclick(&mut app, 0, 3);
        assert!(app.viewer.is_none(), "double-click off the path opens nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chat_drag_from_path_line_copies_instead_of_viewing() {
        let mut app = App::new();
        let dir = std::env::temp_dir().join("tango-drag");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("notes.txt");
        std::fs::write(&f, "N").unwrap();
        app.chat_inner_w = 200;
        app.chat_scroll_offset = 0;
        app.chat_rows = wrap_chat_rows(&[format!("看 {}", f.display())], 200);
        // start the drag on the path itself, move, release elsewhere → copy, no viewer
        app.chat_press(0, 4);
        app.chat_drag(0, 20);
        app.chat_release(0, 0, 20);
        assert!(app.viewer.is_none(), "a drag is a selection, not a click");
        assert!(
            app.toast.as_ref().is_some_and(|(t, _)| t.contains("已复制")),
            "dragged text was copied"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn program_status_reports_prompt_work_and_waits() {
        use crate::status::ProgramStatus;
        let mut app = App::new();
        assert_eq!(app.program_status(), ProgramStatus::Idle);
        app.busy = true;
        assert_eq!(app.program_status(), ProgramStatus::Working);
        app.permission = Some(Permission {
            request: 1,
            name: "bash".into(),
            detail: String::new(),
            reason: String::new(),
            brief: String::new(),
        });
        assert_eq!(
            app.program_status(),
            ProgramStatus::BlockedPermission,
            "a mid-turn permission dialog outranks busy"
        );
        app.permission = None;
        app.busy = false;
        app.stream = "?choice\n1. a\n2. b".into();
        app.commit_stream();
        assert_eq!(app.program_status(), ProgramStatus::BlockedQuestion);
        app.choices_collapsed = true;
        assert_eq!(
            app.program_status(),
            ProgramStatus::Idle,
            "a collapsed panel leaves a plain input box"
        );
    }

    #[test]
    fn choice_block_parses_into_panel_and_todo() {
        let mut app = App::new();
        app.stream = "修复方法如上。\n?choice\n1. 现在就改\n2. 先看测试再改\n3. 不改".into();
        app.commit_stream();
        let c = app.choices.as_ref().expect("choice panel armed");
        let labels: Vec<&str> = c.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, vec!["现在就改", "先看测试再改", "不改"]);
        assert!(!app.choices_collapsed, "panel visible on arrival");
        assert_eq!(app.todos.len(), 1, "unanswered choice also lands in todo");
        // the marker block never shows as raw chat text
        assert!(app.lines.iter().any(|l| l.text.contains("修复方法如上")));
        assert!(
            !app.lines.iter().any(|l| l.text.contains("?choice")),
            "marker stripped from chat"
        );
        assert!(
            !app.lines.iter().any(|l| l.text.contains("1. 现在就改")),
            "options live in the panel, not the transcript"
        );
    }

    #[test]
    fn choice_annotations_and_preview_parse() {
        let mut app = App::new();
        app.stream = "要去执行吗？?choice\n| cd ~/x && tools/run.sh\n| TV_HOST=h tools/run.sh 2>&1 | tail -20\n1. 执行\n   跑完直接提交\n2. 只跑不提交\n3. 不执行".into();
        app.commit_stream();
        let c = app.choices.as_ref().expect("choice panel armed");
        assert_eq!(
            c.preview,
            vec![
                "cd ~/x && tools/run.sh",
                "TV_HOST=h tools/run.sh 2>&1 | tail -20"
            ],
            "| lines become the bar-indented preview block"
        );
        assert_eq!(c.options[0].label, "执行");
        assert_eq!(
            c.options[0].note.as_deref(),
            Some("跑完直接提交"),
            "indented line annotates the option above"
        );
        assert_eq!(c.options[1].note, None, "annotations are per-option");
        // preview and annotations never leak into the transcript as raw text
        assert!(!app.lines.iter().any(|l| l.text.contains("| cd ~/x")));
        assert!(!app.lines.iter().any(|l| l.text.contains("跑完直接提交")));
    }

    #[test]
    fn tab_completes_slash_commands_while_a_turn_runs() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        for ch in "/mo".chars() {
            app.key(KeyCode::Char(ch).into(), &mut serve);
        }
        assert!(app.popup.is_some(), "the palette opens mid-turn too");
        app.key(KeyCode::Tab.into(), &mut serve);
        let typed: String = app.input.iter().collect();
        assert_eq!(typed, "/model ", "tab completes to the first match while busy");
    }

    fn typed(app: &App) -> String {
        app.input.iter().collect()
    }

    fn type_str(app: &mut App, serve: &mut Serve, s: &str) {
        for ch in s.chars() {
            app.key(KeyCode::Char(ch).into(), serve);
        }
    }

    #[test]
    fn tab_cycles_argument_candidates_zsh_style() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        type_str(&mut app, &mut serve, "/mode ");
        assert_eq!(app.popup.as_ref().unwrap().items.len(), 5, "all modes offered after the space");
        app.key(KeyCode::Tab.into(), &mut serve);
        assert_eq!(typed(&app), "/mode plan", "first tab inserts the first candidate");
        app.key(KeyCode::Tab.into(), &mut serve);
        assert_eq!(typed(&app), "/mode confirm", "next tab swaps in the next one");
        app.key(KeyCode::BackTab.into(), &mut serve);
        assert_eq!(typed(&app), "/mode plan", "shift+tab goes back");
        app.key(KeyCode::BackTab.into(), &mut serve);
        assert_eq!(typed(&app), "/mode readonly", "and wraps around");
        app.key(KeyCode::Esc.into(), &mut serve);
        assert!(app.popup.is_none());

        // one match: inserted and the menu closes
        app.clear_input();
        type_str(&mut app, &mut serve, "/mode a");
        let items = app.popup.as_ref().unwrap().items.clone();
        assert_eq!(items, vec!["ask", "auto"], "prefix filter");
        app.clear_input();
        type_str(&mut app, &mut serve, "/mode re");
        app.key(KeyCode::Tab.into(), &mut serve);
        assert_eq!(typed(&app), "/mode readonly");
        assert!(app.popup.is_none(), "a single candidate completes and closes");

        app.clear_input();
        type_str(&mut app, &mut serve, "/effort h");
        app.key(KeyCode::Tab.into(), &mut serve);
        assert_eq!(typed(&app), "/effort high");
    }

    #[test]
    fn model_send_and_resume_complete_from_quiet_fetches() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.arg_models = vec!["zhipu-plan/glm-5.3".into(), "zhipu-plan/glm-5.4".into(), "apikeyfan/claude-opus-5-5".into()];
        app.arg_sessions = vec![
            ("c81e407d-1111-4222-8333-aaaaaaaaaaaa".into(), "1 小时前 · 信箱".into(), true),
            ("d7d868f3-1111-4222-8333-bbbbbbbbbbbb".into(), "2 小时前 · 投稿".into(), false),
        ];
        type_str(&mut app, &mut serve, "/model glm54");
        app.key(KeyCode::Tab.into(), &mut serve);
        assert_eq!(typed(&app), "/model zhipu-plan/glm-5.4", "subsequence match like the picker");

        app.clear_input();
        type_str(&mut app, &mut serve, "/send ");
        assert_eq!(app.popup.as_ref().unwrap().items, vec!["bbbbbbbbbbbb"], "the current session is never a target");
        app.key(KeyCode::Tab.into(), &mut serve);
        assert_eq!(typed(&app), "/send bbbbbbbbbbbb ", "a space follows: the message comes next");
        assert!(app.popup.is_none());

        app.clear_input();
        type_str(&mut app, &mut serve, "/resume bb");
        app.key(KeyCode::Tab.into(), &mut serve);
        assert_eq!(typed(&app), "/resume bbbbbbbbbbbb");

        // the frames that fill the caches
        app.pending.push((7, Pending::ArgSessions));
        app.frame(
            serde_json::json!({"type":"result","id":7,"ok":true,"data":{"sessions":[
                {"id":"e1","name":"","topic":"t","ago":"刚刚","current":false}]}}),
            &mut serve,
        );
        assert_eq!(app.arg_sessions, vec![("e1".to_string(), "刚刚 · t".to_string(), false)]);
        assert!(app.lines.iter().all(|l| !l.text.contains("e1")), "quiet: nothing printed");
    }

    #[test]
    fn tab_amends_choice_into_input() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.stream = "要改吗？\n?choice\n1. 现在就改\n2. 不改".into();
        app.commit_stream();
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &mut serve,
        );
        let typed: String = app.input.iter().collect();
        assert_eq!(
            typed, "关于要改吗，Tango 在等你选择: ",
            "tab preloads the ask as a lead-in ending in \": \" for the user's own answer"
        );
        assert_eq!(
            app.cursor, app.input.len(),
            "the cursor sits after the lead-in, ready for the answer"
        );
        assert!(
            app.choices_collapsed,
            "panel steps aside while the user writes"
        );
        assert!(
            app.choices.is_some() && app.todos.len() == 1,
            "the ask itself is not answered by amending"
        );
    }

    #[test]
    fn tab_lead_in_follows_the_conversations_language() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        // an English conversation: the ask and Tab's lead-in are English too
        app.push_user("Refactor the parser and add tests");
        app.stream = "Ship it now?\n?choice\n1. Yes\n2. No".into();
        app.commit_stream();
        assert_eq!(
            app.choices.as_ref().unwrap().question, "Ship it now?",
            "the model's own question is kept as written"
        );
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &mut serve,
        );
        let typed: String = app.input.iter().collect();
        assert_eq!(typed, "About: Ship it now Tango is waiting for your choice: ");

        // no question before ?choice: a stand-in label in the conversation's language
        let mut app = App::new();
        app.push_user("Deploy next?");
        app.stream = "?choice\n1. staging\n2. production".into();
        app.commit_stream();
        assert_eq!(
            app.choices.as_ref().unwrap().question,
            "Tango is waiting for your choice"
        );
        assert_eq!(app.choice_title(), "❓ Tango is waiting for your choice");
        app.ascii = true;
        assert_eq!(app.choice_title(), "? Tango is waiting for your choice");
        let typed = App::amend_lead(&app.choices.as_ref().unwrap().question, app.chat_language());
        assert_eq!(typed, "Tango is waiting for your choice: ");

        // Chinese again once the user writes Chinese
        let mut app = App::new();
        app.push_user("开始吧");
        app.stream = "?choice\n1. 好\n2. 不了".into();
        app.commit_stream();
        assert_eq!(app.choice_title(), "❓ Tango 在等你选择");
        assert_eq!(App::amend_lead("Tango 在等你选择", app.chat_language()), "Tango 在等你选择: ");

        // no user prompt at all: the default wording
        let app = App::new();
        assert_eq!(App::amend_lead("", app.chat_language()), "Tango 在等你选择: ");
    }

    #[test]
    fn a_tab_lead_in_is_not_sent_only_the_answer_is() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.push_user("重构解析器");
        app.stream = "现在就发吗？\n?choice\n1. 发\n2. 不发".into();
        app.commit_stream();
        app.key(KeyCode::Tab.into(), &mut serve);
        // Enter with the lead-in alone: nothing goes out
        app.key(KeyCode::Enter.into(), &mut serve);
        assert!(
            !serve.sent.iter().any(|m| m.contains("\"cmd\":\"prompt\"") || m.contains("\"cmd\": \"prompt\"")),
            "an empty answer is not sent: {:?}",
            serve.sent
        );
        assert!(app.choices.is_some(), "the ask stays open");
        // with an answer: the model gets the answer alone
        for c in "可以，发吧".chars() {
            app.key(KeyCode::Char(c).into(), &mut serve);
        }
        let typed: String = app.input.iter().collect();
        assert_eq!(typed, "关于现在就发吗，Tango 在等你选择: 可以，发吧");
        app.key(KeyCode::Enter.into(), &mut serve);
        let sent: String = serve.sent.concat();
        assert!(sent.contains("可以，发吧"), "the answer goes out: {sent}");
        assert!(!sent.contains("Tango 在等你选择"), "the lead-in stays in the box: {sent}");
        assert!(!sent.contains("关于现在就发吗"), "the lead-in stays in the box: {sent}");
        // an answer that repeats the phrase is cut at the first one, not the last
        assert_eq!(
            App::split_tab_lead("关于发吗，Tango 在等你选择: 写一句 Tango 在等你选择: 当例子"),
            ("写一句 Tango 在等你选择: 当例子", true)
        );
    }

    #[test]
    fn chat_language_tells_scripts_apart() {
        assert_eq!(ChatLang::of("要改吗？"), Some(ChatLang::Zh));
        assert_eq!(ChatLang::of("Ship it now?"), Some(ChatLang::En));
        assert_eq!(ChatLang::of("今すぐ出しますか？"), Some(ChatLang::Ja));
        assert_eq!(ChatLang::of("바로 배포할까요?"), Some(ChatLang::Ko));
        assert_eq!(ChatLang::of("déploiement bientôt ?"), Some(ChatLang::En));
        assert_eq!(ChatLang::of("Czy wdrożyć?"), Some(ChatLang::En));
        assert_eq!(ChatLang::of("3"), None, "no letters to tell by");
        assert_eq!(ChatLang::of("❯ 2"), None);
        // the TUI's own lead-in does not pin the language: it is stripped first
        let mut app = App::new();
        app.push_user("Refactor the parser");
        app.push_user("About: Ship it now Tango is waiting for your choice: looks good");
        assert_eq!(app.chat_language(), ChatLang::En);
        let mut app = App::new();
        app.push_user("重构解析器");
        app.push_user("关于现在就发吗，Tango 在等你选择: 可以");
        assert_eq!(app.chat_language(), ChatLang::Zh);
    }

    #[test]
    fn suggest_line_lands_in_todo_and_renders_clean() {
        let mut app = App::new();
        app.stream = "主体已完成。?suggest 之后顺手补个回归测试\n继续下一步。".into();
        app.commit_stream();
        assert!(app.choices.is_none(), "a suggestion is not a blocking ask");
        assert_eq!(app.todos.len(), 1);
        assert!(matches!(
            &app.todos[0].kind,
            TodoKind::Suggest { text } if text == "之后顺手补个回归测试"
        ));
        assert!(
            !app.lines.iter().any(|l| l.text.contains("?suggest")),
            "marker stripped from chat"
        );
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("之后顺手补个回归测试")),
            "suggestion text still visible in context"
        );
    }

    #[test]
    fn picking_choice_sends_prompt_and_clears_todo() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.stream = "要改吗？\n?choice\n1. 现在就改\n2. 不改".into();
        app.commit_stream();
        app.pick_choice(&mut serve, 0);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "prompt");
        assert_eq!(sent["text"], "现在就改");
        assert!(app.choices.is_none(), "panel retires once answered");
        assert!(app.todos.is_empty(), "answered choice leaves the todo list");
    }

    #[test]
    fn typed_option_number_plus_enter_picks_it() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        let enter = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.stream = "要改吗？\n?choice\n1. 现在就改\n2. 不改\n3. 先不动".into();
        app.commit_stream();
        for ch in "3".chars() {
            app.key(
                crossterm::event::KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &mut serve,
            );
        }
        app.key(enter, &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(
            sent["text"], "先不动",
            "the option label goes out, not \"3\""
        );
        assert!(app.choices.is_none() && app.input.is_empty());

        // 4 = the built-in explain row; it asks for a walkthrough
        app.busy = false; // the turn the pick started has ended
        app.stream = "?choice\n1. a\n2. b\n3. c".into();
        app.commit_stream();
        app.input = "4".chars().collect();
        app.cursor = 1;
        app.key(enter, &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert!(sent["text"].as_str().unwrap().starts_with("请逐项详细解释"));

        // out of range, or more than a number: an ordinary prompt
        for text in ["9", "3 个文件"] {
            app.busy = false;
            app.stream = "?choice\n1. a\n2. b".into();
            app.commit_stream();
            app.input = text.chars().collect();
            app.cursor = app.input.len();
            app.key(enter, &mut serve);
            let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
            assert_eq!(sent["text"], text);
        }
    }

    #[test]
    fn alt_arrows_hop_between_user_prompts() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        for (q, a) in [("第一问", "答一"), ("第二问", "答二"), ("第三问", "答三")] {
            app.push_user(q);
            for c in ["a", "b", "c"] {
                app.push(format!("{a}{c}"), Tone::Text);
            }
        }
        app.chat_inner_w = 40;
        let texts: Vec<String> = app.chat_lines().iter().map(|l| l.text.clone()).collect();
        let user_flags: Vec<bool> = app.tone_rows().iter().map(|t| *t == Tone::User).collect();
        let think_flags: Vec<bool> = app.tone_rows().iter().map(|t| *t == Tone::Thinking).collect();
        app.chat_rows = wrap_chat_rows_mixed(&texts, 40, &user_flags, &think_flags, &[], app.user_bubble);
        // 12 rows: p1 g0, ans g1-3, p2 g4, ans g5-7, p3 g8, ans g9-11
        app.chat_area = Rect::new(0, 0, 40, 4);
        let alt_up =
            crossterm::event::KeyEvent::new(KeyCode::Up, KeyModifiers::ALT);
        let alt_down =
            crossterm::event::KeyEvent::new(KeyCode::Down, KeyModifiers::ALT);
        // from the bottom (showing prompt 3's block): hop back lands prompt 2
        // with its first row just under the header
        app.key(alt_up, &mut serve);
        assert_eq!(app.scroll_back, 5, "prompt 2 at the top of the view");
        // then prompt 1; a third press stays (oldest)
        app.key(alt_up, &mut serve);
        assert_eq!(app.scroll_back, 8);
        app.key(alt_up, &mut serve);
        assert_eq!(app.scroll_back, 8, "already the oldest prompt");
        // forward hops: prompt 2, prompt 3, then back to the bottom
        app.key(alt_down, &mut serve);
        assert_eq!(app.scroll_back, 5);
        app.key(alt_down, &mut serve);
        assert_eq!(app.scroll_back, 1);
        app.key(alt_down, &mut serve);
        assert_eq!(app.scroll_back, 0, "no prompt below → back to the bottom");
    }

    #[test]
    fn resumed_session_backfills_the_prompt_history_for_recall() {
        let mut app = App::new();
        app.load_history(&json!({"total": 3, "items": [
            {"kind": "user", "text": "第一问"},
            {"kind": "assistant", "text": "答"},
            {"kind": "user", "text": "再问"}
        ]}));
        assert_eq!(app.prompt_history, vec!["第一问", "再问"]);
        // ↑ recalls the newest, then the oldest — same as after a live session
        assert!(app.history_up());
        assert_eq!(app.input.iter().collect::<String>(), "再问");
        assert!(app.history_up());
        assert_eq!(app.input.iter().collect::<String>(), "第一问");
        // consecutive identical prompts dedupe like a live send
        let mut app = App::new();
        app.load_history(&json!({"total": 2, "items": [
            {"kind": "user", "text": "继续"},
            {"kind": "user", "text": "继续"}
        ]}));
        assert_eq!(app.prompt_history, vec!["继续"]);
    }

    #[test]
    fn resumed_history_only_revives_unanswered_choices_and_suggestions() {
        let mut app = App::new();
        let ask = |q: &str| json!({"kind": "assistant", "text": format!("{q}\n?choice\n1. a\n2. b\n?suggest idea {q}")});
        app.load_history(&json!({"total": 4, "items": [
            {"kind": "user", "text": "q1"}, ask("old"), {"kind": "user", "text": "a"}, {"kind": "assistant", "text": "ok"}
        ]}));
        assert!(
            app.choices.is_none(),
            "an answered choice doesn't reopen on resume"
        );
        assert!(
            app.todos.is_empty(),
            "answered choice / old suggestion stay out of todo"
        );

        let mut app = App::new();
        app.load_history(&json!({"total": 4, "items": [
            {"kind": "user", "text": "q1"}, ask("old"), {"kind": "user", "text": "a"}, ask("new")
        ]}));
        assert_eq!(
            app.choices.as_ref().map(|c| c.question.as_str()),
            Some("new")
        );
        assert_eq!(
            app.todos.len(),
            2,
            "the trailing, unanswered choice and suggestion survive"
        );
    }

    #[test]
    fn unrelated_submit_hides_panel_keeps_todo() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.stream = "?choice\n1. 现在就改\n2. 不改".into();
        app.commit_stream();
        app.submit_text(&mut serve, "先做另一件事".into(), vec![]);
        assert!(app.choices.is_none(), "panel steps aside for a new prompt");
        assert_eq!(
            app.todos.len(),
            1,
            "unanswered choice stays in todo until explicitly handled"
        );
    }

    #[test]
    fn trailing_question_lands_in_todo_and_retires_on_any_reply() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.stream = "所以要把这个修掉吗？".into();
        app.commit_stream();
        assert!(app.choices.is_none(), "a plain question is not a ?choice");
        assert!(app.status_segments_text().contains("❓"), "badge via todo");
        assert!(matches!(
            &app.todos[0].kind,
            TodoKind::Question { text } if text == "所以要把这个修掉吗？"
        ));
        // a heuristic question is answered by whatever the user sends next…
        app.submit_text(&mut serve, "修吧".into(), vec![]);
        assert!(
            app.todos.is_empty(),
            "any reply answers a trailing question"
        );
        // …while strict items (choice/suggest) still need per-item handling
        app.stream = "?choice\n1. 改\n2. 不改".into();
        app.commit_stream();
        app.stream = "另外 ?suggest 补个测试".into();
        app.commit_stream();
        app.submit_text(&mut serve, "别的事".into(), vec![]);
        assert_eq!(
            app.todos.len(),
            2,
            "strict markers survive an unrelated prompt"
        );
    }

    #[test]
    fn todo_dialog_reply_button_loads_question_into_input() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.stream = "要继续吗？".into();
        app.commit_stream();
        app.run_slash("/todo", &mut serve);
        {
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        let (reply, _): (Rect, TodoAction) = app
            .todo_buttons
            .iter()
            .find(|(_, act)| matches!(act, TodoAction::Reply(_)))
            .cloned()
            .unwrap();
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(MouseButton::Left),
                column: reply.x + 2,
                row: reply.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        let typed: String = app.input.iter().collect();
        assert_eq!(
            typed, "要继续吗？",
            "reply preloads the question for editing"
        );
        assert!(app.todos.is_empty(), "replying retires the question");
    }

    #[test]
    fn status_shows_pending_badge() {
        let mut app = App::new();
        app.todos.push(TodoItem {
            kind: TodoKind::Suggest { text: "x".into() },
        });
        assert!(app.status_segments_text().contains("❓"));
    }

    #[test]
    fn display_width_matches_the_renderer() {
        // ratatui lays text out with unicode-width; the mapping must agree or
        // clicks/selection drift right on lines carrying emoji
        assert_eq!(display_width("❓"), 2);
        assert_eq!(display_width("💡"), 2);
        assert_eq!(display_width("✅"), 2);
        assert_eq!(display_width("🔧"), 2);
        // ambiguous-width symbols stay 1 in the renderer — must stay 1 here too
        assert_eq!(display_width("⚙"), 1);
        assert_eq!(display_width("❯"), 1);
        assert_eq!(display_width("中"), 2);
        assert_eq!(display_width("a"), 1);
    }

    #[test]
    fn emoji_columns_map_to_exact_chars() {
        let mut app = App::new();
        app.chat_inner_w = 40;
        app.chat_scroll_offset = 0;
        app.chat_rows = wrap_chat_rows(&["💡 已完成，继续".to_string()], 40);
        // screen cols: 💡=0-1, space=2, 已=3-4, 完=5-6
        assert_eq!(app.chat_line_hit(0, 3, 0).map(|(_, _, i)| i), Some(2));
        assert_eq!(app.chat_line_hit(0, 4, 0).map(|(_, _, i)| i), Some(2));
        assert_eq!(app.chat_line_hit(0, 5, 0).map(|(_, _, i)| i), Some(3));
    }

    #[test]
    fn suggest_arms_panel_at_turn_end() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        app.prompt_id = 7;
        app.stream = "备份已开启。?suggest 把 BACKUP_DIR 指到外接盘".into();
        app.commit_stream();
        assert!(
            app.choices.is_none(),
            "mid-turn the suggestion must not steal the panel"
        );
        app.frame(
            serde_json::json!({"type": "result", "id": 7, "ok": true}),
            &mut serve,
        );
        let c = app.choices.as_ref().expect("panel armed at turn end");
        assert!(c.question.contains("外接盘"), "{}", c.question);
        let labels: Vec<&str> = c.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, vec!["接受", "忽略"]);
        assert_eq!(app.todos.len(), 1, "the todo entry still exists");
    }

    #[test]
    fn accept_on_suggest_panel_sends_and_retires() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        app.prompt_id = 7;
        app.stream = "?suggest 换个备份盘".into();
        app.commit_stream();
        app.frame(
            serde_json::json!({"type": "result", "id": 7, "ok": true}),
            &mut serve,
        );
        app.pick_choice(&mut serve, 0);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["text"], "请执行这条建议：换个备份盘");
        assert!(app.todos.is_empty(), "accepted suggestion retires");
        assert!(app.choices.is_none());
    }

    #[test]
    fn ignore_on_suggest_panel_chains_to_next() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        app.prompt_id = 7;
        app.stream = "?suggest 第一条\n又 ?suggest 第二条".into();
        app.commit_stream();
        assert_eq!(app.todos.len(), 2);
        app.frame(
            serde_json::json!({"type": "result", "id": 7, "ok": true}),
            &mut serve,
        );
        app.pick_choice(&mut serve, 1); // 忽略 the first
        assert!(serve.sent.is_empty(), "ignoring sends nothing");
        assert_eq!(app.todos.len(), 1, "ignored suggestion retires");
        let c = app
            .choices
            .as_ref()
            .expect("next suggestion takes the panel");
        assert!(c.question.contains("第二条"), "{}", c.question);
    }

    #[test]
    fn directory_path_click_opens_file_manager() {
        let mut app = App::new();
        let dir = std::env::temp_dir().join("tango-dir-click");
        std::fs::create_dir_all(&dir).unwrap();
        app.chat_inner_w = 200;
        app.chat_scroll_offset = 0;
        app.chat_rows = wrap_chat_rows(&[format!("Files in {}/ and more", dir.display())], 200);
        // double-click (press+release, twice) on the directory path itself
        let on_path = 9 + dir.display().to_string().chars().count() / 2;
        dblclick(&mut app, 0, on_path as u16);
        assert!(
            app.viewer.is_none(),
            "directories never open as text in the viewer"
        );
        assert!(
            app.lines.iter().any(|l| l.text.contains("打开目录")),
            "the directory opens in the system file manager"
        );
        // a click off the path (position rule) must not open anything
        let opened = app
            .lines
            .iter()
            .filter(|l| l.text.contains("打开目录"))
            .count();
        let off = 2u16;
        dblclick(&mut app, 0, off);
        assert_eq!(
            app.lines
                .iter()
                .filter(|l| l.text.contains("打开目录"))
                .count(),
            opened,
            "off-path click opens nothing"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn user_bubbles_right_aligned_block() {
        let mut rows = wrap_chat_rows(
            &[
                "一二三四五六七八九十 一二三四五六七八九十 abc".to_string(),
                "agent 回复左对齐".to_string(),
            ],
            40,
        );
        let is_user = vec![true, false];
        layout_user_bubbles(&mut rows, &is_user, 40, true);
        let user_rows: Vec<&ChatRow> = rows.iter().filter(|r| r.user).collect();
        assert!(user_rows.len() >= 2, "long user text wraps into rows");
        let first_x = user_rows[0].x;
        assert!(first_x > 0, "user rows sit right of the margin");
        assert!(
            user_rows.iter().all(|r| r.x == first_x),
            "wrapped rows share one block indent (bubble shape)"
        );
        let maxw = user_rows
            .iter()
            .map(|r| display_width(&r.text))
            .max()
            .unwrap();
        assert!(
            (first_x as usize) + maxw + 2 <= 40,
            "block plus interior padding fits the width"
        );
        assert!(
            rows.iter().filter(|r| !r.user).all(|r| r.x == 0),
            "agent rows keep the left edge"
        );
    }

    #[test]
    fn click_inside_bubble_maps_through_offset() {
        let mut app = App::new();
        app.chat_inner_w = 40;
        app.chat_scroll_offset = 0;
        app.chat_rows = vec![ChatRow {
            line: 0,
            cs: 0,
            ce: 6,
            text: "abcdef".into(),
            x: 30,
            think: false,
            user: true,
            border: false,
            wordmark: false,
        }];
        // bubble interior starts at x+2 (│ + pad); column 34 → char 2 ('c')
        assert_eq!(app.chat_line_hit(0, 34, 0).map(|(_, _, i)| i), Some(2));
        // left of the bubble clamps to char 0
        assert_eq!(app.chat_line_hit(0, 5, 0).map(|(_, _, i)| i), Some(0));
    }

    #[test]
    fn partial_cjk_selection_inside_bubble_is_char_exact() {
        let mut app = App::new();
        app.chat_rows = vec![ChatRow {
            line: 0,
            cs: 0,
            ce: 4,
            text: "汉字汉字".into(),
            x: 30,
            think: false,
            user: true,
            border: false,
            wordmark: false,
        }];
        // interior starts at col 32 (x=30 + │ + pad); 汉 spans 32-33, 字 spans 34-35
        app.sel_anchor_pos = Some((0, 32));
        app.sel_end_pos = Some((0, 36));
        assert_eq!(app.selected_text(0).as_deref(), Some("汉字"));
        // and the plain-row path keeps working (x=0, ascii)
        app.chat_rows[0] = ChatRow {
            line: 0,
            cs: 0,
            ce: 6,
            text: "abcdef".into(),
            x: 0,
            user: false,
            border: false,
                think: false,
                wordmark: false,
        };
        app.sel_anchor_pos = Some((0, 1));
        app.sel_end_pos = Some((0, 3));
        assert_eq!(app.selected_text(0).as_deref(), Some("bc"));
    }

    #[test]
    fn choices_panel_reserves_layout_space_not_overlay() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.stream = "要改吗？\n?choice\n1. 改\n2. 不改".into();
        app.commit_stream();
        {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        // the panel sits in its own layout row below the chat — never over it
        for (rect, _) in &app.choice_rows {
            assert!(
                rect.y >= app.chat_area.bottom(),
                "panel row at y={} overlaps chat (bottom={})",
                rect.y,
                app.chat_area.bottom()
            );
        }
        // and the chat actually shrank to make room
        assert!(
            app.chat_area.height < 24 - 2,
            "chat height {} should shrink for the panel",
            app.chat_area.height
        );
        // option rows are clickable in the new geometry
        assert!(app.choice_rows.len() >= 2);
        let _ = serve;
    }

    #[test]
    fn long_choice_question_wraps_fully_visible() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let long: String = "这是一个非常长的问题需要多行才能显示完整".repeat(8);
        app.choices = Some(Choices {
            question: long.clone(),
            preview: vec![],
            options: vec![
                ChoiceOption {
                    label: "是".into(),
                    note: None,
                },
                ChoiceOption {
                    label: "否".into(),
                    note: None,
                },
            ],
            sel: 0,
            suggest: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        // the tail of the question must actually be on screen (not truncated away);
        // 144 CJK chars at 35/row → the last wrapped row holds the final 4, so a
        // within-row tail avoids row-boundary padding spaces
        let tail: String = long
            .chars()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol().to_string())
            .collect();
        let compact: String = screen.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(
            compact.contains(tail.as_str()),
            "question tail {tail:?} missing — question was truncated"
        );
        let _ = terminal;
    }

    #[test]
    fn elapsed_and_token_formats() {
        assert_eq!(App::fmt_elapsed(42), "42s");
        assert_eq!(App::fmt_elapsed(59), "59s");
        assert_eq!(App::fmt_elapsed(60), "1m00s");
        assert_eq!(App::fmt_elapsed(802), "13m22s");
        assert_eq!(App::fmt_elapsed(5432), "1h30m");
        assert_eq!(App::fmt_tokens(0), "0");
        assert_eq!(App::fmt_tokens(500), "500");
        assert_eq!(App::fmt_tokens(999), "999");
        assert_eq!(App::fmt_tokens(14_000), "14.0k");
        assert_eq!(App::fmt_tokens(1_500_000), "1.5M");
    }

    #[test]
    fn busy_rule_shows_elapsed_and_received() {
        let mut app = App::new();
        app.busy = true;
        app.turn_started = Some(std::time::Instant::now());
        app.turn_chars = 14_000;
        let s = app.work_text();
        assert!(s.contains("↓14.0k"), "received chars shown while busy: {s}");
        assert!(s.contains('s'), "elapsed shown: {s}");
        assert!(
            !app.status_segments_text().contains("↓14.0k"),
            "not in the status bar anymore"
        );
        // idle stays clean
        app.busy = false;
        app.turn_started = None;
        assert_eq!(app.work_text(), "idle");
    }

    #[test]
    fn stream_deltas_count_into_turn_chars() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.frame(
            serde_json::json!({"type": "stream", "event": {"type": "text", "delta": "hello 世界"}}),
            &mut serve,
        );
        app.frame(
            serde_json::json!({"type": "stream", "event": {"type": "thinking", "delta": "!!"}}),
            &mut serve,
        );
        assert_eq!(app.turn_chars, 10, "text and thinking deltas both count");
    }

    #[test]
    fn tip_line_rotates_every_20s() {
        assert!(TIPS.len() >= 4, "a real pool, not a one-liner");
        assert_eq!(App::tip_for(0), App::tip_for(19), "stable within a window");
        assert_eq!(App::tip_for(20), App::tip_for(39));
        assert_ne!(App::tip_for(0), App::tip_for(20), "rotates across windows");
        assert!(TIPS.contains(&App::tip_for(37)), "always a pool member");
    }

    #[test]
    fn restart_quits_carrying_session_and_mode() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/restart", &mut serve);
        assert!(
            !app.quit && app.restart.is_none(),
            "no session yet → refuse"
        );
        app.session_full = "a1b2c3d4e5f67890".into();
        app.mode = "auto".into();
        app.busy = true;
        app.run_slash("/restart", &mut serve);
        assert!(!app.quit && app.restart.is_none(), "busy turn → refuse");
        app.busy = false;
        app.run_slash("/restart", &mut serve);
        assert!(app.quit);
        assert_eq!(
            app.restart,
            Some(("a1b2c3d4e5f67890".into(), "auto".into()))
        );
    }

    #[test]
    fn exit_note_carries_session_and_resume_command() {
        let mut app = App::new();
        assert!(app.exit_note().is_none(), "no handshake → no note");
        app.session_full = "a1b2c3d4e5f67890".into();
        let note = app.exit_note().expect("note once serve handshook");
        assert!(note.contains("a1b2c3d4e5f67890"), "full session id: {note}");
        assert!(note.contains("tango --resume a1b2c3d4e5f67890"), "{note}");
    }

    #[test]
    fn esc_closes_an_open_dialog_without_interrupting_the_turn() {
        let esc = || crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let interrupted = |s: &Serve| s.sent.iter().any(|l| l.contains("\"interrupt\""));
        let cases: Vec<(&str, fn(&mut App))> = vec![
            ("todo", |a| {
                a.stream = "?choice\n1. 甲\n2. 乙\n另外 ?suggest 补测试".into();
                a.commit_stream();
                a.choices = None;
                a.todo_dialog = true;
            }),
            ("choices", |a| {
                a.stream = "?choice\n1. 甲\n2. 乙".into();
                a.commit_stream();
            }),
            ("picker", |a| a.open_mode_picker()),
            ("slash popup", |a| {
                a.input = "/mo".chars().collect();
                a.cursor = a.input.len();
                a.refresh_popup();
                assert!(a.popup.is_some());
            }),
            ("queue", |a| a.queue_dialog = true),
            ("problems", |a| a.problems_dialog = true),
            ("history popup", |a| {
                a.push_user("第一问");
                a.open_history();
                assert!(a.popup.is_some());
            }),
            ("prompt menu", |a| a.prompt_menu = Some(0)),
        ];
        for (name, open) in cases {
            let mut app = App::new();
            let mut serve = Serve::mock();
            open(&mut app);
            app.busy = true;
            app.key(esc(), &mut serve);
            assert!(
                !interrupted(&serve),
                "{name}: the first Esc must only close the dialog"
            );
            assert!(
                !app.todo_dialog
                    && app.picker.is_none()
                    && app.popup.is_none()
                    && !app.queue_dialog
                    && !app.problems_dialog
                    && app.prompt_menu.is_none()
                    && (app.choices.is_none() || app.choices_collapsed),
                "{name}: the dialog is still open"
            );
            // nothing open any more: the next Esc interrupts
            app.key(esc(), &mut serve);
            assert!(interrupted(&serve), "{name}: the second Esc interrupts");
        }
    }

    #[test]
    fn todo_with_text_adds_a_note_and_lists_everything() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.stream = "?choice\n1. 提交\n2. 再看看".into();
        app.commit_stream();
        // `/todo <text>`: a note goes in ahead of the model's items and the list opens on it
        app.run_slash("/todo 给 tango 加星标", &mut serve);
        assert!(app.todo_dialog);
        assert_eq!(app.todo_sel, 0);
        assert!(matches!(&app.todos[0].kind, TodoKind::Note { text, done: false } if text == "给 tango 加星标"), "notes come first");
        assert_eq!(app.todos.len(), 2, "the choice is still there: the list shows all of it");
        assert!(serve.sent.is_empty(), "adding a note sends nothing to the model");
        // the same text twice is not added again
        app.run_slash("/todo 给 tango 加星标", &mut serve);
        assert_eq!(app.todos.len(), 2);
        // a second note goes after open notes; enter ticks the selected one
        app.run_slash("/todo 回复邮件", &mut serve);
        assert_eq!(app.todo_sel, 1, "opened on the new note");
        let note = |a: &App, i: usize| match &a.todos[i].kind {
            TodoKind::Note { text, done } => (text.clone(), *done),
            _ => unreachable!(),
        };
        assert_eq!(note(&app, 0), ("给 tango 加星标".into(), false));
        assert_eq!(note(&app, 1), ("回复邮件".into(), false));
        app.key(KeyCode::Enter.into(), &mut serve);
        assert_eq!(note(&app, 1), ("回复邮件".into(), true));
        assert!(serve.sent.is_empty(), "ticking is local");
        app.key(KeyCode::Enter.into(), &mut serve);
        assert_eq!(note(&app, 1), ("回复邮件".into(), false), "enter unticks too");
        // d deletes the selected note and leaves the rest
        app.key(KeyCode::Char('d').into(), &mut serve);
        assert_eq!(app.todos.len(), 2);
        assert_eq!(note(&app, 0), ("给 tango 加星标".into(), false));
        // bare /todo closes; bare again reopens with everything still there
        app.run_slash("/todo", &mut serve);
        assert!(!app.todo_dialog);
        app.run_slash("/todo", &mut serve);
        assert!(app.todo_dialog && app.todos.len() == 2);
        // a new prompt does not retire notes (only heuristic questions retire)
        app.submit_text(&mut serve, "别的事".into(), vec![]);
        assert!(app.todos.iter().any(|t| matches!(t.kind, TodoKind::Note { .. })));
    }

    #[test]
    fn todo_notes_survive_a_new_app_and_follow_the_directory() {
        let dir = std::env::temp_dir().join(format!("tango-todo-notes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        let note_texts = |a: &App| {
            a.todos
                .iter()
                .filter_map(|t| match &t.kind {
                    TodoKind::Note { text, done } => Some((text.clone(), *done)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        // first app: notes for directory a, and one for b
        let mut app = App::new();
        app.notes_home = Some(dir.clone());
        app.serve_cwd = Some(dir.join("a"));
        let mut serve = Serve::mock();
        app.run_slash("/todo 甲一", &mut serve);
        app.run_slash("/todo 甲二", &mut serve);
        app.key(KeyCode::Enter.into(), &mut serve); // 甲二 done
        app.serve_cwd = Some(dir.join("b"));
        app.run_slash("/todo 乙一", &mut serve);
        // a fresh app in a sees a's notes as they were left (open first, done after)
        let mut again = App::new();
        again.notes_home = Some(dir.clone());
        again.serve_cwd = Some(dir.join("a"));
        again.load_notes();
        assert_eq!(note_texts(&again), vec![("甲一".to_string(), false), ("甲二".to_string(), true)]);
        // deleting the last note removes the directory's entry instead of leaving []
        again.run_slash("/todo", &mut Serve::mock()); // open the list
        again.key(KeyCode::Char('d').into(), &mut Serve::mock());
        again.key(KeyCode::Char('d').into(), &mut Serve::mock());
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("todos.json")).unwrap()).unwrap();
        assert!(saved.get(dir.join("a").to_str().unwrap()).is_none());
        assert!(saved.get(dir.join("b").to_str().unwrap()).is_some(), "b's notes untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn todo_dialog_is_fully_keyboard_driven() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.stream = "改好了。?choice\n1. 提交\n2. 再看看\n另外 ?suggest 顺手补个回归测试".into();
        app.commit_stream();
        let suggest = app
            .todos
            .iter()
            .position(|t| matches!(t.kind, TodoKind::Suggest { .. }))
            .unwrap();
        app.run_slash("/todo", &mut serve);
        let key = |app: &mut App, serve: &mut Serve, c: KeyCode| app.key(c.into(), serve);
        // typing goes to the dialog, not the input box
        key(&mut app, &mut serve, KeyCode::Char('x'));
        assert!(app.input.is_empty(), "the dialog is modal");
        // ↓/↑ move and clamp; a digit jumps to that row
        key(&mut app, &mut serve, KeyCode::Down);
        key(&mut app, &mut serve, KeyCode::Down);
        assert_eq!(app.todo_sel, 1, "clamped to the last row");
        key(&mut app, &mut serve, KeyCode::Up);
        assert_eq!(app.todo_sel, 0);
        key(&mut app, &mut serve, KeyCode::Char(char::from(b'1' + suggest as u8)));
        assert_eq!(app.todo_sel, suggest);
        // enter on a suggestion runs it (the row's first button)
        key(&mut app, &mut serve, KeyCode::Enter);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["text"], "请执行这条建议：顺手补个回归测试");
        assert_eq!(app.todos.len(), 1);
        assert_eq!(app.todo_sel, 0, "selection follows the shorter list");
        // the hint's "d 忽略" is a button too: clicking it drops the selected row
        let mut term = Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let d = app
            .hint_buttons
            .iter()
            .find(|(_, k)| k.code == KeyCode::Char('d'))
            .map(|(r, _)| *r)
            .expect("d has a hint button");
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: d.x,
                row: d.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert!(app.todos.is_empty());
        assert!(app.todo_dialog, "still open, now empty");
        // enter on an empty list does nothing; esc closes
        key(&mut app, &mut serve, KeyCode::Enter);
        key(&mut app, &mut serve, KeyCode::Esc);
        assert!(!app.todo_dialog);
        // enter on a choice re-opens its panel and closes the dialog
        app.stream = "?choice\n1. 提交\n2. 再看看".into();
        app.commit_stream();
        app.submit_text(&mut serve, "别的事".into(), vec![]);
        app.run_slash("/todo", &mut serve);
        key(&mut app, &mut serve, KeyCode::Enter);
        assert!(app.choices.is_some() && !app.todo_dialog);
    }

    #[test]
    fn todo_dialog_buttons_run_drop_expand() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.stream = "改好了。?choice\n1. 提交\n2. 再看看\n另外 ?suggest 顺手补个回归测试".into();
        app.commit_stream();
        assert_eq!(app.todos.len(), 2);
        app.run_slash("/todo", &mut serve);
        assert!(app.todo_dialog);
        {
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        // the suggestion's [执行] — find it by action kind, not row order
        let (run1, _): (Rect, TodoAction) = app
            .todo_buttons
            .iter()
            .find(|(_, act)| matches!(act, TodoAction::Run(_)))
            .cloned()
            .unwrap();
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(MouseButton::Left),
                column: run1.x + 2,
                row: run1.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "prompt");
        assert_eq!(sent["text"], "请执行这条建议：顺手补个回归测试");
        assert_eq!(app.todos.len(), 1, "acted-on suggestion retires");

        // drop the remaining choice (its Drop button carries the choice's index);
        // the dialog stays open after a button click, so just redraw
        let choice_idx = app
            .todos
            .iter()
            .position(|t| matches!(&t.kind, TodoKind::Choice { .. }))
            .unwrap();
        {
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }
        let (drop0, _): (Rect, TodoAction) = app
            .todo_buttons
            .iter()
            .find(|(_, act)| *act == TodoAction::Drop(choice_idx))
            .cloned()
            .unwrap();
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(MouseButton::Left),
                column: drop0.x + 2,
                row: drop0.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert!(app.todos.is_empty(), "dismissed choice retires");
        assert!(app.todo_dialog, "dialog stays open after a button click");

        // expand re-opens a parked choice's panel
        app.stream = "?choice\n1. 提交\n2. 再看看".into();
        app.commit_stream();
        app.submit_text(&mut serve, "别的事".into(), vec![]);
        assert!(app.choices.is_none());
        app.expand_choice(0);
        let c = app.choices.as_ref().expect("panel re-opened from todo");
        let labels: Vec<&str> = c.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, vec!["提交", "再看看"]);
    }

    #[test]
    fn model_picker_groups_by_platform_with_free_first() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut app = App::new();
        let item = |id: &str, group: &str, current: bool| PickItem {
            id: id.into(),
            label: id.into(),
            note: String::new(),
            warn: false,
            current,
            group: group.into(),
        };
        // serve sends models pre-ordered: virtual "free" platform first, then sources
        app.picker = Some(Picker::new(
            PickerKind::Model,
            vec![
                item("x/free-a", "free", false),
                item("tknet/m1", "tknet", true),
                item("opencode/m2", "opencode", false),
            ],
        ));
        let mut terminal = Terminal::new(TestBackend::new(72, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..20)
            .map(|y| (0..72).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("── free ──"), "free header rendered");
        assert!(text.contains("── tknet ──"), "tknet header rendered");
        assert!(text.contains("── opencode ──"), "opencode header rendered");
        let p = app.picker.as_ref().unwrap();
        assert_eq!(
            p.sel, 1,
            "selection lands on the current model, not a header"
        );
        assert_eq!(
            p.filtered_items().len(),
            3,
            "headers are not selectable items"
        );
        assert_eq!(
            app.picker_rows,
            vec![None, Some(0), None, Some(1), None, Some(2)],
            "mouse row map: header rows are None"
        );
        // filtering keeps only the matching group's header
        app.picker.as_mut().unwrap().query = "opencode".into();
        app.picker.as_mut().unwrap().sel = 0;
        let mut terminal = Terminal::new(TestBackend::new(72, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..20)
            .map(|y| (0..72).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("── opencode ──"));
        assert!(
            !text.contains("── tknet ──"),
            "filtered-out group header gone"
        );
        assert_eq!(app.picker_rows, vec![None, Some(0)]);
    }

    #[test]
    fn pinned_prompt_shows_while_busy_and_recap_lands_on_completion() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.input = "帮我优化数据库查询性能的问题".chars().collect();
        app.cursor = app.input.len();
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        assert!(app.busy);
        assert_eq!(
            app.current_prompt.as_deref(),
            Some("帮我优化数据库查询性能的问题")
        );
        let spans = app.status_spans(200);
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.contains("▶ 帮我优化数据库查询性能的问题"),
            "pinned in status: {text}"
        );

        // turn completes: recap line from the answer's first line, pin context ends
        app.assistant_replies
            .push("已将慢查询加了索引，延迟从 2s 降到 50ms。另外把连接池从 10 调到 50，并给 orders 表的 user_id 和 created_at 建了联合索引，另外把连接池从 10 调到 50，并给 orders 表的 user_id 和 created_at 建了联合索引，另外把连接池从 10 调到 50，并给 orders 表的 user_id 和 created_at 建了联合索引，结尾在这里\n细节略".into());
        app.frame(
            json!({"type": "result", "id": app.prompt_id, "ok": true, "data": {}}),
            &mut serve,
        );
        assert!(!app.busy);
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("✓ 本轮：已将慢查询加了索引")
                    && l.text.ends_with("结尾在这里")),
            "auto recap pushed, whole first line however long"
        );
        assert!(!app.lines.iter().any(|l| l.text.contains("细节略")), "only the first line");
        // pinned prompt span stops showing after completion
        let spans2 = app.status_spans(200);
        let text2: String = spans2.iter().map(|s| s.content.to_string()).collect();
        assert!(
            !text2.contains("▶ 帮我优化"),
            "pin cleared after completion: {text2}"
        );
    }

    #[test]
    fn queued_prompts_send_in_order_and_interrupt_keeps_the_queue() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        for text in ["第二个问题", "第三个问题"] {
            app.input = text.chars().collect();
            app.cursor = app.input.len();
            app.key(
                crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut serve,
            );
        }
        assert_eq!(app.local_queue.len(), 2);

        // turn 1 finishes → head auto-sends
        app.frame(
            json!({"type": "result", "id": app.prompt_id, "ok": true, "data": {}}),
            &mut serve,
        );
        assert_eq!(serve.sent.len(), 1);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["text"], "第二个问题");

        // ctrl+c interrupts the CURRENT turn only — the queue survives
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &mut serve,
        );
        let interrupt: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(interrupt["cmd"], "interrupt");
        assert_eq!(
            app.local_queue,
            vec![QueuedPrompt::text_only("第三个问题")],
            "queue survives"
        );
        assert!(app.lines.iter().any(|l| l.text.contains("排队保留 1 条")));
    }

    #[test]
    fn ime_switch_policy() {
        // already ABC/US → no switch
        assert!(!ime_should_switch("com.apple.keylayout.ABC"));
        assert!(!ime_should_switch("com.apple.keylayout.US"));
        // CJK IMEs and layouts → switch
        assert!(ime_should_switch("com.tencent.inputmethod.wetype.pinyin"));
        assert!(ime_should_switch("im.rime.inputmethod.Squirrel.Hans"));
    }

    #[test]
    fn ime_state_round_trip_is_guarded_in_tests() {
        let mut app = App::new();
        app.ime_restore = Some("com.tencent.inputmethod.wete.pinyin".into());
        app.ime_for_dialog(); // cfg(test) → no system call, no state change beyond guard
        assert_eq!(
            app.ime_restore.as_deref(),
            Some("com.tencent.inputmethod.wete.pinyin"),
            "tests never touch the real IME"
        );
        app.ime_restore_prev(); // cfg(test) → clears only
        assert!(app.ime_restore.is_none());
    }

    // ---------- pasted images ----------

    /// A unique existing .png path for paste tests (content is irrelevant to the TUI).
    fn temp_image(tag: &str) -> String {
        let p = std::env::temp_dir().join(format!(
            "tango-img-{}-{}-{tag}.png",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, b"fake-png-bytes").unwrap();
        p.display().to_string()
    }

    #[test]
    fn image_path_detection_rules() {
        let img = temp_image("detect");
        assert_eq!(image_path_from_paste(&img), Some(img.clone()));
        assert_eq!(
            image_path_from_paste(&format!("  {img} \n")),
            Some(img.clone()),
            "trimmed single line"
        );
        assert_eq!(
            image_path_from_paste(&format!("'{img}'")),
            Some(img.clone()),
            "quoted drag path"
        );
        assert_eq!(
            image_path_from_paste(&format!("{img}\nmore")),
            None,
            "multi-line"
        );
        assert_eq!(
            image_path_from_paste("/no/such/file.png"),
            None,
            "must exist"
        );
        assert_eq!(image_path_from_paste("notes.txt"), None, "not an image ext");
        assert_eq!(image_path_from_paste(""), None);
        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn pasted_image_becomes_a_clickable_chip_not_a_path() {
        let mut app = App::new();
        let img = temp_image("chip");
        app.insert_paste(&img);
        // displayed as a chip, raw input keeps the real path
        assert_eq!(app.rendered_input().text, "[图片 #1]");
        assert_eq!(app.input.iter().collect::<String>(), img);
        assert_eq!(app.images.len(), 1);
        assert_eq!(app.images[0].path, img);
        assert!(app.lines.iter().any(|l| l.text.contains("已附加图片 #1")));
        // backspace consumes the whole chip atomically
        app.cursor = app.input.len();
        app.backspace_input();
        assert!(app.input.is_empty());
        assert!(app.pastes.is_empty());
        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn non_image_paste_is_unchanged() {
        let mut app = App::new();
        app.insert_paste("just some text");
        assert_eq!(app.rendered_input().text, "just some text");
        assert!(app.images.is_empty());
    }

    #[test]
    fn image_rides_the_next_prompt_as_upload_and_marker() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        let img = temp_image("send");
        app.insert_paste(&img);
        for c in "看看这张图".chars() {
            app.insert_char(c);
        }
        app.send_prompt(&mut serve);
        assert_eq!(serve.sent.len(), 1);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "prompt");
        assert_eq!(sent["images"], json!([img]));
        let text = sent["text"].as_str().unwrap();
        assert!(text.contains("[图片 #1]"), "marker in text: {text}");
        assert!(text.contains("看看这张图"));
        assert!(!text.contains(&img), "the long path is not sent as text");
        // input cleared, but the registry keeps the path so the chat chip stays clickable
        assert!(app.input.is_empty());
        assert_eq!(app.images.len(), 1);
        assert!(app.lines.iter().any(|l| l.text.contains("[图片 #1]")));
        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn image_only_prompt_is_sendable() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        let img = temp_image("only");
        app.insert_paste(&img);
        app.send_prompt(&mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["text"], "[图片 #1]");
        assert_eq!(sent["images"], json!([img]));
        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn queued_image_prompt_keeps_its_images_until_drained() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.busy = true;
        let img = temp_image("queue");
        app.insert_paste(&img);
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        assert_eq!(app.local_queue.len(), 1);
        assert_eq!(app.local_queue[0].images, vec![img.clone()]);
        assert!(serve.sent.is_empty(), "queued, not sent yet");
        // the running turn finishes → the queued image prompt goes out with its images
        app.frame(
            json!({"type": "result", "id": app.prompt_id, "ok": true, "data": {}}),
            &mut serve,
        );
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "prompt");
        assert_eq!(sent["images"], json!([img]));
        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn draft_stash_restores_the_image_chip() {
        let mut app = App::new();
        let img = temp_image("stash");
        app.insert_paste(&img);
        app.toggle_draft_stash();
        assert!(app.input.is_empty());
        app.toggle_draft_stash();
        assert_eq!(app.rendered_input().text, "[图片 #1]");
        let (text, images) = app.take_submission();
        assert_eq!(text, "[图片 #1]");
        assert_eq!(images, vec![img.clone()]);
        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn input_rule_carries_work_on_the_left_and_the_tip_on_the_right() {
        use ratatui::{Terminal, backend::TestBackend};
        let plain = Style::new();
        let text = |l: &Line| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };

        let l = App::rule_line("idle", None, 40, plain, plain, None);
        assert_eq!(display_width(&text(&l)), 40);
        assert!(text(&l).starts_with("─ idle ─"), "{}", text(&l));

        let l = App::rule_line("\\o/ 12s · ↓14.0k", Some("ctrl+o 看工具"), 60, plain, plain, None);
        let t = text(&l);
        assert_eq!(display_width(&t), 60);
        assert!(t.starts_with("─ \\o/ 12s · ↓14.0k ─"), "{t}");
        assert!(t.ends_with(" ctrl+o 看工具 ─"), "{t}");

        // too narrow for both: the tip goes, then the counter, then the timer
        let t = text(&App::rule_line(
            "\\o/ 12s · ↓14.0k",
            Some("ctrl+o 看工具"),
            24,
            plain,
            plain,
            None,
        ));
        assert!(t.contains("12s") && !t.contains("看工具"), "{t}");
        let t = text(&App::rule_line("\\o/ 12s · ↓14.0k", None, 12, plain, plain, None));
        assert!(t.contains("\\o/") && !t.contains("↓"), "{t}");
        let t = text(&App::rule_line("\\o/ 12s · ↓14.0k", None, 8, plain, plain, None));
        assert_eq!(display_width(&t), 8, "{t}");

        // on screen it is the input box's first row
        let mut app = App::new();
        app.busy = true;
        app.turn_started = Some(std::time::Instant::now() - std::time::Duration::from_secs(45));
        app.turn_chars = 14_000;
        let mut terminal = Terminal::new(TestBackend::new(70, 14)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let b = terminal.backend().buffer();
        let row: String = (0..70).map(|x| b[(x, app.input_area.y)].symbol()).collect();
        assert!(
            row.starts_with("─ ") && row.contains("45s") && row.contains("↓14.0k"),
            "{row}"
        );
        assert!(
            row.ends_with('─') && row.contains("/queue"),
            "long-turn tip on the right: {row}"
        );
    }

    #[test]
    fn multiline_prompt_renders_as_rows_inside_one_frame() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        app.push_user("第一行\n\nthird");
        let mut t = Terminal::new(TestBackend::new(40, 12)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let b = t.backend().buffer();
        let rows: Vec<String> = (0..12)
            .map(|y| (0..40).map(|x| b[(x, y)].symbol().to_string()).collect())
            .collect();
        // wide chars leave a blank continuation cell, so compare with spaces removed
        let at = |needle: &str| {
            rows.iter()
                .position(|r| r.replace(' ', "").contains(needle))
        };
        let first = at("❯第一行").expect("marker and first line share a row");
        assert_eq!(at("third"), Some(first + 2), "{rows:#?}");
        assert!(
            rows[first - 1].contains('╭') && rows[first + 3].contains('╰'),
            "{rows:#?}"
        );
        assert!(
            rows[first + 1].trim_end().ends_with('│'),
            "blank row is framed: {rows:#?}"
        );
        assert!(!rows.iter().any(|r| r.contains('⏎')));
    }

    #[test]
    fn bubble_right_bar_lines_up_with_the_frame_corners() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        // a collapsed paste: the first row fills the bubble exactly, the rest are shorter
        let text = format!(
            "给香港政府一些建议，你要说人话，口语化 {}",
            "补充说明 ".repeat(60)
        );
        app.push_user(&text);
        for w in [120u16, 80, 40, 24] {
            let mut t = Terminal::new(TestBackend::new(w, 24)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            let b = t.backend().buffer();
            let mut cols = Vec::new();
            for y in 0..24 {
                if let Some(x) = (0..w).rev().find(|&x| "│╮╯".contains(b[(x, y)].symbol())) {
                    cols.push(x);
                }
            }
            assert!(cols.len() >= 3, "a framed bubble at width {w}: {cols:?}");
            assert!(
                cols.iter().all(|c| *c == cols[0]),
                "right edge misaligned at width {w}: {cols:?}"
            );
            assert_eq!(
                cols[0],
                w - 1 - BUBBLE_MARGIN as u16,
                "the bubble keeps a {BUBBLE_MARGIN}-column right margin at width {w}"
            );
        }
    }

    #[test]
    fn narrow_status_bar_keeps_work_cost_and_todo_and_drops_the_rest() {
        let mut app = App::new();
        app.model = "apikeyfan/claude-opus-5-5".into();
        app.mode = "ask".into();
        app.cost = 0.1234;
        app.busy = true;
        app.turn_started = Some(std::time::Instant::now());
        app.session = "1d587483".into();
        app.session_full = "1d587483-0000".into();
        app.context_window = 200_000;
        app.context_tokens = 54_000;
        app.current_prompt = Some("帮我优化数据库查询性能的问题".into());
        app.todos.push(TodoItem {
            kind: TodoKind::Suggest { text: "x".into() },
        });

        let text_at = |app: &mut App, w: u16| -> String {
            let spans = app.status_spans(w);
            let s: String = spans.iter().map(|s| s.content.to_string()).collect();
            assert!(display_width(&s) <= w as usize, "width {w} overflowed: {s}");
            s
        };
        let wide = text_at(&mut app, 200);
        assert!(
            wide.contains("sess") && wide.contains("ctx") && wide.contains("▶"),
            "{wide}"
        );

        for w in [60u16, 40, 30, 20, 12, 6] {
            let s = text_at(&mut app, w);
            if w >= 12 {
                assert!(s.contains("$0.1234"), "cost stays at {w}: {s}");
            }
            assert!(
                !s.contains("↓") && !s.contains("working"),
                "work moved to the input rule: {s}"
            );
            assert!(
                !s.contains("ctx") && !s.contains("sess"),
                "ctx/sess go first at {w}: {s}"
            );
            if w >= 30 {
                assert!(s.contains("❓"), "todo badge stays at {w}: {s}");
            }
        }
        let s = text_at(&mut app, 40);
        assert!(!s.contains("claude-opus-5-5"), "model name shortened: {s}");
    }

    #[test]
    fn working_figure_dances_the_tango_in_fixed_width_frames() {
        let frames: Vec<String> = (0..8).map(|i| App::tango_frame(i * 120)).collect();
        assert!(frames.iter().all(|f| display_width(f) == 3), "{frames:?}");
        assert!(
            frames.iter().all(|f| f
                .chars()
                .all(|c| ('\u{2800}'..='\u{28FF}').contains(&c))),
            "braille cells: {frames:?}"
        );
        assert!(frames.windows(2).any(|w| w[0] != w[1]), "it moves");
        assert_eq!(App::tango_frame(0), App::tango_frame(8 * 120), "loops");
        assert_eq!(
            App::tango_frame(10),
            App::tango_frame(119),
            "holds within a frame"
        );

        let mut app = App::new();
        app.busy = true;
        app.turn_started = Some(std::time::Instant::now());
        app.turn_chars = 14_000;
        let s = app.work_text();
        assert!(
            s.contains("↓14.0k") && s.starts_with('●'),
            "the dot leads the work text: {s}"
        );
        app.busy = false;
        assert_eq!(app.work_text(), "idle");
    }

    #[test]
    fn busy_dot_leads_the_work_text() {
        let mut app = App::new();
        app.busy = true;
        app.turn_started = None;
        assert_eq!(app.work_text(), "● working");
    }

    #[test]
    fn rule_sweep_runs_with_output_and_stops_on_a_stall() {
        use std::time::Duration;
        assert_eq!(sweep_speed(Duration::from_millis(100)), 2, "streaming runs");
        assert_eq!(sweep_speed(Duration::from_secs(3)), 1, "waiting drifts");
        assert_eq!(sweep_speed(Duration::from_secs(11)), 0, "silence stops");
        // the band: head brightest, a fading trail to its left, wrapping
        // (truecolor or the 256-color fallback, the band shape is the same)
        let head = sweep_color(10, 10, 40, false).unwrap();
        let trail = sweep_color(7, 10, 40, false).unwrap();
        assert_ne!(head, trail);
        assert!(sweep_color(11, 10, 40, false).is_none(), "nothing ahead of the head");
        assert!(sweep_color(38, 1, 40, false).is_some(), "the trail wraps");
        assert!(sweep_color(10, 10, 40, true).is_none(), "stalled: no band");
        // a stalled rule greys the dot; it still reads the same text
        let plain = Style::default();
        let live = Style::default().fg(Color::Green);
        let l = App::rule_line("● 12s · ↓1.0k", None, 40, plain, live, Some((5, true)));
        let dot = l.spans.iter().find(|s| s.content == "●").unwrap();
        assert_eq!(dot.style, plain, "stalled dot drops the live color");
        let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(t.starts_with("─ ● 12s · ↓1.0k ─"), "{t}");
        assert_eq!(display_width(&t), 40);
        // live: the dot is steady bold live color
        let l = App::rule_line("● 12s", None, 30, plain, live, Some((5, false)));
        let dot = l.spans.iter().find(|s| s.content == "●").unwrap();
        assert_eq!(dot.style.fg, Some(Color::Green));
        let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(display_width(&t), 30, "the band never changes the width: {t}");
    }

    #[test]
    fn animations_off_keeps_the_rule_text_but_drops_the_motion() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut serve = Serve::mock();
        let mut app = App::new();
        assert!(app.animations, "on by default");
        app.run_slash("/animations off", &mut serve);
        assert!(!app.animations);
        let sent: Value = serde_json::from_str(serve.sent.last().unwrap()).unwrap();
        assert_eq!(sent["cmd"], "set_animations");
        assert_eq!(sent["animations"], false);
        app.busy = true;
        app.turn_started = None;
        let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
        term.draw(|f| app.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let row: Vec<_> = (0..12)
            .map(|y| (0..60).map(|x| buf[(x, y)].clone()).collect::<Vec<_>>())
            .find(|r| r.iter().map(|c| c.symbol()).collect::<String>().starts_with("─ ● working"))
            .expect("busy rule row");
        // no band: every dash after the text shares one style
        let dashes: Vec<_> = row.iter().skip(12).filter(|c| c.symbol() == "─").collect();
        assert!(dashes.windows(2).all(|w| w[0].fg == w[1].fg), "no sweep when off");
        app.run_slash("/animations", &mut serve);
        assert!(app.animations, "bare command toggles back on");
        app.run_slash("/animations maybe", &mut serve);
        assert!(app.animations, "bad arg changes nothing");
    }

    #[test]
    fn splash_wordmark_showcases_until_the_first_message() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        app.push(
            "tango serve · session 1234abcd · 20 tools · /help",
            Tone::Muted,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        fn text(t: &mut ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
            let buffer = t.backend().buffer();
            (0..24usize)
                .map(|y| {
                    (0..80usize)
                        .map(|x| buffer[(x as u16, y as u16)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        terminal.draw(|f| app.draw(f)).unwrap();
        let s = text(&mut terminal);
        assert!(s.contains('█'), "the wordmark's block letters: {s}");
        // centered and tight: the top row spans the 26-column wordmark only
        let top = s.lines().find(|l| l.contains('█')).unwrap();
        let first = top.find('█').unwrap();
        let last = top.rfind('█').unwrap() + '█'.len_utf8();
        let span = top[first..last].chars().count();
        assert!(span <= 26, "letters sit one space apart: {top:?}");
        assert_eq!(top[..first].chars().count(), (80 - 26) / 2, "centered: {top:?}");
        let flat: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(flat.contains("输入问题开始"), "the hint line: {s}");
        // a user message replaces the splash with the normal chat
        app.push_user("开始干活");
        terminal.draw(|f| app.draw(f)).unwrap();
        let s2 = text(&mut terminal);
        let flat2: String = s2.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(!flat2.contains("输入问题开始"), "splash gone: {s2}");
        assert!(flat2.contains("开始干活"), "chat instead: {s2}");
    }

    /// The screen's rows as plain strings (wide chars occupy two cells; the
    /// second is a blank, so text checks strip spaces).
    fn screen_rows(t: &mut ratatui::Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
        let w = t.backend().buffer().area.width as usize;
        let h = t.backend().buffer().area.height as usize;
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| t.backend().buffer()[(x as u16, y as u16)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn wordmark_stays_at_the_top_of_the_chat_after_the_first_message() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        app.push_user("演示一下这个功能");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let rows = screen_rows(&mut terminal);
        let chat_y = app.chat_area.y as usize;
        // the wordmark: 5 rows of block letters at the very top of the chat …
        for r in 0..5 {
            let row = &rows[chat_y + r];
            assert!(row.contains('█'), "wordmark row {r}: {row:?}");
            assert!(!row.contains("演示"), "the prompt comes after the wordmark: {row:?}");
        }
        // … centered like the splash, …
        let top = &rows[chat_y];
        assert_eq!(
            top.chars().take_while(|c| *c == ' ').count(),
            (80 - 26) / 2,
            "centered: {top:?}"
        );
        // … then blank padding (the chat is bottom-anchored) …
        assert!(rows[chat_y + 5].trim().is_empty(), "pad under the wordmark: {:?}", &rows[chat_y..chat_y + 8]);
        // … and the first message on the chat's last row, so the two share the screen
        let last = app.chat_area.bottom() as usize - 1;
        let tail: String = rows[last - 2..=last].concat().chars().filter(|c| !c.is_whitespace()).collect();
        assert!(tail.contains("演示一下这个功能"), "first message's bubble at the bottom: {:?}", &rows[last - 2..=last]);
        // the splash's guide lines are gone; the chat is what scrolls
        let flat: String = rows.concat().chars().filter(|c| !c.is_whitespace()).collect();
        assert!(!flat.contains("输入问题开始"), "hint gone: {flat}");
        // it is one chat line of 5 rows, first in the chat, and not a saved line
        let lines = app.chat_lines();
        assert!(lines[0].wordmark);
        assert_eq!(lines[0].text.lines().count(), 5);
        assert!(lines[1].text.contains("演示一下这个功能"));
        assert!(app.lines.iter().all(|l| !l.wordmark), "never in the saved lines");

        // clicking the wordmark does nothing (it is decoration)
        app.chat_press(0, 40);
        app.chat_release(0, 0, 40);
        assert!(app.prompt_menu.is_none());
        assert!(app.viewer.is_none());

        // it scrolls away with the chat
        app.push("回答".to_string(), Tone::Text);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(app.chat_rows.iter().filter(|r| r.wordmark).count() >= 5);
    }

    #[test]
    fn wordmark_leaves_the_chat_on_short_or_narrow_terminals_and_before_the_first_message() {
        use ratatui::{Terminal, backend::TestBackend};
        // short terminal (19 rows): the rows go to the conversation
        let mut app = App::new();
        app.push_user("你好");
        let mut terminal = Terminal::new(TestBackend::new(80, 19)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(!app.chat_lines().iter().any(|l| l.wordmark), "too short");
        let rows = screen_rows(&mut terminal);
        assert!(!rows.concat().contains('█'), "no wordmark rows on a 19-row terminal");

        // narrow terminal (29 columns): the wordmark would not fit
        let mut app = App::new();
        app.push_user("你好");
        let mut terminal = Terminal::new(TestBackend::new(29, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(!app.chat_lines().iter().any(|l| l.wordmark), "too narrow");

        // before the first message the splash (not the chat) carries the wordmark
        let mut app = App::new();
        app.push("tango serve · session 1234abcd", Tone::Muted);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(!app.chat_lines().iter().any(|l| l.wordmark), "splash, not chat");
    }

    #[test]
    fn splash_stays_with_startup_notices_listed_under_it() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.push(
            "tango serve · session 1234abcd · 20 tools · /help",
            Tone::Muted,
        );
        app.frame(
            json!({"type": "notice", "text": "⚠ 另一个 tango 会话正在此目录运行（pid 1）— 建议在不同 worktree 里并行"}),
            &mut serve,
        );
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        fn rows(t: &ratatui::Terminal<ratatui::backend::TestBackend>) -> Vec<String> {
            let b = t.backend().buffer();
            (0..24u16)
                .map(|y| {
                    (0..80u16)
                        .map(|x| b[(x, y)].symbol())
                        .collect::<String>()
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect()
                })
                .collect()
        }
        let flat = |r: &[String]| -> String {
            r.concat().chars().filter(|c| !c.is_whitespace()).collect()
        };
        terminal.draw(|f| app.draw(f)).unwrap();
        let r = rows(&terminal);
        assert!(flat(&r).contains('█'), "wordmark still shown: {r:?}");
        assert!(
            flat(&r).contains("另一个tango会话正在此目录运行"),
            "notice visible: {r:?}"
        );
        let hint = r.iter().position(|l| l.contains("输入问题开始")).unwrap();
        let warn = r.iter().position(|l| l.contains('⚠')).unwrap();
        assert!(warn > hint, "notice sits under the hint: {r:?}");
        // the first message ends the splash; the notice stays in the chat
        app.push_user("开始干活");
        terminal.draw(|f| app.draw(f)).unwrap();
        let r = rows(&terminal);
        assert!(!flat(&r).contains("输入问题开始"), "splash gone: {r:?}");
        assert!(
            flat(&r).contains("另一个tango会话正在此目录运行"),
            "notice kept: {r:?}"
        );
    }

    #[test]
    fn jump_pill_sits_centered_above_the_input_and_shrinks_on_narrow_screens() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        for n in 0..40 {
            app.push(format!("line-{n:02}"), Tone::Text);
        }
        app.unread_rows = 3;
        app.scroll_back = 5;
        let row_text = |t: &Terminal<TestBackend>, y: u16, w: u16| {
            let b = t.backend().buffer();
            (0..w).map(|x| b[(x, y)].symbol()).collect::<String>()
        };
        for w in [80u16, 30, 14] {
            let mut terminal = Terminal::new(TestBackend::new(w, 16)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
            let a = app.jump_area;
            assert_eq!(a.height, 1);
            assert_eq!(
                a.y,
                app.input_area.y - 1,
                "its own row right above the input"
            );
            assert!(
                (a.x + a.width / 2).abs_diff(w / 2) <= 1,
                "centered at width {w}: {a:?}"
            );
            assert!(a.width <= w, "fits width {w}");
            let text = row_text(&terminal, a.y, w);
            assert!(text.contains("↓"), "pill drawn at width {w}: {text}");
            // the chat row above it is untouched conversation text
            assert!(
                row_text(&terminal, a.y - 1, w).starts_with("line-"),
                "no overlay"
            );
        }
        // clicking it goes back to the bottom and the pill disappears
        let mut terminal = Terminal::new(TestBackend::new(30, 16)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let a = app.jump_area;
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: a.x + 1,
                row: a.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        terminal.draw(|f| app.draw(f)).unwrap();
        assert_eq!(app.scroll_back, 0);
        assert_eq!(app.jump_area, Rect::default(), "no pill at the bottom");
        assert!(row_text(&terminal, app.input_area.y - 1, 30).starts_with("line-"));
    }

    #[test]
    fn scrolled_up_header_names_the_prompt_above_and_click_jumps_to_it() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.push_user("first question");
        for i in 0..30 {
            app.push(format!("answer one line {i}"), Tone::Text);
        }
        app.push_user("second question");
        for i in 0..30 {
            app.push(format!("answer two line {i}"), Tone::Text);
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        let screen = |t: &Terminal<TestBackend>| {
            let b = t.backend().buffer();
            (0..b.area.height)
                .map(|y| {
                    (0..b.area.width)
                        .map(|x| b[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
        };

        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(
            app.prompt_header.is_none(),
            "following the bottom: no header"
        );

        // scroll into answer two: header names the second prompt
        app.scroll_back = 10;
        terminal.draw(|f| app.draw(f)).unwrap();
        let top = &screen(&terminal)[app.chat_area.y as usize];
        assert!(
            top.contains("❯ second question") && top.replace(' ', "").contains("点击跳到"),
            "{top}"
        );

        // further up into answer one: it changes to the first prompt
        app.scroll_back = 45;
        terminal.draw(|f| app.draw(f)).unwrap();
        let top = &screen(&terminal)[app.chat_area.y as usize];
        assert!(top.contains("❯ first question"), "{top}");

        // click it: the prompt itself shows right below the header, header goes away
        let (area, _) = app.prompt_header.unwrap();
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x + 2,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        terminal.draw(|f| app.draw(f)).unwrap();
        let rows = screen(&terminal);
        let at = rows
            .iter()
            .position(|r| r.contains("❯ first question"))
            .expect("prompt visible");
        assert!(
            at <= app.chat_area.y as usize + 2,
            "prompt lands at the top, row {at}"
        );
        assert!(
            app.scroll_back > 0,
            "still scrolled up, not jumped to bottom"
        );
    }

    #[test]
    fn clicking_the_input_chip_opens_the_image() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        let img = temp_image("inputclick");
        app.insert_paste(&img);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        // chip sits on the first input text row; click inside the label
        let row = app.input_area.y + 1;
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains(&format!("打开图片 {img}"))),
            "click opened the image"
        );
        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn clicking_a_chat_chip_opens_the_image() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        let img = temp_image("chatclick");
        app.insert_paste(&img); // registers image #1
        app.clear_input();
        app.push_user("[图片 #1] 这是什么？");
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        // locate the chip row and the label's display column within it
        let g = app
            .chat_rows
            .iter()
            .position(|r| r.text.contains("[图片 #1]"))
            .expect("chip row drawn");
        let row_text = &app.chat_rows[g].text;
        let label_char = row_text.find("[图片 #1]").unwrap();
        let col: usize = row_text[..label_char]
            .chars()
            .map(|c| display_width(&c.to_string()))
            .sum();
        let term_row = app.chat_area.y + (g - app.chat_scroll_offset) as u16;
        // the user line renders as a right-positioned bubble: add its indent + pad
        let bubble = app.chat_rows[g].x as usize + 2 * usize::from(app.chat_rows[g].user);
        // chips open on double-click (two press+release pairs), like file paths
        for _ in 0..2 {
            app.handle(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: (col + 1 + bubble) as u16,
                    row: term_row,
                    modifiers: KeyModifiers::NONE,
                }),
                &mut serve,
            );
            app.handle(
                Event::Mouse(crossterm::event::MouseEvent {
                    kind: MouseEventKind::Up(MouseButton::Left),
                    column: (col + 1 + bubble) as u16,
                    row: term_row,
                    modifiers: KeyModifiers::NONE,
                }),
                &mut serve,
            );
        }
        app.handle(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: (col + 1 + bubble) as u16,
                row: term_row,
                modifiers: KeyModifiers::NONE,
            }),
            &mut serve,
        );
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains(&format!("打开图片 {img}"))),
            "chat chip click opened the image"
        );
        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn session_switch_resets_image_ids() {
        let mut app = App::new();
        let img = temp_image("reset");
        app.insert_paste(&img);
        assert_eq!(app.images.len(), 1);
        app.switch_session(None);
        assert!(app.images.is_empty());
        assert_eq!(app.next_image, 1);
        let _ = std::fs::remove_file(&img);
    }

    #[test]
    fn typed_image_path_collapses_to_chip_at_separator() {
        let mut app = App::new();
        let dir = std::env::temp_dir().join("tango-typed-img");
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("shot.png");
        std::fs::write(&img, b"x").unwrap();
        // type the path char by char (no bracketed paste), then a space
        for c in format!("{}", img.display()).chars() {
            app.insert_char(c);
        }
        // before the separator it is still plain text
        assert!(app.rendered_input().text.contains("shot.png"));
        app.insert_char(' ');
        let rendered = app.rendered_input().text;
        assert!(
            rendered.contains("[图片 #1] "),
            "chip after separator: {rendered}"
        );
        assert!(!rendered.contains(".png"), "raw path collapsed: {rendered}");
        // submit: the upload rides along, chat line shows the marker not the path
        let mut serve = Serve::mock();
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["images"][0], json!(img.display().to_string()));
        assert!(sent["text"].to_string().contains("[图片 #1]"));
        assert!(!sent["text"].to_string().contains(".png"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bare_image_token_at_submit_still_uploads_via_sweep() {
        let mut app = App::new();
        let dir = std::env::temp_dir().join("tango-sweep-img");
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("final.png");
        std::fs::write(&img, b"x").unwrap();
        // typed WITHOUT a trailing separator — only the submit sweep catches it
        for c in format!("看这张 {}", img.display()).chars() {
            app.insert_char(c);
        }
        let (text, images) = app.take_submission();
        assert_eq!(images, vec![img.display().to_string()]);
        assert!(text.contains("[图片 1]"));
        assert!(!text.contains(".png"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
