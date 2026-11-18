use crate::child::{Event, Serve};
use crate::dialog::render_dialog;
use crate::keys::{Action, Keymap};
use crate::nvim::{EditResolution, Neovim, NvimRequest, NvimTarget, edit_submission};
use crate::theme::{Theme, Tone};
use crate::viewer::{PromptState, Viewer};
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};
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
    /// wall time recorded when the line was pushed ([HH:MM] for /timestamps)
    ts: Option<String>,
    /// full tool output or collapsed user prompt; shown in the independent viewer
    output: Option<String>,
    prompt_state: Option<PromptState>,
    source_index: usize,
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
    Branch,
    Tree,
    Find,
    Export,
    Transcript,
    Rename,
    Recap,
    Context,
    Queue,
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
    PluginsList,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PickerKind {
    Model,
    Session,
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
}

impl Picker {
    fn new(kind: PickerKind, items: Vec<PickItem>) -> Self {
        let sel = items.iter().position(|item| item.current).unwrap_or(0);
        Self {
            kind,
            items,
            sel,
            query: String::new(),
        }
    }

    fn filterable(&self) -> bool {
        matches!(
            self.kind,
            PickerKind::Model | PickerKind::Session | PickerKind::Command | PickerKind::History
        )
    }

    fn filtered_items(&self) -> Vec<&PickItem> {
        let query = self.query.trim().to_lowercase();
        self.items
            .iter()
            .filter(|item| {
                query.is_empty()
                    || [&item.id, &item.label, &item.note]
                        .into_iter()
                        .any(|text| text.to_lowercase().contains(&query))
            })
            .collect()
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
}

struct Popup {
    items: Vec<String>,
    descs: Vec<String>,
    sel: usize,
    kind: PopKind,
}

#[derive(Clone, Copy)]
struct PastedSpan {
    start: usize,
    end: usize,
    id: usize,
    extra_lines: usize,
}

struct InputDraft {
    input: Vec<char>,
    cursor: usize,
    pastes: Vec<PastedSpan>,
}

struct RenderedInput {
    text: String,
    cursor: usize,
}

struct InputLayout {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_col: usize,
}

const SLASH: &[(&str, &str)] = &[
    (
        "/branch",
        "就地分叉：保留到该回合、丢弃其后（原未来可用 /redo 找回）",
    ),
    ("/btw <q>", "临时提问当前上下文，不写入会话"),
    ("/clear", "清空屏幕（上下文保留）"),
    ("/compact", "压缩较早的上下文"),
    ("/cost", "本会话花费与上下文条数"),
    (
        "/density",
        "工具详情密度 0紧凑/1默认/2完整 · /density [0|1|2]",
    ),
    ("/diff", "只读查看工作区 diff · /diff --staged [paths]"),
    (
        "/editor",
        "用 $VISUAL/$EDITOR 编辑当前草稿（nvim 专用 /nvim）",
    ),
    ("/effort [low|medium|high|off]", "查看/设置推理力度"),
    ("/exit", "退出"),
    (
        "/export [md|html]",
        "导出完整会话（含被回滚的未来）为本地文件",
    ),
    ("/find <text>", "全库搜索对话（含被放弃的分支），可跳转"),
    ("/focus", "安静模式：隐藏成功工具与思考，保留错误"),
    ("/usage /stats", "跨会话聚合：按模型/会话的请求与花费"),
    ("/fork <name>", "从当前（或 /rewind 选中）位置复制出新会话"),
    (
        "/goal <text>|off|max N",
        "持续目标：每轮自动判断达成与否并续跑（上限轮数可调）",
    ),
    ("/rewind", "打开时间轴：代码+对话+上下文整体回滚"),
    ("/add_dir <dir>", "添加 cwd 之外的额外可写目录（持久化）"),
    (
        "/agents [role model]",
        "角色模型路由：driver/subagent/longContext（/bg 自动用 subagent）",
    ),
    ("/init", "生成 AGENTS.md 项目说明脚手架（--force 追加）"),
    ("/doctor", "环境体检：git/key/目录/版本"),
    ("/help", "命令列表"),
    ("/insights", "生成 HTML 使用报告（离线可读）"),
    (
        "/keymap",
        "显示当前按键映射（在 config.json 的 keys 里自定义）",
    ),
    ("/interrupt", "停止当前回合"),
    ("/model <id>", "切换模型（存为默认），裸 /model 打开列表"),
    ("/mode <m>", "ask | auto | readonly"),
    ("/mcp", "MCP 服务器与桥接工具状态"),
    ("/memory", "查看长期记忆（全局 AGENTS.md，ctrl+e 编辑）"),
    ("/remember <text>", "记住一条偏好 → 全局记忆，所有会话生效"),
    ("/mouse", "切换鼠标捕获 · /mouse on|off"),
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
    (
        "/preset <name>|save <name>",
        "提示模板：保存当前输入 / 填入输入框（不发送）",
    ),
    ("/nvim", "用外部 Neovim 编辑草稿 · ctrl+e 编辑当前草稿"),
    ("/quit", "退出"),
    ("/resume", "切换到历史会话，打开列表"),
    (
        "/skill [title]",
        "把本会话存成技能草稿 · /skill finish <file> 定稿",
    ),
    ("/skills", "列出全局/项目技能"),
    ("/status", "会话信息"),
    (
        "/subtask <任务>",
        "派子代理在独立 worktree 干活，完成后摘要写回本会话",
    ),
    (
        "/statusline [tpl|off]",
        "自定义状态栏模板：{model} {mode} {cost} {ctx} {pct} {session}",
    ),
    ("/theme", "选择并实时应用 dark/light/auto 主题"),
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
    ("/vim", "切换 Vim 编辑 · ctrl+s 发送 · :wq/:wqa 发送"),
    (
        "/worktree [new|rm|merge <name>]",
        "受管 git worktree：后台/子代理的隔离单元",
    ),
];

/// inline @path tokens are expanded into file contents at send time (Claude Code style)
const AT_FILE_CAP: usize = 6000;

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

/// Scan a rendered chat line for the first token that looks like and IS an existing
/// local file (absolute, ./…, ../… or ~/…). Clicking it opens the file viewer.
fn find_existing_file_path(text: &str) -> Option<String> {
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
        if expanded.len() > 1 && std::path::Path::new(&expanded).is_file() {
            best = Some(expanded);
            break;
        }
        i = j.max(i + 1);
    }
    best
}

/// One rendered terminal row of the chat: which source line, which char range, what text.
/// Owning the wrap (instead of ratatui's Paragraph) is what makes character-exact
/// mouse selection possible.
#[derive(Clone, Debug)]
struct ChatRow {
    line: usize,
    cs: usize, // char start (into the source line text)
    ce: usize, // char end, exclusive
    text: String,
}

