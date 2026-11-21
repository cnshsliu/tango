use crate::child::{Event, Serve};
use crate::dialog::render_dialog;
use crate::keys::{Action, Keymap};
use crate::nvim::{EditResolution, Neovim, NvimRequest, NvimTarget, edit_submission};
use crate::theme::{Theme, Tone};
use crate::viewer::{PromptState, Viewer};
use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
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
    PluginsList,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum PickerKind {
    Model,
    Session,
    /// /handoff without an id: pick the session to send this one's brief to
    HandoffTarget,
    /// /takeover without an id: pick the session whose work this one continues
    TakeoverSource,
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
        }
    }

    fn session_scoped(kind: PickerKind) -> bool {
        matches!(
            kind,
            PickerKind::Session | PickerKind::HandoffTarget | PickerKind::TakeoverSource
        )
    }

    fn filterable(&self) -> bool {
        matches!(
            self.kind,
            PickerKind::Model
                | PickerKind::Session
                | PickerKind::HandoffTarget
                | PickerKind::TakeoverSource
                | PickerKind::Command
                | PickerKind::History
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
const SLASH: &[(&str, &str)] = &[
    (
        "/branch",
        "就地分叉：保留到该回合、丢弃其后（原未来可用 /redo 找回）",
    ),
    ("/btw <q>", "临时提问当前上下文，不写入会话"),
    (
        "/todo",
        "待定列表：模型等你选的 ❓ 与顺带建议 · 点击逐条处理",
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
    ("/effort [low|medium|high|off]", "查看/设置推理力度"),
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
    ("/cd <dir>", "切换会话工作目录（工具与快照跟随）"),
    ("/pwd", "显示当前工作目录"),
    (
        "/agents [role model]",
        "角色模型路由：driver/subagent/longContext（/bg 自动用 subagent）",
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
    ("/mode <m>", "ask | auto | readonly"),
    ("/plan", "计划模式开关：禁止一切写操作，只出执行计划"),
    (
        "/permissions [add|rm <rule>]",
        "允许规则：tool:<名> · path:<前缀> · bash:<命令前缀>",
    ),
    ("/mcp", "MCP 服务器与桥接工具状态"),
    ("/plugins", "本地插件列表（~/.tango/plugins）"),
    ("/memory", "查看长期记忆（全局 AGENTS.md，ctrl+e 编辑）"),
    ("/remember <text>", "记住一条偏好 → 全局记忆，所有会话生效"),
    ("/mouse", "切换鼠标捕获 · /mouse on|off"),
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
        "/queue [rm <n>|clear]",
        "本地排队管理 · Alt+Enter 补充到正在处理的回合",
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
    (
        "/skill [title]",
        "把本会话存成技能草稿 · /skill finish <file> 定稿",
    ),
    ("/save-as-skill", "同 /skill"),
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
    ("/redo", "前进一个回合（撤销 /undo）"),
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
    "/todo 处理模型的选项与建议（❓ 和 ?suggest 收在这里）",
    "选项面板按 tab 改写自答 · esc 只收起面板",
    "ctrl+o 展开最近工具输出 · /focus 安静模式",
    "ctrl+r 历史提示 · ctrl+p 命令面板 · ctrl+e 编辑草稿",
    "/rewind 时间轴回滚代码+对话 · /export 导出会话",
];

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

/// A clickable action in the /todo dialog.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TodoAction {
    Expand(usize), // re-open this unanswered choice's option panel
    Run(usize),    // act on a suggestion: send "执行这条建议" as the next prompt
    Reply(usize),  // preload a heuristic question into the input box for answering
    Drop(usize),   // dismiss without acting
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
    /// thinking rows carry a left "┃ " gutter whose color scrolls while busy
    think: bool,
    /// rounded top/bottom frame row of a user bubble — styled, never selectable
    border: bool,
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
const THINK_FRAME_MS: u128 = 120;

/// Color of the thinking bar cell at global row `g` on animation frame `frame`
/// (~120ms apart). `animate` false (idle, or no truecolor) → flat dim gray.
fn thinking_bar_color(frame: u128, local_row: usize, animate: bool) -> Color {
    if !animate {
        return Color::DarkGray;
    }
    let phase = (frame + THINK_WAVE.len() as u128).wrapping_sub(local_row as u128)
        % THINK_WAVE.len() as u128;
    THINK_WAVE[phase as usize]
}

/// The running gear's color cycle: same cyan family as the thinking wave, one
/// step every 2 frames (~240ms). Without truecolor it alternates dark-gray and
/// cyan, which is the most any 256-color terminal can express.
fn gear_color(frame: u128, animate: bool) -> Color {
    if !animate {
        return Color::DarkGray;
    }
    if !supports_truecolor() {
        return if (frame / 2) % 2 == 0 { Color::Cyan } else { Color::DarkGray };
    }
    const GEAR_WAVE: [Color; 4] = [
        Color::Rgb(120, 225, 240),
        Color::Rgb(60, 190, 220),
        Color::Rgb(32, 130, 160),
        Color::Rgb(24, 84, 104),
    ];
    GEAR_WAVE[(frame / 2 % 4) as usize]
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
        + 2 * usize::from(row.think); // ┃ + gutter pad
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
    bubble: bool,
) -> Vec<ChatRow> {
    let mut rows = Vec::new();
    let w = width.max(1);
    for (line, text) in texts.iter().enumerate() {
        let user = is_user.get(line).copied().unwrap_or(false);
        // thinking lines wrap 2 columns narrower via the flagged wrapper (the "┃ "
        // gutter); bubble-bound user lines reserve the frame columns
        let lw = if user && bubble {
            w.saturating_sub(4 + BUBBLE_MARGIN).max(1)
        } else {
            w
        };
        let is_think = think.get(line).copied().unwrap_or(false);
        let mut part = wrap_chat_rows_flagged(&[text.clone()], lw, &[is_think]);
        for r in &mut part {
            // wrap_chat_rows indexes within the single-line slice — restore the
            // real source-line index so grouping/mapping stay correct
            r.line = line;
            r.user = user;
        }
        rows.extend(part);
    }
    rows
}

/// Word wrap with char-index tracking; long words (URLs, CJK runs) hard-split at width.
fn wrap_chat_rows(texts: &[String], width: usize) -> Vec<ChatRow> {
    wrap_chat_rows_flagged(texts, width, &[])
}

/// Wrap with per-line flags: a thinking line reserves its 2-column "┃ " gutter,
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
    /// input-box mouse selection as (anchor, end) display char indices; kept
    /// highlighted after release until the next key or click
    input_sel: Option<(usize, usize)>,
    input_dragging: bool,
    /// previous left press in the input box (time, row, column) — double-click detection
    input_last_click: Option<(std::time::Instant, u16, u16)>,
    /// the user prompt currently being worked on — pinned in the status bar so
    /// switching between many agents never loses track of "which ask is this"
    current_prompt: Option<String>,
    /// prompts entered while a turn runs: sent locally in order when it finishes
    local_queue: Vec<QueuedPrompt>,
    /// /queue dialog open (clickable per-item actions)
    queue_dialog: bool,
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
    /// /todo dialog open
    todo_dialog: bool,
    /// clickable button rects of the /todo dialog, rebuilt every draw
    todo_buttons: Vec<(Rect, TodoAction)>,
    /// when the running turn started — drives the elapsed readout and tip rotation
    turn_started: Option<std::time::Instant>,
    /// chars received this turn (text + thinking deltas) — the ↓ readout
    turn_chars: u64,
    /// full serve session id (the status bar shows the 8-char prefix; the exit
    /// note needs the whole thing for `tango --resume`)
    session_full: String,
    /// the serve cwd's current git branch (hello frame) — Ctrl+B filters by it
    serve_branch: String,
    /// the current session's display name (/name) — status bar shows it over the id
    session_name: String,
    /// the status-bar sess span's rect — clicking copies the full id
    session_id_area: Rect,
    /// user-message style: true = right-positioned bubble (default),
    /// false = Claude Code style (same theme, left, full-width bg) — /bubble
    user_bubble: bool,
    /// scroll offset of the permission dialog's command preview (↑↓/wheel)
    perm_scroll: usize,
    /// the preview's wrap width, captured each draw so key/wheel scrolling
    /// computes the same row count the dialog rendered
    perm_wrap_w: usize,
    /// alt+e armed: the destructive confirm (interrupt + code rollback) waits
    /// for a second press
    editlast_armed: bool,
    /// the prompt menu (click a user bubble): anchored line index
    prompt_menu: Option<usize>,
    /// clickable rows of the prompt menu, rebuilt every draw
    prompt_menu_buttons: Vec<(Rect, u8)>,
    /// the first line of the turn targeted by 编辑重发 (timeline matching)
    edit_target_head: Option<String>,
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
            input_sel: None,
            input_dragging: false,
            input_last_click: None,
            current_prompt: None,
            local_queue: Vec::new(),
            queue_dialog: false,
            choices: None,
            choices_collapsed: false,
            choice_rows: Vec::new(),
            todos: Vec::new(),
            todo_dialog: false,
            todo_buttons: Vec::new(),
            turn_started: None,
            turn_chars: 0,
            session_full: String::new(),
            serve_branch: String::new(),
            session_name: String::new(),
            session_id_area: Rect::default(),
            user_bubble: true,
            perm_scroll: 0,
            perm_wrap_w: 76,
            editlast_armed: false,
            prompt_menu: None,
            prompt_menu_buttons: Vec::new(),
            edit_target_head: None,
            ime_restore: None,
            queue_buttons: Vec::new(),
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
            if row.border {
                continue; // frame rows are decoration, never copied
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

    /// A click (press + release, no drag) in the chat area: an image chip under the
    /// cursor opens the image; a file path under the cursor opens the read-only
    /// viewer; a line with collapsed output opens the transcript viewer.
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
            if source
                .and_then(|i| self.lines.get(i))
                .is_some_and(|l| l.tone == Tone::User && l.output.is_none())
            {
                self.prompt_menu = source;
                return;
            }
        }
        let vrow = row.saturating_sub(chat_y) as usize;
        if let Some(index) = self.line_at_visual_row(
            self.chat_area.width as usize,
            self.chat_area.height as usize,
            vrow,
        ) {
            self.open_line_viewer_quiet(index);
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
                Ok(()) => self.push(format!("已复制原文：{head}"), Tone::Success),
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
        let width = self.input_area.width as usize;
        let layout = self.input_layout(self.input_area.width);
        let visible = self.input_area.height.saturating_sub(1).max(1) as usize;
        let scroll = layout.cursor_row.saturating_sub(visible - 1);
        let text_top = self.input_area.y + 1;
        if row < text_top {
            return None;
        }
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

    /// Release: a press+release without movement is a click (chips/paths/viewers);
    /// anything dragged over is copied exactly to the system clipboard.
    fn chat_release(&mut self, chat_y: u16, row: u16, column: u16) {
        let clicked = self.sel_anchor_pos == Some((row, column));
        let text = self.selected_text(chat_y);
        self.sel_anchor_pos = None;
        self.sel_end_pos = None;
        if clicked {
            self.chat_click(row, column, chat_y);
        } else if let Some(text) = text
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

    /// The display char index under a screen position in the input box, using the
    /// draw pass's wrap and scroll. Rows above/below clamp to the first/last line;
    /// columns past a line's end land after its last char.
    fn input_index_at(&self, row: u16, column: u16) -> usize {
        let layout = self.input_layout(self.input_area.width);
        let total = self.rendered_input().text.chars().count();
        let visible = self.input_area.height.saturating_sub(1).max(1) as usize;
        let scroll = layout.cursor_row.saturating_sub(visible - 1);
        let text_top = self.input_area.y + 1;
        if row < text_top {
            return 0;
        }
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

    fn input_copy(&mut self, text: &str) {
        match self.copy_to_clipboard(text) {
            Ok(()) => {
                let n = text.chars().filter(|c| *c != '\n').count();
                self.push(format!("已复制 {n} 个字符到剪贴板"), Tone::Success);
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
        let d: String = detail.chars().take(4000).collect();
        d
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
                "模型在等你选择".to_string()
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
            "/todo" => {
                self.todo_dialog = !self.todo_dialog;
                if !self.todo_dialog {
                    self.todo_buttons.clear();
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
                } else if arg == "all" {
                    self.queue_supplement_all(serve);
                } else if arg.is_empty() {
                    self.queue_dialog = !self.queue_dialog;
                    if !self.queue_dialog {
                        self.queue_buttons.clear();
                    }
                } else {
                    self.push(
                        "usage: /queue · /queue rm <n> · /queue all（全部并入当前） · /queue clear",
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
            format!("🖼 已附加图片 #{id} · 点击查看 · 随下一条消息发送"),
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
            format!("🖼 已附加图片 #{id} · 点击查看 · 随下一条消息发送"),
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
                        match action {
                            QueueAction::Supplement(i) => self.queue_supplement_one(serve, i),
                            QueueAction::Remove(i) => {
                                if let Some(q) = self.local_queue.get(i).cloned() {
                                    self.local_queue.remove(i);
                                    let head: String = q
                                        .text
                                        .lines()
                                        .next()
                                        .unwrap_or("")
                                        .chars()
                                        .take(40)
                                        .collect();
                                    self.push(
                                        format!("已移除排队 #{}：{head}", i + 1),
                                        Tone::Muted,
                                    );
                                }
                            }
                            QueueAction::SupplementAll => self.queue_supplement_all(serve),
                            QueueAction::ClearAll => {
                                let n = self.local_queue.len();
                                self.local_queue.clear();
                                self.push(format!("已清空 {n} 条排队提示"), Tone::Muted);
                            }
                        }
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
                        match action {
                            TodoAction::Expand(i) => self.expand_choice(i),
                            TodoAction::Run(i) => self.run_suggestion(serve, i),
                            TodoAction::Reply(i) => self.reply_question(i),
                            TodoAction::Drop(i) => {
                                if i < self.todos.len() {
                                    self.todos.remove(i);
                                }
                            }
                        }
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
                        // one ordered click handler: input-box image chip → jump badge →
                        // chat press (selection anchor; chips/paths/viewers wait for
                        // release so a drag from anywhere can select text)
                        MouseEventKind::Down(MouseButton::Left) => {
                            let pos = Position::new(m.column, m.row);
                            // clicking the status-bar sess span copies the full id
                            if self.session_id_area.width > 0 && self.session_id_area.contains(pos)
                            {
                                let id = self.session_full.clone();
                                match self.copy_to_clipboard(&id) {
                                    Ok(()) => {
                                        self.push(format!("已复制会话 id：{id}"), Tone::Success)
                                    }
                                    Err(e) => self.push(format!("复制失败：{e}"), Tone::Error),
                                }
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
                }
                self.frame(v, serve)
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
                PickerKind::HandoffTarget | PickerKind::TakeoverSource => {
                    self.send_transfer(kind, &item.id, "", serve);
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
        if self.queue_dialog && (k.code == KeyCode::Esc || k.code == KeyCode::Char('q')) {
            self.queue_dialog = false;
            self.queue_buttons.clear();
            return;
        }
        if self.todo_dialog && k.code == KeyCode::Esc {
            self.todo_dialog = false;
            self.todo_buttons.clear();
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
                KeyCode::Esc if !self.busy => {
                    self.choices_collapsed = true;
                    return;
                }
                // tab = amend: the options are suggestions, the user's own words win
                KeyCode::Tab => {
                    let question = self
                        .choices
                        .as_ref()
                        .map(|c| c.question.clone())
                        .unwrap_or_default();
                    self.input = question.chars().collect();
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
        let interrupt_key =
            k.code == KeyCode::Esc || (self.keys.matches(Action::Interrupt, k) && self.input.is_empty());
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
            self.push("[interrupt requested]", Tone::Muted);
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
            self.editlast_armed = false; // esc backs out of the armed confirm too
            if self.history_nav.is_some() {
                // esc while browsing history: back to the draft, not wiping it
                self.exit_history_nav();
                return;
            }
            self.clear_input();
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
            KeyCode::Enter if self.rendered_input().text.trim_start().starts_with('/') => {
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

    fn send_prompt(&mut self, serve: &mut Serve) {
        let (text, images) = self.take_submission();
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
                self.session_full = v["sessionId"].as_str().unwrap_or("").to_string();
                self.serve_branch = v["branch"].as_str().unwrap_or("").to_string();
                self.session_name = v["sessionName"].as_str().unwrap_or("").to_string();
                self.user_bubble = v["bubble"].as_bool().unwrap_or(true);
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
                    Some("text") => {
                        self.stream.push_str(d);
                        self.turn_chars += d.chars().count() as u64;
                    }
                    Some("thinking") => {
                        self.thinking.push_str(d);
                        self.turn_chars += d.chars().count() as u64;
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
            // chat-line notices from serve (warnings, goal/subtask status) — never part of a stream
            "notice" => {
                if let Some(text) = v["text"].as_str() {
                    self.push(text, Tone::Muted);
                    self.jump_to_bottom();
                }
            }
            "tool_result" => {
                let id = v["call"]["id"].as_str().unwrap_or_default().to_string();
                if !id.is_empty() {
                    self.running_tools.remove(&id);
                }
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
                let text = format!("⚙ {name} {summary} · {ms}ms{note}");
                // freeze the RUNNING line into its summary in place: one line per call,
                // and the animation settles exactly where it played. A result without
                // a seen start (resumed session) still appends a line.
                let hit = self
                    .lines
                    .iter()
                    .rposition(|l| !id.is_empty() && l.tool_id.as_deref() == Some(id.as_str()));
                if let Some(i) = hit {
                    let line = &mut self.lines[i];
                    line.text = text;
                    line.tone = style;
                    line.output = full;
                    line.tool_id = None;
                } else {
                    self.push_tool(text, style, full);
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
                    }
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
                                    h[k].as_str().unwrap_or("?").chars().take(8).collect()
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
                                d["from"].as_str().unwrap_or("?").chars().take(8).collect();
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
                    if let Some(line) = self.lines.last_mut() {
                        line.prompt_state = Some(PromptState::Completed);
                    }
                }
                Some("assistant") => {
                    self.stream = text.to_string();
                    self.commit_stream();
                }
                Some("handoff") => {
                    let short: String =
                        it["from"].as_str().unwrap_or("?").chars().take(8).collect();
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
        // +2: title + hint rows; capped so the chat keeps at least half the screen
        (content + 2).min(term_h / 2).max(3)
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
                    tool_id: line.tool_id.clone(),
                    source_index,
                }
            })
            .collect()
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
        // the choices panel takes a real layout row — the chat scrolls up to make
        // room instead of being covered (the text under it stays reachable)
        let panel_h = self.choices_panel_height(area.width, area.height);
        // jump-to-bottom pill: its own row above the input, never over the chat or the
        // status bar; the chat shrinks by one so nothing gets covered
        let jump_h = u16::from(self.scroll_back > 0);
        let chat_h = self
            .chat_height(area, input_h)
            .saturating_sub(panel_h as usize + jump_h as usize);
        let [chat_a, jump_a, panel_a, input_a, status_a] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(jump_h),
            Constraint::Length(panel_h),
            Constraint::Length(input_h),
            Constraint::Length(1),
        ])
        .areas(area);

        self.chat_area = chat_a;
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
        let mut texts: Vec<String> = self.chat_lines().iter().map(|l| l.text.clone()).collect();
        if !self.stream.is_empty() {
            // live assistant text (not committed yet) hangs off the last line, markdown stripped
            texts.push(self.stream.replace("**", "").replace('`', ""));
        }
        let tones: Vec<Tone> = self.chat_lines().iter().map(|l| l.tone).collect();
        let user_flags: Vec<bool> = tones.iter().map(|t| *t == Tone::User).collect();
        let think_flags: Vec<bool> = tones.iter().map(|t| *t == Tone::Thinking).collect();
        let gear_running: Vec<bool> = self
            .chat_lines()
            .iter()
            .map(|l| l.tool_id.as_deref().is_some_and(|id| self.running_tools.contains(id)))
            .collect();
        let mut all_rows =
            wrap_chat_rows_mixed(&texts, inner_w, &user_flags, &think_flags, self.user_bubble);
        layout_user_bubbles(&mut all_rows, &user_flags, inner_w, self.user_bubble);
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
                // "┃ " gutter; the color scrolls down the live block while a turn runs
                let local = live_think_block.map_or(g, |b| g.saturating_sub(b));
                let live = self.busy
                    && supports_truecolor()
                    && live_think_block.is_some_and(|b| g >= b);
                let color = thinking_bar_color(self.think_frame, local, live);
                pad.push(Span::styled("┃".to_string(), Style::default().fg(color)));
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
                spans.push(Span::styled(row.text.clone(), style));
                shown.push(Line::from(spans));
            } else if gear_running.get(row.line).copied().unwrap_or(false)
                && row.text.starts_with("⚙")
            {
                // the tool is still running: the gear itself cycles color
                let rest = row.text.strip_prefix("⚙ ").unwrap_or(&row.text).to_string();
                let mut spans = pad;
                spans.push(Span::styled(
                    "⚙ ".to_string(),
                    Style::default().fg(gear_color(self.think_frame, true)),
                ));
                spans.push(Span::styled(rest, style));
                shown.push(Line::from(spans));
            } else {
                shown.push(Line::from(Span::styled(row.text.clone(), style)));
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
            let h = (n.min(10) as u16) + 4; // rows + rule + footer buttons + hint
            let w = 72u16.min(area.width);
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            let inner = render_dialog(f, pop, "提示队列", "点击按钮操作 · esc 关闭", self.theme);
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
                f.render_widget(Paragraph::new(format!("{}. {head}", i + 1)), text_area);
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

        // the /todo dialog: every unanswered choice and side suggestion, one row each
        if self.todo_dialog {
            let n = self.todos.len();
            let h = (n.min(10) as u16) + 3; // rows + hint + padding
            let w = 76u16.min(area.width);
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            let inner = render_dialog(
                f,
                pop,
                "待定 · To be considered",
                "点击逐条处理 · esc 关闭",
                self.theme,
            );
            self.todo_buttons.clear();
            let btn_style = self.theme.style(Tone::Badge);
            let del_style = self.theme.style(Tone::Warning);
            let iw = inner.width as usize;
            let mut y = inner.y;
            for (i, item) in self.todos.iter().enumerate() {
                if i >= 10 || y >= inner.y + inner.height {
                    break;
                }
                let (badge, head, b1_label, b2_label, act1, act2) = match &item.kind {
                    TodoKind::Choice { question, .. } => (
                        "❓",
                        question.clone(),
                        "[展开]",
                        "[忽略]",
                        TodoAction::Expand(i),
                        TodoAction::Drop(i),
                    ),
                    TodoKind::Suggest { text } => (
                        "💡",
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
                };
                let head: String = head.chars().take(iw.saturating_sub(22)).collect();
                let text_area = Rect {
                    x: inner.x,
                    y,
                    width: inner.width.saturating_sub(20),
                    height: 1,
                };
                f.render_widget(Paragraph::new(format!("{badge} {head}")), text_area);
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
                    Paragraph::new("（空 · 模型的 ?choice/?suggest 会收进这里）"),
                    inner,
                );
            }
        }

        // the prompt menu: a small centered dialog anchored to a user bubble
        if self.prompt_menu.is_some() {
            let w = 44u16.min(area.width);
            let h = 6;
            let pop = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            let inner = render_dialog(f, pop, "这条消息", "数字键或点击 · esc 关闭", self.theme);
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
            let inner = render_dialog(
                f,
                pop,
                "❓ 模型在等你选择",
                "↑↓/点击 选择 · enter 确认（或输编号+enter）· tab 改写自答 · esc 收起",
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
                Paragraph::new(format!("{}. {emarker} ❓ 详细解释各选项", n + 1)).style(estyle),
                erow,
            );
            self.choice_rows.push((erow, n));
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
        let rule_row = Self::rule_line(
            &self.work_text(),
            tip,
            area.width as usize,
            self.theme.style(Tone::Muted),
            self.theme.style(if self.busy {
                Tone::Success
            } else {
                Tone::Muted
            }),
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
        let layout = self.input_layout(area.width);
        let visible = input_a.height.saturating_sub(1).max(1) as usize;
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
        f.render_widget(
            Paragraph::new(input_lines).scroll((input_scroll as u16, 0)),
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
            let h = (line_count.min(14) + pagination + query_rows + 2) as u16;
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
                PickerKind::HandoffTarget => "交接给哪个会话 · 当前会话的摘要会并入它",
                PickerKind::TakeoverSource => "接管哪个会话 · 它的摘要会并入当前会话",
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
                                let note: String = it
                                    .note
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
            let w = (area.width.min(76)).max(40).min(area.width);
            // the full command wraps; the window shows at most 8 rows and scrolls
            let wrap_w = (w as usize).saturating_sub(2);
            self.perm_wrap_w = wrap_w;
            let cmd_rows = wrap_chat_rows(&[p.detail.clone()], wrap_w);
            let max_rows = 8.min(area.height.saturating_sub(6) as usize);
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
            let h = (3 + brief_rows + shown.len() + usize::from(total > max_rows)) as u16;
            let h = h.max(5).min(area.height);
            let popup = Rect {
                x: (area.width.saturating_sub(w)) / 2,
                y: (area.height.saturating_sub(h)) / 2,
                width: w,
                height: h,
            };
            let mut hint = String::from("y 允许 · n 拒绝 · a 始终允许 · c 复制命令");
            if total > max_rows {
                hint = format!(
                    "↑↓/滚轮 滚动（{}/{} 行） · {}",
                    self.perm_scroll + 1,
                    total,
                    hint
                );
            }
            let inner = render_dialog(f, popup, &p.name, &hint, self.theme);
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
            return vec![Span::styled(
                format!(
                    " {} ",
                    clip_to_width(raw.trim(), (width as usize).saturating_sub(2)).trim_end()
                ),
                self.theme.style(Tone::Badge),
            )];
        }
        // (segment, drop order: lower goes first)
        let mut segs: Vec<(Span<'static>, u8)> = vec![
            (
                Span::styled(format!(" {} ", self.model), self.theme.style(Tone::Badge)),
                5,
            ),
            (
                Span::styled(format!(" {} ", self.mode), self.theme.style(Tone::Muted)),
                6,
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
                    format!(" ❓{}待 /todo ", self.todos.len()),
                    self.theme.style(Tone::Warning),
                ),
                8,
            ));
        }
        if self.focus {
            segs.push((Span::styled(" focus ", self.theme.style(Tone::Badge)), 4));
        }
        if self.plan_mode {
            segs.push((Span::styled(" PLAN ", self.theme.style(Tone::Warning)), 7));
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
            // shrink before dropping: model name → 12 cols, pinned prompt → 16, ❓ badge → just the count
            let limit = if text.starts_with('▶') {
                16
            } else if text.starts_with('❓') {
                4
            } else if text.starts_with("\\o")
                || text.starts_with("_o")
                || text.starts_with("/o")
                || text == "idle"
                || text.starts_with("♪")
            {
                // the work segment: counter first, then the timer — the dancer stays to the last
                if display_width(&text) > 9 { 9 } else { 3 }
            } else if i == 0 {
                12
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

    /// "\o/ 12s · ↓14.0k" while a turn runs, "idle" otherwise. Lives on the input rule's
    /// left end (and in a custom statusline's {work}); the default status bar no longer carries it.
    fn work_text(&self) -> String {
        if !self.busy {
            return "idle".to_string();
        }
        match self.turn_started {
            Some(t0) => {
                let ms = t0.elapsed().as_millis();
                format!(
                    "{} {} · ↓{}",
                    Self::tango_frame(ms),
                    Self::fmt_elapsed(ms as u64 / 1000),
                    Self::fmt_tokens(self.turn_chars)
                )
            }
            None => "♪ working".to_string(),
        }
    }

    /// The input rule: work state on the left, a rotating tip on the right, dashes between.
    /// Too narrow for both → the tip goes; narrower still → the work text sheds the counter,
    /// then the timer, keeping the dancer.
    fn rule_line(
        work: &str,
        tip: Option<&str>,
        width: usize,
        dash: Style,
        live: Style,
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
        let left = format!("─ {work} ");
        let right = tip.map(|t| format!(" {t} ─")).unwrap_or_default();
        let fill = width.saturating_sub(display_width(&left) + display_width(&right));
        Line::from(vec![
            Span::styled(left, live),
            Span::styled("─".repeat(fill), dash),
            Span::styled(right, dash),
        ])
    }

    /// The working dot, dancing: a couple swaying and dipping the tango, one frame
    /// once per heartbeat tick off the turn clock, so every step is actually drawn.
    /// Every frame is 3 columns wide so the rest of the status bar never shifts.
    fn tango_frame(ms: u128) -> &'static str {
        const FRAMES: [&str; 8] = ["\\o/", "_o/", "_o_", "\\o_", "\\o/", "/o_", "_o_", "/o\\"];
        FRAMES[(ms / 1000) as usize % FRAMES.len()]
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
                self.session_full, self.session_full
            ))
        }
    }

    /// plain text of the status bar (tests and /statusline preview)
    fn status_segments_text(&mut self) -> String {
        self.status_spans(200)
            .iter()
            .map(|s| s.content.to_string())
            .collect::<Vec<_>>()
            .join("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

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
        let mut rows = wrap_chat_rows_mixed(&texts, 40, &[true], &[], true);
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
        // scrolling up shows the pill row, shrinking the chat by one row
        assert!(
            row(&terminal, 0).starts_with("line-07"),
            "unexpected first row: {:?}",
            row(&terminal, 0)
        );

        app.push("line-20", Tone::Text);
        app.push("line-21", Tone::Text);
        app.push("line-22", Tone::Text);
        terminal.draw(|f| app.draw(f)).unwrap();
        assert!(row(&terminal, 0).starts_with("line-07"));
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
        let rows = wrap_chat_rows_mixed(&texts, 40, &flags, &[], true);
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
        let mut rows = wrap_chat_rows_mixed(&texts, 24, &flags, &[], true);
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
        let y = app.input_area.y + 1; // first text row under the input rule
        mouse(
            &mut app,
            &mut serve,
            MouseEventKind::Down(MouseButton::Left),
            6,
            y,
        );
        mouse(
            &mut app,
            &mut serve,
            MouseEventKind::Drag(MouseButton::Left),
            11,
            y,
        );
        mouse(
            &mut app,
            &mut serve,
            MouseEventKind::Up(MouseButton::Left),
            11,
            y,
        );
        assert_eq!(app.input_sel, Some((6, 11)));
        assert_eq!(app.input_raw_text(6, 11), "brave");
        assert!(
            app.lines
                .iter()
                .any(|l| l.text == "已复制 5 个字符到剪贴板")
        );
        // the selection stays highlighted until the next key
        let b = {
            use ratatui::{Terminal, backend::TestBackend};
            let mut t = Terminal::new(TestBackend::new(40, 12)).unwrap();
            t.draw(|f| app.draw(f)).unwrap();
            t.backend().buffer().clone()
        };
        use ratatui::style::Modifier;
        let rev = |x: u16| b[(x, y)].modifier.contains(Modifier::REVERSED);
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
        let y = app.input_area.y + 1;
        let click = |app: &mut App, serve: &mut Serve, x: u16, y: u16| {
            app.input_last_click = None; // separate clicks, never a double-click
            mouse(app, serve, MouseEventKind::Down(MouseButton::Left), x, y);
            mouse(app, serve, MouseEventKind::Up(MouseButton::Left), x, y);
        };
        click(&mut app, &mut serve, 3, y); // right half of 一 (columns 2-3)
        assert_eq!(app.cursor, 1);
        click(&mut app, &mut serve, 5, y + 1); // the 中 after "abc "
        assert_eq!(app.cursor, 8);
        click(&mut app, &mut serve, 30, y); // past the end of row 1
        assert_eq!(app.cursor, 3, "before the newline");
        assert_eq!(app.input_sel, None);
        assert!(
            !app.lines.iter().any(|l| l.text.contains("已复制")),
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
        let y = app.input_area.y + 1;
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
            app.lines
                .iter()
                .any(|l| l.text == format!("已复制 {n} 个字符到剪贴板")),
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
            "✻ 思考内容比较长需要折行思考内容比较长需要折行".to_string(),
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
        // click mapping skips the "┃ " gutter
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
        }];
        assert_eq!(app.chat_line_hit(0, 1, 0).map(|(_, _, i)| i), Some(0), "gutter clamps to 0");
        assert_eq!(app.chat_line_hit(0, 2, 0).map(|(_, _, i)| i), Some(0), "text starts at col 2");
        assert_eq!(app.chat_line_hit(0, 4, 0).map(|(_, _, i)| i), Some(1), "汉 spans 2-3");
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
            // frame is frozen at 0 under test: truecolor gives GEAR_WAVE[0], a
            // 256-color terminal gives the blink's first Cyan — both are "lit"
            let fg = b[(0, 0)].fg;
            assert!(
                matches!(fg, Color::Cyan | Color::Rgb(120, 225, 240)),
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
        assert_eq!(wave[0], THINK_WAVE[0]);
        assert!(wave.iter().any(|c| matches!(c, Color::Rgb(120, 225, 240))));
        assert_eq!(thinking_bar_color(8, 0, true), thinking_bar_color(0, 0, true));
    }

    #[test]
    fn thinking_rows_render_with_the_bar_and_the_text_indented() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.push("✻ 这是思考".to_string(), Tone::Thinking);
        app.busy = true;
        let mut t = Terminal::new(TestBackend::new(30, 10)).unwrap();
        t.draw(|f| app.draw(f)).unwrap();
        let b = t.backend().buffer();
        let row: String = (0..30).map(|x| b[(x, 0)].symbol().to_string()).collect();
        // wide chars leave blank continuation cells — compare with spaces removed
        let tight = row.replace(' ', "");
        assert!(tight.starts_with("┃✻这是思考"), "{row:?}");
        assert_eq!(b[(1, 0)].symbol(), " ", "one space gutter pad");
        assert!(app.chat_rows.iter().all(|r| r.think));
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
            app.lines
                .iter()
                .any(|l| l.text.contains("已复制会话 id") && l.text.contains("a1b2c3d4e5f67890")),
            "click confirms the copy"
        );
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
        let sent2: Value = serde_json::from_str(&serve.sent[1]).unwrap();
        assert_eq!(sent2["text"], "第三问");
        {
            use ratatui::{Terminal, backend::TestBackend};
            let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
            terminal.draw(|f| app.draw(f)).unwrap();
        }

        // queue more, click [清空]
        app.local_queue = vec![QueuedPrompt::text_only("x")];
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

        // Esc closes
        app.key(
            crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut serve,
        );
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
        let spans = app.status_spans(200);
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
            &mut serve,
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
    fn subtask_command_sends() {
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.run_slash("/subtask 修个测试", &mut serve);
        let sent: Value = serde_json::from_str(&serve.sent[0]).unwrap();
        assert_eq!(sent["cmd"], "subtask");
        assert_eq!(sent["prompt"], "修个测试");
        app.frame(
            json!({"type": "result", "id": sent["id"], "ok": true,
                "data": {"task": {"id": "s1", "status": "running", "worktreeName": "bg-1"}}}),
            &mut serve,
        );
        assert!(app.lines.iter().any(|l| l.text.contains("s1")));
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
    fn clicking_a_user_bubble_opens_the_prompt_menu() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = App::new();
        let mut serve = Serve::mock();
        app.push_user("帮我看看这个");
        app.chat_inner_w = 80;
        app.chat_scroll_offset = 0;
        app.chat_rows = wrap_chat_rows(&["❯ 帮我看看这个".to_string()], 80);
        {
            let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
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
        eprintln!(
            "dbg: x={x} prompt_menu={:?} line0 tone={:?} output={:?} lavr={:?}",
            app.prompt_menu,
            app.lines.first().map(|l| l.tone),
            app.lines.first().and_then(|l| l.output.as_deref()),
            app.line_at_visual_row(80, 17, 0)
        );
        assert_eq!(app.prompt_menu, Some(0), "menu opens anchored to the line");
        {
            let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
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
            app.lines
                .iter()
                .any(|l| l.text.contains("已复制") && l.text.contains("第一行")),
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
        assert!(app.lines.iter().any(|l| l.text.contains("命令已复制")));
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
        assert!(app.lines.iter().any(|l| l.text.contains("已复制 5 个字符")));
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
        // release on the path → viewer opens, selection cleared
        app.chat_release(0, 1, 30);
        let v = app.viewer.as_ref().expect("viewer opened on release");
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
            app.lines.iter().any(|l| l.text.contains("已复制")),
            "dragged text was copied"
        );
        let _ = std::fs::remove_dir_all(&dir);
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
            typed, "要改吗？",
            "tab preloads the question for a custom answer"
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
        // click (press+release) on the directory path itself
        let on_path = 9 + dir.display().to_string().chars().count() / 2;
        app.chat_press(0, on_path as u16);
        app.chat_release(0, 0, on_path as u16);
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
        app.chat_press(0, off);
        app.chat_release(0, 0, off);
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
            .push("已将慢查询加了索引，延迟从 2s 降到 50ms。\n细节略".into());
        app.frame(
            json!({"type": "result", "id": app.prompt_id, "ok": true, "data": {}}),
            &mut serve,
        );
        assert!(!app.busy);
        assert!(
            app.lines
                .iter()
                .any(|l| l.text.contains("✓ 本轮：已将慢查询加了索引")),
            "auto recap pushed"
        );
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

        let l = App::rule_line("idle", None, 40, plain, plain);
        assert_eq!(display_width(&text(&l)), 40);
        assert!(text(&l).starts_with("─ idle ─"), "{}", text(&l));

        let l = App::rule_line("\\o/ 12s · ↓14.0k", Some("ctrl+o 看工具"), 60, plain, plain);
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
        ));
        assert!(t.contains("12s") && !t.contains("看工具"), "{t}");
        let t = text(&App::rule_line("\\o/ 12s · ↓14.0k", None, 12, plain, plain));
        assert!(t.contains("\\o/") && !t.contains("↓"), "{t}");
        let t = text(&App::rule_line("\\o/ 12s · ↓14.0k", None, 8, plain, plain));
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
                !s.contains("idle") && !s.contains('↓'),
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
    fn working_dot_dances_the_tango_in_fixed_width_frames() {
        let frames: Vec<&str> = (0..8).map(|i| App::tango_frame(i * 1000)).collect();
        assert!(frames.iter().all(|f| display_width(f) == 3), "{frames:?}");
        assert!(frames.windows(2).any(|w| w[0] != w[1]), "it moves");
        assert_eq!(App::tango_frame(0), App::tango_frame(8 * 1000), "loops");
        assert_eq!(
            App::tango_frame(100),
            App::tango_frame(999),
            "holds within a frame"
        );

        let mut app = App::new();
        app.busy = true;
        app.turn_started = Some(std::time::Instant::now());
        app.turn_chars = 14_000;
        let s = app.work_text();
        assert!(
            s.contains("↓14.0k") && !s.contains('●'),
            "dancer replaces the dot: {s}"
        );
        app.busy = false;
        assert_eq!(app.work_text(), "idle");
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
        // chips open on release (press+release, no drag), like file paths
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