/// Word wrap with char-index tracking; long words (URLs, CJK runs) hard-split at width.
fn wrap_chat_rows(texts: &[String], width: usize) -> Vec<ChatRow> {
    let mut rows = Vec::new();
    let w = width.max(1);
    for (line, text) in texts.iter().enumerate() {
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            rows.push(ChatRow {
                line,
                cs: 0,
                ce: 0,
                text: String::new(),
            });
            continue;
        }
        // split into tokens: runs of spaces stay attached to the preceding word end
        let mut cs = 0usize;
        let mut idx = 0usize;
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
            } else if let Some(sp) = last_space.filter(|&s| s > idx) {
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
    rows
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
    prompt_history: Vec<String>,
    stashed_draft: Option<InputDraft>,
    popup: Option<Popup>,
    picker: Option<Picker>,
    viewer: Option<Viewer>,
    chat_area: Rect,
    picker_area: Rect,
    /// visible picker window rows → item index (None = group header row)
    picker_rows: Vec<Option<usize>>,
    popup_area: Rect,
    jump_area: Rect,
    /// 0 = pinned to the newest line (follow mode); increases when scrolling back
    scroll_back: usize,
    last_visual_total: usize,
    unread_rows: usize,
    focus: bool,
    busy: bool,
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
    /// serve-side working directory (/cd moves it); @file resolves against it
    serve_cwd: Option<std::path::PathBuf>,
    /// in-chat mouse selection (terminal rows); release copies the covered lines
    sel_anchor: Option<u16>,
    sel_end: Option<u16>,
    /// rendered chat rows (self-managed wrap), captured each draw for mouse mapping
    chat_rows: Vec<ChatRow>,
    chat_scroll_offset: usize,
    chat_inner_w: u16,
    /// selection anchor/end as (terminal row, column) pairs — char-exact
    sel_anchor_pos: Option<(u16, u16)>,
    sel_end_pos: Option<(u16, u16)>,
    /// the user prompt currently being worked on — pinned in the status bar so
    /// switching between many agents never loses track of "which ask is this"
    current_prompt: Option<String>,
    /// plan mode badge (/plan)
    plan_mode: bool,
    /// active /goal text (badge)
    goal_text: Option<String>,
    /// tool detail level: 0 compact / 1 default / 2 full (/density)
    density: u8,
    pending: Vec<(u64, Pending)>,
    permission: Option<Permission>,
    model: String,
    mode: String,
    session: String,
    cost: f64,
    context_tokens: u64,
    context_window: u64,
    pub quit: bool,
    /// serve respawn request for the main loop: Some(Some(id)) resumes, Some(None) starts fresh
    pub respawn: Option<Option<String>>,
    pub err: Option<String>,
}

impl App {
    pub fn new() -> Self {
        Self {
            theme: Theme::dark(),
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
            prompt_history: Vec::new(),
            stashed_draft: None,
            popup: None,
            picker: None,
            viewer: None,
            chat_area: Rect::default(),
            picker_area: Rect::default(),
            picker_rows: Vec::new(),
            popup_area: Rect::default(),
            jump_area: Rect::default(),
            scroll_back: 0,
            last_visual_total: 0,
            unread_rows: 0,
            focus: false,
            busy: false,
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
            sel_anchor: None,
            sel_end: None,
            chat_rows: Vec::new(),
            chat_scroll_offset: 0,
            chat_inner_w: 0,
            sel_anchor_pos: None,
            sel_end_pos: None,
            current_prompt: None,
            plan_mode: false,
            goal_text: None,
            pending: Vec::new(),
            permission: None,
            model: "…".into(),
            mode: String::new(),
            session: String::new(),
            cost: 0.0,
            context_tokens: 0,
            context_window: 0,
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

    pub fn mouse_enabled(&self) -> bool {
        self.mouse
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

    /// switching context means a new serve child; wipe the old transcript and turn state
    fn switch_session(&mut self, target: Option<String>) {
        let keep: Vec<ChatLine> = self.lines.drain(..).last().into_iter().collect();
        self.lines = keep; // the "resuming…/new session" notice just pushed
        self.stream.clear();
        self.thinking.clear();
        self.pending.clear();
        self.btw_id = None;
        self.btw_title = None;
        self.permission = None;
        self.viewer = None;
        self.pending_viewer = None;
        self.edited = None;
        self.nvim_request = None;
        self.busy = false;
        self.cost = 0.0;
        self.context_tokens = 0;
        self.context_window = 0;
        self.scroll_back = 0;
        self.last_visual_total = 0;
        self.unread_rows = 0;
        self.jump_area = Rect::default();
        self.respawn = Some(target);
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
        Some((ra, ca as usize, rb, cb as usize))
    }

    /// The exact selected text: partial rows at both ends, full rows between.
    fn selected_text(&self, chat_y: u16) -> Option<String> {
        let (ra, ca, rb, cb) = self.selection_range(chat_y)?;
        let mut out = String::new();
        for g in ra..=rb.min(self.chat_rows.len().saturating_sub(1)) {
            let row = self.chat_rows.get(g)?;
            let chars: Vec<char> = row.text.chars().collect();
            let from = if g == ra { ca.min(chars.len()) } else { 0 };
            let to = if g == rb {
                cb.min(chars.len())
            } else {
                chars.len()
            };
            if g > ra {
                out.push('\n');
            }
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

    /// Left press in the chat area: a complete local file path under the cursor opens in
    /// the read-only viewer; anything else starts a character-exact selection.
    fn chat_press(&mut self, row: u16, column: u16, chat_y: u16) {
        if let Some(g) = self.global_row(row, chat_y) {
            if let Some(li) = self.chat_rows.get(g).map(|r| r.line) {
                let mut full = String::new();
                for r in &self.chat_rows {
                    if r.line == li {
                        full.push_str(&r.text);
                    }
                }
                if let Some(path) = find_existing_file_path(&full) {
                    self.open_file_viewer(&path);
                    return;
                }
            }
        }
        self.sel_anchor_pos = Some((row, column));
        self.sel_end_pos = Some((row, column));
    }

    fn chat_drag(&mut self, row: u16, column: u16) {
        if self.sel_anchor_pos.is_some() {
            self.sel_end_pos = Some((row, column));
        }
    }

    /// Release: copy exactly the dragged-over characters to the system clipboard.
    fn chat_release(&mut self, chat_y: u16) {
        let text = self.selected_text(chat_y);
        self.sel_anchor_pos = None;
        self.sel_end_pos = None;
        if let Some(text) = text
            && !text.trim().is_empty()
        {
            match self.copy_to_clipboard(&text) {
                Ok(()) => {
                    let n = text.chars().filter(|c| *c != '\n').count();
                    self.push(format!("已复制 {n} 个字符到剪贴板"), Tone::Success);
                }
                Err(e) => self.push(format!("复制失败：{e}"), Tone::Error),
            }
        }
    }

    /// Clickable file paths: show the file's content in the shared read-only viewer.
    fn open_file_viewer(&mut self, path: &str) {
        let trimmed = path.trim_end_matches(|c: char| ".,;:!?\"')]".contains(c));
        let line_free = trimmed.trim_end_matches(|c: char| c.is_ascii_digit() || c == ':');
        let candidate = std::path::Path::new(line_free);
        let target = if candidate.is_file() {
            line_free
        } else {
            trimmed
        };
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
                source_index: usize::MAX,
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
            source_index: usize::MAX,
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
                        item("internal", "使用内置查看器", "不启动外部编辑器"),
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
                line.output.as_ref().map(|output| {
                    (
                        Self::viewer_title(line),
                        output.clone(),
                        index,
                        line.prompt_state,
                    )
                })
            });
        if let Some((title, text, index, state)) = found {
            self.request_viewer(Viewer::with_source(title, text, index, state));
        }
    }

    fn open_transcript(&mut self) {
        let mut text = String::new();
        let mut prompt_rows = Vec::new();
        let mut row = 0;
        for line in self.chat_lines() {
            for (index, part) in line.text.split('\n').enumerate() {
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

    /// open_line_viewer that reports success, so a plain-text click can fall through
    /// to path-click / selection instead of silently doing nothing
    fn open_line_viewer_quiet(&mut self, index: usize) -> bool {
        let found = self.lines.get(index).and_then(|line| {
            line.output
                .as_ref()
                .map(|output| (Self::viewer_title(line), output.clone(), line.prompt_state))
        });
        match found {
            Some((title, text, state)) => {
                self.request_viewer(Viewer::with_source(title, text, index, state));
                true
            }
            None => false,
        }
    }

    fn open_line_viewer(&mut self, index: usize) {
        let found = self.lines.get(index).and_then(|line| {
            line.output
                .as_ref()
                .map(|output| (Self::viewer_title(line), output.clone(), line.prompt_state))
        });
        if let Some((title, text, state)) = found {
            self.request_viewer(Viewer::with_source(title, text, index, state));
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
        self.assistant_replies.push(s.trim_end().to_string());
        if self.assistant_replies.len() > 20 {
            self.assistant_replies.remove(0);
        }
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
        if !self.busy && self.pastes.is_empty() && text.starts_with('/') && !text.contains(' ') {
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
            let items = Self::file_candidates(&tok, self.serve_cwd.as_deref());
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

    fn toggle_draft_stash(&mut self) {
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
            "/queue" => {
                let id = serve.command("queue", vec![]);
                self.pending.push((id, Pending::Queue));
            }
            "/usage" | "/stats" => {
                let id = serve.command("usage", vec![]);
                self.pending.push((id, Pending::Usage));
            }
            "/insights" => {
                let id = serve.command("insights", vec![]);
                self.pending.push((id, Pending::Insights));
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
            "/pwd" => {
                let id = serve.command("pwd", vec![]);
                self.pending.push((id, Pending::Pwd));
            }
            "/cd" => {
                if rest.is_empty() {
                    self.push("usage: /cd <dir>", Tone::Muted);
                } else {
                    let id = serve.command("cd", vec![("dir", Value::String(rest.trim().into()))]);
                    self.pending.push((id, Pending::Cd));
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
                        "usage: /agents [driver|subagent|longContext] <model>",
                        Tone::Muted,
                    );
                }
            }
            "/subtask" => {
                if rest.is_empty() {
                    self.push(
                        "usage: /subtask <小任务描述> · 完成后摘要自动写回",
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
                let id = serve.command("mcp_list", vec![]);
                self.pending.push((id, Pending::McpList));
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
                if let Some(name) = arg.strip_prefix("new ") {
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
                    _ => "1 默认（错误显示输出）",
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
            "/rename" => {
                if rest.is_empty() {
                    self.push("usage: /rename <name>", Tone::Muted);
                } else {
                    let id = serve.command("rename", vec![("name", Value::String(rest.into()))]);
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
                    match crate::clipboard::copy(text) {
                        Ok(()) => self.push(format!("已复制助手回复（{n}）",), Tone::Success),
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
                                _ => "跟随终端 COLORFGBG",
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
            "/mode" => {
                if rest.is_empty() {
                    self.push("usage: /mode ask|auto|readonly", Tone::Muted);
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
        let cursor = self.cursor;
        self.replace_input_range(cursor, cursor, &[c]);
    }

    fn insert_paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
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
        });
        self.next_paste += 1;
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.pastes.clear();
        self.cursor = 0;
        self.popup = None;
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
        let mut raw = 0;
        let mut display = 0;
        for paste in pastes {
            while raw < paste.start {
                positions[raw] = display;
                text.push(self.input[raw]);
                raw += 1;
                display += 1;
            }
            positions[raw] = display;
            let label = if paste.extra_lines > 0 {
                format!("[Pasted text #{} +{} lines]", paste.id, paste.extra_lines)
            } else {
                format!("[Pasted text #{}]", paste.id)
            };
            display += label.chars().count();
            text.push_str(&label);
            while raw < paste.end {
                positions[raw] = display;
                raw += 1;
            }
            positions[raw] = display;
        }
        while raw < self.input.len() {
            positions[raw] = display;
            text.push(self.input[raw]);
            raw += 1;
            display += 1;
        }
        positions[self.input.len()] = display;
        RenderedInput {
            text,
            cursor: positions[self.cursor],
        }
    }

    fn input_layout(&self, width: u16) -> InputLayout {
        let rendered = self.rendered_input();
        let width = width.max(1) as usize;
        let mut lines = vec![String::new()];
        let mut row = 0;
        let mut col = 0;
        let mut cursor = None;
        for (index, c) in rendered.text.chars().enumerate() {
            if c == '\n' {
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
                row += 1;
                col = 0;
            }
            if index == rendered.cursor {
                cursor = Some((row, col));
            }
            lines[row].push(c);
            col += char_width;
        }
        if rendered.cursor == rendered.text.chars().count() {
            if col >= width {
                lines.push(String::new());
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
            cursor_row = lines.len() - 1;
        }
        InputLayout {
            lines,
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
                if let Some(viewer) = self.viewer.as_mut() {
                    match m.kind {
                        MouseEventKind::ScrollUp => viewer.scroll_by(-3, viewer.max_scroll),
                        MouseEventKind::ScrollDown => viewer.scroll_by(3, viewer.max_scroll),
                        _ => {}
                    }
                } else if self.permission.is_some() {
                    // permission answers are deliberate keyboard choices; never click through
                } else if self.picker.is_some() {
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
                            let row = m.row.saturating_sub(self.popup_area.y + 1) as usize;
                            if row < self.popup.as_ref().unwrap().items.len() {
                                self.popup.as_mut().unwrap().sel = row;
                                self.accept_popup();
                            }
                        }
                        _ => {}
                    }
                } else {
                    match m.kind {
                        MouseEventKind::ScrollUp => self.scroll_back += 3,
                        MouseEventKind::ScrollDown => self.scroll_chat_down(3),
                        // one ordered click handler: jump badge → line-with-output →
                        // clickable file path → text selection start (drag+release copies)
                        MouseEventKind::Down(MouseButton::Left) => {
                            let pos = Position::new(m.column, m.row);
                            if self.jump_area.contains(pos) {
                                self.jump_to_bottom();
                            } else if self.chat_area.contains(pos) {
                                // path click is the most specific intent; fall back to
                                // the line-output viewer, then to selection start
                                self.chat_press(m.row, m.column, self.chat_area.y);
                                if self.viewer.is_none() {
                                    let row = m.row.saturating_sub(self.chat_area.y) as usize;
                                    if let Some(index) = self.line_at_visual_row(
                                        self.chat_area.width as usize,
                                        self.chat_area.height as usize,
                                        row,
                                    ) {
                                        self.open_line_viewer_quiet(index);
                                    }
                                }
                            }
                        }
                        MouseEventKind::Drag(MouseButton::Left) => self.chat_drag(m.row, m.column),
                        MouseEventKind::Up(MouseButton::Left) => {
                            self.chat_release(self.chat_area.y)
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

    fn accept_picker(&mut self, serve: &mut Serve) {
        let picked = self
            .picker
            .as_ref()
            .and_then(|p| p.selected_item().map(|item| (p.kind, item)));
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
                PickerKind::Command => self.run_slash(&item.id, serve),
                PickerKind::Theme => self.run_slash(&format!("/theme {}", item.id), serve),
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
                            self.submit_text(serve, text);
                        }
                    }
                }
            }
        }
    }

    fn submit_text(&mut self, serve: &mut Serve, text: String) {
        // pin what we're working on: first line, capped for the status bar
        let first = text.lines().next().unwrap_or("").to_string();
        let shown: String = first.chars().take(48).collect();
        self.current_prompt = Some(shown);
        self.push_user(&text);
        self.jump_to_bottom();
        self.busy = true;
        self.prompt_id = serve.command("prompt", vec![("text", Value::String(text))]);
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
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if self.viewer.is_some() {
            // Ctrl+C always remains a safe interrupt even when the action is remapped —
            // and even while a viewer (e.g. an in-flight /btw answer) has the keys
            if self.busy && self.keys.matches(Action::Interrupt, k) {
                serve.notify(json!({ "cmd": "interrupt" }));
                self.goal_text = None; // serve clears the goal; mirror it immediately
                self.push("[interrupt requested]", Tone::Muted);
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
            if !ctrl && k.code == KeyCode::Char('c') {
                let result = crate::clipboard::copy(&self.viewer.as_ref().unwrap().text);
                match result {
                    Ok(()) => self.push("已复制查看器全文", Tone::Success),
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
                // copy the pending command to the clipboard without answering
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    if let Some(p) = self.permission.as_ref() {
                        let text = format!("{} {}", p.name, p.detail);
                        match self.copy_to_clipboard(&text) {
                            Ok(()) => self.push("命令已复制（y/n/a 待选）", Tone::Success),
                            Err(e) => self.push(format!("复制失败：{e}"), Tone::Error),
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        if ctrl && k.code == KeyCode::Char('t') {
            self.open_transcript();
            return;
        }
        if ctrl && k.code == KeyCode::Char('p') && self.viewer.is_none() {
            self.open_command_palette();
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
            if self.busy {
                self.push("[queued — ctrl+c interrupts]", Tone::Muted);
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
        if self.busy && (self.keys.matches(Action::Interrupt, k) || k.code == KeyCode::Esc) {
            serve.notify(json!({ "cmd": "interrupt" }));
            self.goal_text = None;
            self.push("[interrupt requested]", Tone::Muted);
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
                        let len = p.filtered_items().len();
                        p.sel = (p.sel + 1).min(len.saturating_sub(1));
                    }
                }
                KeyCode::Esc => {
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
                KeyCode::Enter | KeyCode::Tab => self.accept_picker(serve),
                _ => {
                    if let Some(p) = self.picker.as_mut() {
                        p.filter_key(k.code, ctrl);
                    }
                }
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
        if self.keys.matches(Action::Interrupt, k) || self.keys.matches(Action::ClearInput, k) {
            self.clear_input();
            return;
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
                if self
                    .input
                    .iter()
                    .collect::<String>()
                    .trim_start()
                    .starts_with('/') =>
            {
                let text: String = self.input.iter().collect();
                self.clear_input();
                self.run_slash(&text, serve); // commands stay live during a turn
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
            KeyCode::Enter => {
                self.push("[queued — ctrl+c interrupts]", Tone::Muted);
            }
            KeyCode::Char(c) if !ctrl && (!self.vim || self.edit_mode == EditMode::Insert) => {
                self.insert_char(c);
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
        self.clear_input();
        if text.trim().starts_with('/') || text.trim() == ":q" {
            self.run_slash(&text, serve);
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
        self.prompt_id = serve.command("prompt", vec![("text", Value::String(expanded))]);
        if let Some(line) = self.lines.last_mut() {
            line.prompt_state = Some(PromptState::Running);
        }
    }

    fn frame(&mut self, v: Value) {
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
                );
                self.mouse = v["mouse"].as_bool().unwrap_or(true);
                self.statusline = v["statusline"].as_str().map(|t| t.to_string());
                self.mouse_capture = Some(self.mouse);
                self.model = v["model"].as_str().unwrap_or("?").into();
                self.mode = v["mode"].as_str().unwrap_or("?").into();
                self.serve_cwd = v["cwd"].as_str().map(Into::into);
                self.session = v["sessionId"]
                    .as_str()
                    .unwrap_or("?")
                    .chars()
                    .take(8)
                    .collect();
                self.cost = v["totalCostUsd"].as_f64().unwrap_or(0.0);
                self.context_tokens = v["contextTokens"].as_u64().unwrap_or(0);
                self.context_window = v["contextWindow"].as_u64().unwrap_or(0);
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
            // chat-line notices from serve (warnings, goal/subtask status) — never part of a stream
            "notice" => {
                if let Some(text) = v["text"].as_str() {
                    self.push(text, Tone::Muted);
                    self.jump_to_bottom();
                }
            }
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
                    self.current_prompt = None; // the pin retires with the turn
                    // auto recap: first line of the answer, so a glance reconstructs
                    // what this long turn actually produced
                    if let Some(last) = self.assistant_replies.last() {
                        let first = last.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
                        let head: String = first.trim().chars().take(80).collect();
                        if !head.is_empty() {
                            self.push(format!("✓ 本轮：{head}"), Tone::Muted);
                        }
                    }
                    for line in &mut self.lines {
                        if line.prompt_state == Some(PromptState::Running) {
                            line.prompt_state = Some(PromptState::Completed);
                        }
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
                            self.picker = Some(Picker::new(PickerKind::Model, items));
                        }
                        (Pending::Sessions, true) => {
                            let items: Vec<PickItem> = d["sessions"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|s| {
                                            let id = s["id"].as_str()?.to_string();
                                            Some(PickItem {
                                                group: String::new(),
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
                                self.picker = Some(Picker::new(PickerKind::Session, items));
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
                            // badges follow the serve truth (goal cleared by interrupt etc.)
                            self.goal_text = d["goal"].as_str().map(|g| g.to_string());
                            self.plan_mode = d["plan"].as_bool().unwrap_or(self.plan_mode);
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
                            let items: Vec<PickItem> = d["nodes"]
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
                            let cur = self.session.clone();
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
                        (Pending::Queue, true) => {
                            let running = d["running"].as_str().unwrap_or("");
                            let queued: Vec<String> = d["queued"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|q| {
                                            let text = q["text"].as_str()?;
                                            let waited = q["waitedMs"].as_u64().unwrap_or(0);
                                            Some(format!("  · {text}（等待 {}s）", waited / 1000))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            let text = if running.is_empty() && queued.is_empty() {
                                "空闲 — 没有正在运行或排队的提示".to_string()
                            } else {
                                format!(
                                    "运行中：{}\n\n排队（{}）：\n{}",
                                    if running.is_empty() {
                                        "（无）"
                                    } else {
                                        running
                                    },
                                    queued.len(),
                                    if queued.is_empty() {
                                        "  （空）".to_string()
                                    } else {
                                        queued.join("\n")
                                    }
                                )
                            };
                            self.viewer = Some(Viewer::read_only("提示队列", text));
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
                                    "规则已添加并生效：{}",
                                    d["allow"].as_array().map(|a| a.len()).unwrap_or(0)
                                ),
                                Tone::Success,
                            );
                        }
                        (Pending::PermissionsRemove, true) => {
                            self.push("规则已移除", Tone::Muted);
                        }
                        (Pending::ConfigShow, true) => {
                            let path = d["path"].as_str().unwrap_or("?").to_string();
                            let cfg =
                                serde_json::to_string_pretty(&d["config"]).unwrap_or_default();
                            self.viewer = Some(Viewer::read_only(
                                "配置",
                                format!(
                                    "{path}\n\n{cfg}\n\n（直接编辑此文件；多数配置 /theme /mouse /statusline 等命令可代改）"
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
                            self.push(format!("cwd → {cwd}"), Tone::Success);
                        }
                        (Pending::AddDir, true) => {
                            let n = d["extraDirs"].as_array().map(|a| a.len()).unwrap_or(0);
                            self.push(format!("额外可写目录已保存（共 {n} 个）"), Tone::Success);
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
                            let e = d["effort"].as_str().unwrap_or("off");
                            self.push(format!("推理力度 → {e}"), Tone::Muted);
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
                                    let sess = t["sessionId"].as_str().unwrap_or("");
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
                                ("subagent", "/subtask 与 /bg 默认"),
                                ("longContext", "/bg --long 长上下文"),
                            ] {
                                text.push_str(&format!("  {role:<12} {} · {note}\n", get(role)));
                            }
                            text.push_str("\n/agents <role> <model> 设置");
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
                                Tone::Success,
                            );
                        }
                        (Pending::McpList, true) => {
                            let mut text = String::from("MCP 服务器：\n\n");
                            if let Some(list) = d["servers"].as_array() {
                                for s in list {
                                    let connected = s["connected"].as_bool().unwrap_or(false);
                                    text.push_str(&format!(
                                        "  {} {}\n",
                                        s["name"].as_str().unwrap_or("?"),
                                        if connected {
                                            "· 已连接"
                                        } else {
                                            "· 未连接（config.json 的 mcpServers 配置）"
                                        }
                                    ));
                                }
                            }
                            if text.trim() == "MCP 服务器：" {
                                text.push_str("  （无）· config.json 里配 mcpServers");
                            }
                            text.push_str("\n工具以 mcp_<server>_<tool> 形式出现在模型工具表里");
                            self.viewer = Some(Viewer::read_only("MCP", text));
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
                Some("user") => {
                    self.push_user(text);
                    if let Some(line) = self.lines.last_mut() {
                        line.prompt_state = Some(PromptState::Completed);
                    }
                }
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
        (self.input_layout(width).lines.len().min(8)) as u16 + 1 // +1 for the rule above the input
    }

    fn chat_height(&self, area: Rect, input_h: u16) -> usize {
        (area.height.saturating_sub(1 + input_h)) as usize
    }

    /// the chat exactly as drawn — scroll math and mouse hit testing use the same lines
    fn chat_lines(&self) -> Vec<ChatLine> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                !self.focus
                    || (line.tone != Tone::Thinking
                        && !(line.tone == Tone::Muted && line.text.starts_with("⚙ ")))
            })
            .map(|(source_index, line)| {
                let mut text = line.text.clone();
                // density: 0 = names only; 1 = errors show output (default); 2 = everything does
                let show_output = match self.density {
                    0 => false,
                    1 => line.tone == Tone::Error,
                    _ => true,
                };
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
                    source_index,
                }
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
        let lines = self.chat_lines();
        let total = Self::visual_lines(&lines, width);
        let offset = total.saturating_sub(chat_height + self.scroll_back);
        let target = offset + row;
        let mut top = 0;
        for line in &lines {
            let height = Self::visual_lines(std::slice::from_ref(line), width);
            if target >= top && target < top + height {
                return line.output.is_some().then_some(line.source_index);
            }
            top += height;
        }
        None
    }

    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let input_h = self.input_height(area.width);
        let chat_h = self.chat_height(area, input_h);
        let [chat_a, input_a, status_a] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(input_h),
            Constraint::Length(1),
        ])
        .areas(area);

        self.chat_area = chat_a;

        // status bar
        let spans = self.status_spans();
        f.render_widget(Paragraph::new(Line::from(spans)), status_a);

        // chat with bottom-anchored scrolling (scroll_back=0 follows the newest line).
        // We own the wrap (wrap_chat_rows) so every terminal row maps back to an exact
        // (source line, char range) — character-exact mouse selection and click targets.
        let inner_w = chat_a.width as usize;
        let mut texts: Vec<String> = self.chat_lines().iter().map(|l| l.text.clone()).collect();
        if !self.stream.is_empty() {
            // live assistant text (not committed yet) hangs off the last line, markdown stripped
            texts.push(self.stream.replace("**", "").replace('`', ""));
        }
        let tones: Vec<Tone> = self.chat_lines().iter().map(|l| l.tone).collect();
        let all_rows = wrap_chat_rows(&texts, inner_w);
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
        self.chat_inner_w = chat_a.width;

        // render the visible window row by row with char-exact selection highlight
        let sel = self.selection_range(chat_a.y);
        let mut shown: Vec<Line> = Vec::new();
        for g in offset..total.min(offset + chat_h as usize) {
            let Some(row) = self.chat_rows.get(g) else {
                continue;
            };
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
            if sel.is_some() && s_to > s_from {
                shown.push(Line::from(vec![
                    Span::styled(chars[..s_from].iter().collect::<String>(), style),
                    Span::styled(
                        chars[s_from..s_to].iter().collect::<String>(),
                        style.reversed(),
                    ),
                    Span::styled(chars[s_to..].iter().collect::<String>(), style),
                ]));
            } else {
                shown.push(Line::from(Span::styled(row.text.clone(), style)));
            }
        }
        f.render_widget(Paragraph::new(Text::from(shown)), chat_a);
        self.jump_area = Rect::default();
        if self.unread_rows > 0 && chat_a.height > 0 {
            let label = format!(" ↓ {} 行新内容 · ctrl+end 回到底部 ", self.unread_rows);
            let width = (display_width(&label) as u16).min(chat_a.width);
            self.jump_area = Rect {
                x: chat_a.x + chat_a.width.saturating_sub(width),
                y: chat_a.y + chat_a.height - 1,
                width,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(label).style(self.theme.style(Tone::Badge)),
                self.jump_area,
            );
        }

        // completion popup, glued above the input rule
        if let Some(p) = &self.popup {
            let ph = (p.items.len().min(6) + 2) as u16;
            let pop = Rect {
                x: 0,
                y: input_a.y.saturating_sub(ph),
                width: area.width,
                height: ph,
            };
            let title = match p.kind {
                PopKind::Slash => "命令补全",
                PopKind::File => "文件引用",
            };
            self.popup_area = pop;
            let inner = render_dialog(f, pop, title, "tab/enter 选择 · esc 关闭", self.theme);
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
            f.render_widget(Paragraph::new(rows), inner);
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
        let layout = self.input_layout(area.width);
        let visible = input_a.height.saturating_sub(1).max(1) as usize;
        let input_scroll = layout.cursor_row.saturating_sub(visible - 1);
        f.render_widget(
            Paragraph::new(layout.lines.into_iter().map(Line::from).collect::<Vec<_>>())
                .scroll((input_scroll as u16, 0)),
            Rect {
                x: 0,
                y: input_a.y + 1,
                width: area.width,
                height: input_a.height.saturating_sub(1),
            },
        );
        f.set_cursor_position(Position::new(
            input_a.x + layout.cursor_col as u16,
            input_a.y + 1 + (layout.cursor_row - input_scroll) as u16,
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
            let pagination = usize::from(display.len() > 14);
            let query_rows = usize::from(p.filterable());
            let h = (display.len().min(14) + pagination + query_rows + 2) as u16;
            let w = (area.width.min(72)).max(44).min(area.width);
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            let title = match p.kind {
                PickerKind::Model => "选择模型",
                PickerKind::Session => "选择会话",
                PickerKind::Timeline => "时间轴 · 选中即回滚到该回合之前",
                PickerKind::Find => "搜索命中 · 本会话=回滚到该处 · 他会话=切换过去",
                PickerKind::Preset => "提示模板 · 选中填入输入框（不发送）",
                PickerKind::Permissions => "允许规则 · /permissions rm <rule> 移除",
                PickerKind::Skills => "技能列表 · global=全局 project=项目",
                PickerKind::Worktree => "worktree · /worktree merge <名> 合并 · rm 删除",
                PickerKind::Command => "命令面板",
                PickerKind::History => "搜索 prompt 历史",
                PickerKind::Theme => "选择主题",
                PickerKind::NvimBackend => "检测到 Neovim，如何查看？",
                PickerKind::EditResolution => "内容已修改，如何处理？",
            };
            let hint = if p.filterable() {
                "输入 过滤 · ↑↓/滚轮 选择 · enter 确认 · esc 取消"
            } else {
                match p.kind {
                    PickerKind::NvimBackend => "↑↓/滚轮 选择 · enter 确认 · esc 使用内置查看器",
                    PickerKind::EditResolution => "↑↓ 选择 · enter 提交 · esc 取消",
                    _ => "↑↓ 选择 · enter 确认 · esc 取消",
                }
            };
            let inner = render_dialog(f, pop, title, hint, self.theme);
            let label_w = (inner.width as usize).saturating_sub(28);
            let sel_row = display
                .iter()
                .position(|r| matches!(r, PickRow::Item(i) if *i == p.sel))
                .unwrap_or(0);
            let start = if sel_row < 13 { 0 } else { sel_row - 12 };
            self.picker_area = pop;
            let win = &display[start..(start + 14).min(display.len())];
            self.picker_rows = win
                .iter()
                .map(|r| match r {
                    PickRow::Item(i) => Some(*i),
                    PickRow::Header(_) => None,
                })
                .collect();
            let mut rows: Vec<Line> = win
                .iter()
                .map(|slot| match slot {
                    PickRow::Header(g) => Line::from(Span::styled(
                        format!(" ── {} ──", g),
                        self.theme.style(Tone::Muted),
                    )),
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
                        let pad = label_w.saturating_sub(display_width(&it.label));
                        let note: String = it
                            .note
                            .chars()
                            .take((inner.width as usize).saturating_sub(label_w + 9))
                            .collect();
                        Line::from(Span::styled(
                            format!(" {marks} {}{} {}", it.label, " ".repeat(pad), note),
                            style,
                        ))
                    }
                })
                .collect();
            if p.filterable() {
                rows.insert(
                    0,
                    Line::from(Span::styled(
                        format!(" 搜索: {}", p.query),
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
            let w = (area.width.min(72)).max(40).min(area.width);
            let h = 7.min(area.height);
            let popup = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            let inner = render_dialog(
                f,
                popup,
                &p.name,
                "y 允许 · n 拒绝 · a 始终允许 · c 复制命令",
                self.theme,
            );
            let body = Paragraph::new(vec![
                Line::from(Span::styled(
                    p.reason.clone(),
                    self.theme.style(Tone::Warning),
                )),
                Line::from(""),
                Line::from(Span::raw(p.detail.chars().take(240).collect::<String>())),
            ])
            .wrap(Wrap { trim: true });
            f.render_widget(body, inner);
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
            let title = viewer.title.clone();
            let hint = if viewer.transcript {
                "/ 搜索 · n/N 匹配 · {/} prompt · c 复制 · q 关闭"
            } else {
                "c 复制 · esc 关闭 · ctrl+e 编辑(nvim) · ↑↓/滚轮滚动"
            };
            let inner = render_dialog(f, pop, &title, hint, self.theme);
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
    }

    /// Status bar spans; a custom {placeholder} template replaces the whole bar.
    fn status_spans(&mut self) -> Vec<Span<'static>> {
        let work = if self.busy { "● working" } else { "idle" };
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
                .replace("{work}", work);
            return vec![Span::styled(
                format!(" {} ", raw.chars().take(200).collect::<String>()),
                self.theme.style(Tone::Badge),
            )];
        }
        let mut spans = vec![
            Span::styled(format!(" {} ", self.model), self.theme.style(Tone::Badge)),
            Span::styled(format!(" {} ", self.mode), self.theme.style(Tone::Muted)),
            Span::styled(
                format!(" ▶ {} ", self.current_prompt.as_deref().unwrap_or("?")),
                self.theme.style(Tone::Badge),
            ),
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
        if self.context_window > 0 {
            let percent = self.context_tokens.saturating_mul(100) / self.context_window;
            spans.push(Span::styled(
                format!(
                    " ctx {}/{} {}% ",
                    Self::token_count(self.context_tokens),
                    Self::token_count(self.context_window),
                    percent
                ),
                self.theme.style(self.context_tone()),
            ));
        }
        if self.focus {
            spans.push(Span::styled(" focus ", self.theme.style(Tone::Badge)));
        }
        if self.plan_mode {
            spans.push(Span::styled(" PLAN ", self.theme.style(Tone::Warning)));
        }
        if self.goal_text.is_some() {
            spans.push(Span::styled(" ●goal ", self.theme.style(Tone::Badge)));
        }
        if self.vim {
            let mode = if self.vim_command.is_some() {
                "COMMAND"
            } else if self.edit_mode == EditMode::Insert {
                "INSERT"
            } else {
                "NORMAL"
            };
            spans.push(Span::styled(
                format!(" VIM {mode} "),
                self.theme.style(Tone::Badge),
            ));
        }
        spans
    }

    /// plain text of the status bar (tests and /statusline preview)
    fn status_segments_text(&mut self) -> String {
        self.status_spans()
            .iter()
            .map(|s| s.content.to_string())
            .collect::<Vec<_>>()
            .join("")
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
        app.open_latest_viewer();
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

    #[test]
    fn multiline_input_cursor_matches_the_actual_wrapped_rows() {
        use ratatui::{
            Terminal,
            backend::{Backend, TestBackend},
        };

        let mut app = App::new();
        app.input = "one two three four five".chars().collect();
        app.cursor = app.input.len();
        let mut terminal = Terminal::new(TestBackend::new(12, 10)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();

        assert_eq!(app.input_height(12), 3);
        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            Position::new(11, 8)
        );
        let buffer = terminal.backend().buffer();
        let row = |y| (0..12).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert_eq!(row(7), "one two thre");
        assert_eq!(row(8), "e four five ");
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
            Position::new(7, 12)
        );
        let buffer = terminal.backend().buffer();
        let row = |y| (0..20).map(|x| buffer[(x, y)].symbol()).collect::<String>();
        assert!(row(5).starts_with("line-3"));
        assert!(row(12).starts_with("line-10"));
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
        assert!(
            row(&terminal, 0).starts_with("line-06"),
            "unexpected first row: {:?}",
            row(&terminal, 0)
        );

        app.push("line-20", Tone::Text);
        app.push("line-21", Tone::Text);
        app.push("line-22", Tone::Text);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(row(&terminal, 0).starts_with("line-06"));
        let buffer = terminal.backend().buffer();
        let screen = (0..12)
            .map(|y| (0..40).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            screen.replace(' ', "").contains("3行新内容"),
            "screen:\n{screen}"
        );

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
        app.frame(json!({
            "type": "tool_result", "call": {"name": "bash", "input": {"command": "fail"}},
            "isError": true, "ms": 3, "output": "first failure\nsecond failure"
        }));
        app.frame(json!({
            "type": "tool_result", "call": {"name": "bash", "input": {"command": "ok"}},
            "isError": false, "ms": 1, "output": "successful output"
        }));
        let rendered = app.chat_lines();
        assert!(
            rendered
                .iter()
                .any(|line| line.text.contains("first failure"))
        );
        assert!(
            rendered
                .iter()
                .any(|line| line.text.contains("second failure"))
        );
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
        assert!(
            focused
                .iter()
                .any(|line| line.text.contains("first failure"))
        );

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
        app.frame(json!({
            "type": "hello", "sessionId": "12345678", "model": "mock/mock", "mode": "ask",
            "contextWindow": 200, "contextTokens": 40, "tools": []
        }));
        assert_eq!(app.context_tone(), Tone::Success);
        app.frame(json!({
            "type": "turn_end", "usage": {"input": 160, "output": 2},
            "contextWindow": 200, "contextTokens": 160
        }));
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
        app.frame(json!({
            "type": "result", "id": sent["id"], "ok": true,
            "data": {"diff": "@@ -1 +1,2 @@\n old\n+new", "staged": false}
        }));
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

        app.frame(json!({
            "type": "btw_stream", "id": sent["id"],
            "event": {"type": "text", "delta": "side answer"}
        }));
        assert_eq!(app.viewer.as_ref().unwrap().text, "side answer");
        assert!(app.lines.is_empty());
        app.frame(json!({
            "type": "result", "id": sent["id"], "ok": true,
            "data": {"text": "complete side answer"}
        }));
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
        app.frame(json!({ "type": "result", "id": id, "ok": false, "error": "aborted" }));
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
        app.frame(json!({
            "type": "result", "id": id, "ok": true,
            "data": {"text": "the answer"}
        }));
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
        app.frame(json!({
            "type": "hello", "sessionId": "12345678", "model": "mock/mock",
            "mode": "ask", "editor": "default", "tools": []
        }));
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
        }));
        assert!(app.picker.as_ref().unwrap().kind == PickerKind::Timeline);
        let picker = app.picker.as_ref().unwrap();
        assert_eq!(picker.items.len(), 2);
        assert!(picker.items[0].label.contains("fix the bug"));
        assert!(picker.items[1].note.contains("无代码快照"));

        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        assert!(app.picker.is_none());
        assert_eq!(serve.sent.len(), 2);
        let rew: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(rew["cmd"], "rewind");
        assert_eq!(rew["entryIndex"], 1);
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
        }));
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
    fn fork_result_triggers_a_session_respawn() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/fork alt", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "fork");
        assert_eq!(sent["name"], "alt");
        let id = sent["id"].as_u64().unwrap();
        app.frame(json!({
            "type": "result", "id": id, "ok": true,
            "data": {"sessionId": "forked-123", "name": "alt"}
        }));
        // a respawn is requested with the fork's session id, like /resume switching
        assert_eq!(app.respawn, Some(Some("forked-123".into())));
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
        }));
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
        }));
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
        }));
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
        app.frame(json!({"type": "result", "id": t1["id"], "ok": true, "data": {"text": "⟲ user: lost future"}}));
        assert!(app.viewer.as_ref().unwrap().read_only);
        assert!(app.viewer.as_ref().unwrap().text.contains("lost future"));
        // export → chat line with the path
        app.run_slash("/export md", &mut serve);
        let t2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(t2["cmd"], "export");
        app.frame(json!({"type": "result", "id": t2["id"], "ok": true, "data": {"path": "/tmp/x.md", "format": "md"}}));
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
        }));
        let viewer = app.viewer.as_ref().unwrap();
        assert!(viewer.read_only);
        assert!(viewer.text.contains("system"));
        assert!(viewer.text.contains("history"));
        assert!(viewer.text.contains("27%"));
        assert!(viewer.text.contains("5520"));
    }

    #[test]
    fn queue_command_shows_running_and_queued() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/queue", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "queue");
        app.frame(json!({
            "type": "result", "id": sent["id"], "ok": true,
            "data": {"running": "first prompt", "queued": [{"text": "second prompt", "waitedMs": 4200}]}
        }));
        let viewer = app.viewer.as_ref().unwrap();
        assert!(viewer.read_only);
        assert!(viewer.text.contains("first prompt"));
        assert!(viewer.text.contains("second prompt"));
        assert!(viewer.text.contains("4"));
        // empty state
        app.run_slash("/queue", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        app.frame(json!({
            "type": "result", "id": sent2["id"], "ok": true,
            "data": {"running": null, "queued": []}
        }));
        assert!(app.viewer.as_ref().unwrap().text.contains("空闲"));
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
        }));
        let viewer = app.viewer.as_ref().unwrap();
        assert!(viewer.read_only);
        assert!(viewer.text.contains("mock/mock"));
        assert!(viewer.text.contains("7"));
        assert!(viewer.text.contains("0.1234"));

        app.run_slash("/insights", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "insights");
        app.frame(json!({"type": "result", "id": sent2["id"], "ok": true, "data": {"path": "/tmp/ins.html"}}));
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
        app.push_tool("⚙ bash bad", Tone::Error, Some("boom\ntrace".into()));

        // density 1 (default): error rows show their first output lines
        let lines = app.chat_lines();
        assert!(
            lines.iter().any(|l| l.text.contains("boom")),
            "errors visible at density 1"
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
        );
        assert!(app.lines.iter().any(|l| l.text.contains("50%")));

        app.run_slash("/autocompact 60%", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "set_autocompact");
        assert_eq!(sent2["value"], "60%");
        app.frame(
            json!({"type": "result", "id": sent2["id"], "ok": true, "data": {"autocompact": 0.6}}),
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
        assert!(default.contains("m1"), "no template: default status");

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
        app.frame(json!({
            "type": "result", "id": sent2["id"], "ok": true,
            "data": {"presets": [{"name": "open", "preview": "固定开场白文本"}]}
        }));
        assert!(app.picker.as_ref().unwrap().kind == PickerKind::Preset);
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut serve,
        );
        let get: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(get["cmd"], "preset_get");
        app.frame(json!({
            "type": "result", "id": get["id"], "ok": true, "data": {"text": "固定开场白文本"}
        }));
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
        }));
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
    fn config_keymap_cd_pwd_add_dir_wire_up() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/config", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "config_show");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
            "data": {"config": {"theme": "dark", "extraDirs": ["/x"]}, "path": "/home/config.json"}}));
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
        app.frame(json!({"type": "result", "id": sent3["id"], "ok": true, "data": {"cwd": "/w"}}));
        app.run_slash("/cd sub", &mut serve);
        let sent4: Value = serde_json::from_str(&serve.sent[2]).unwrap();
        assert_eq!(sent4["cmd"], "cd");
        assert_eq!(sent4["dir"], "sub");
    }

    #[test]
    fn init_and_doctor_wire_up() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/init", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "init");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true, "data": {"path": "/w/AGENTS.md"}}));
        assert!(app.lines.iter().any(|l| l.text.contains("AGENTS.md")));

        app.run_slash("/doctor", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "doctor");
        app.frame(json!({"type": "result", "id": sent2["id"], "ok": true,
        "data": {"checks": [
            {"name": "git", "ok": true, "detail": "repo detected"},
            {"name": "model key", "ok": false, "detail": "no key"}
        ]}}));
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
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true, "data": {"path": "/w/d.zip", "format": "zip"}}));
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
    fn plan_command_toggles_and_shows_badge() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/plan", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "set_plan");
        assert_eq!(sent["on"], true);
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true, "data": {"plan": true}}));
        assert!(app.plan_mode);
        let spans = app.status_spans();
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("PLAN"), "status bar shows PLAN badge: {text}");
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
        );
        assert!(app.lines.iter().any(|l| l.text.contains("high")));
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
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
            "data": {"remembered": "用户偏好中文", "path": "/h/AGENTS.md"}}));
        assert!(app.lines.iter().any(|l| l.text.contains("已记住")));

        app.run_slash("/memory", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "memory");
        app.frame(json!({"type": "result", "id": sent2["id"], "ok": true,
            "data": {"text": "- 用户偏好中文\n", "path": "/h/AGENTS.md"}}));
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
            ]}}));
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
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
            "data": {"goal": "完成示例", "maxRounds": 5}}));
        assert!(app.goal_text.is_some());
        let spans = app.status_spans();
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("goal"), "badge shows: {text}");
        app.run_slash("/goal off", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["text"], "off");
        app.frame(json!({"type": "result", "id": sent2["id"], "ok": true, "data": {"goal": null}}));
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
            "data": {"worktrees": [{"name": "exp1", "path": "/w/x", "branch": "tango/exp1", "dirty": 2, "createdAt": "t"}]}}));
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
            json!({"type": "result", "id": sent["id"], "ok": true, "data": {"task": {"id": "abc", "status": "running"}}}),
        );
        assert!(app.lines.iter().any(|l| l.text.contains("abc")));

        app.run_slash("/tasks", &mut serve);
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["cmd"], "tasks");
        app.frame(
            json!({"type": "result", "id": sent2["id"], "ok": true, "data": {"tasks": [
                {"id": "abc", "status": "done", "model": "m", "prompt": "后台检查", "sessionId": "sess1"}
            ]}}),
        );
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
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
                "data": {"roles": {"subagent": "mock/flash"}, "driver": "mock/mock"}}));
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
    fn subtask_command_sends() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/subtask 修个测试", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "subtask");
        assert_eq!(sent["prompt"], "修个测试");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
                "data": {"task": {"id": "s1", "status": "running", "worktreeName": "bg-1"}}}));
        assert!(app.lines.iter().any(|l| l.text.contains("s1")));
    }

    #[test]
    fn mcp_command_lists_servers() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/mcp", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "mcp_list");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
                "data": {"servers": [{"name": "demo", "connected": true}]}}));
        let v = app.viewer.as_ref().unwrap();
        assert!(v.text.contains("demo"));
        assert!(v.text.contains("已连接"));
    }

    #[test]
    fn plugins_command_lists() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/plugins", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "plugins_list");
        app.frame(json!({"type": "result", "id": sent["id"], "ok": true,
                "data": {"plugins": [{"name": "demo", "description": "测试", "enabled": true}]}}));
        assert!(app.viewer.as_ref().unwrap().text.contains("demo"));
    }

    #[test]
    fn notice_frames_render_as_chat_lines() {
        let mut app = App::new();
        app.frame(json!({"type": "notice", "text": "⚠ 另一个会话正在此目录运行"}));
        assert!(app.lines.iter().any(|l| l.text.contains("另一个会话")));
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
        });
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            &mut serve,
        );
        assert!(app.permission.is_some(), "modal stays open after copy");
        assert!(app.lines.iter().any(|l| l.text.contains("命令已复制")));
        assert!(serve.sent.is_empty(), "no answer was sent");
    }

    #[test]
    fn find_existing_file_path_click_targets() {
        let dir = std::env::temp_dir().join("tango-click");
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("real-file.txt");
        std::fs::write(&real, "x").unwrap();
        // found: absolute path with trailing punctuation and a :line suffix
        let line = format!("see {} failed", real.display());
        assert_eq!(
            find_existing_file_path(&line),
            Some(real.display().to_string())
        );
        // ~ expansion
        let home = std::env::var("HOME").unwrap_or_default();
        let homefile = std::fs::canonicalize(&real).ok(); // skip if not under home
        if homefile.is_none() {
            let hf = std::path::Path::new(&home).join("tango-click-home-probe.txt");
            std::fs::write(&hf, "x").unwrap();
            let l2 = format!("open ~/{} now", hf.file_name().unwrap().to_string_lossy());
            assert!(
                find_existing_file_path(&l2).is_some(),
                "home expansion works"
            );
        }
        // not a file → no match, no panic
        assert_eq!(find_existing_file_path("no paths here at all"), None);
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
        app.chat_release(0);
        assert!(app.sel_anchor_pos.is_none());
        assert!(app.lines.iter().any(|l| l.text.contains("已复制 5 个字符")));
    }

    #[test]
    fn chat_press_opens_viewer_for_real_file_click() {
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
        app.chat_press(1, 30, 0); // press on the prompt row
        let v = app.viewer.as_ref().expect("viewer opened");
        assert!(v.read_only);
        assert!(v.title.contains("target.md"));
        assert_eq!(v.text, "CONTENT");
        assert!(
            app.sel_anchor_pos.is_none(),
            "no selection started for a path click"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
        let spans = app.status_spans();
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.contains("▶ 帮我优化数据库查询性能的问题"),
            "pinned in status: {text}"
        );

        // turn completes: recap line from the answer's first line, pin context ends
        app.assistant_replies
            .push("已将慢查询加了索引，延迟从 2s 降到 50ms。\n细节略".into());
        app.frame(json!({"type": "result", "id": app.prompt_id, "ok": true, "data": {}}));
        assert!(!app.busy);
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("✓ 本轮：已将慢查询加了索引")),
            "auto recap pushed"
        );
        // pinned prompt span stops showing after completion
        let spans2 = app.status_spans();
        let text2: String = spans2.iter().map(|s| s.content.to_string()).collect();
        assert!(
            !text2.contains("▶ 帮我优化"),
            "pin cleared after completion: {text2}"
        );
    }
}
