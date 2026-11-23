# 各家 agent 如何显示 bash 工具调用（2026-11-23 调研）

资料来源：Claude Code 2.1.270 本机二进制里的渲染函数；Codex `codex-rs/tui/src/exec_cell/render.rs`（GitHub main）；
ZCode 本机 `/Applications/ZCode.app` 的 app.asar（renderer 源码 + 文案表）及 zcode.z.ai 文档；
grok-cli 社区版（superagent-ai，`fb97af8`）`src/ui/app.tsx`。xAI 官方没有公开的 Grok CLI 源码。

## Claude Code

- 标题行 `Bash(命令)`。命令超过 2 行或 160 字符时，只显示前 2 行、截到 160 字符，末尾加 `…`。
- `sed -i` 这类就地编辑命令，标题直接显示被改的文件路径。
- 工具有可选参数 `description`（模型写的一句话意图），用在运行中状态 "Running {description}" 和摘要里。
- 输出只显示前 3 行，下面一行灰字 `… +N lines (ctrl+o to expand)`。
- 查看完整内容：Ctrl+O 打开 transcript 视图，命令和输出全文显示。全屏渲染模式下单击折叠的工具结果，
  调用和结果一起就地展开，再点收起。

## Codex

- 标题 `• Ran 命令`（运行中是 `Running`），圆点绿色成功、红色失败。去掉 `bash -lc` 外壳，做语法高亮。
- 命令第一行接在标题后，后续行用 `  │ ` 前缀，最多再显示 2 行，超出给 `… +N lines`。
- 输出用 `  └ ` 前缀，最多 5 行，取头尾、中间省略：`… +N lines (ctrl+t to view transcript)`。
  用户自己 `!` 跑的命令放宽到 50 行。
- 读文件、搜索、列目录类命令被解析成语义动作，连续几条合并成一块 `Explored`，
  下面是 `Read a.rs, b.rs`、`Search foo in src` 这样的行，不显示原命令。
- 查看完整内容：Ctrl+T 打开 transcript，命令全文、输出全文。

## ZCode（智谱，桌面 Electron 应用）

- 默认是折叠卡片，一行：`已执行` + 次要文字。次要文字优先用 Bash 的 `description`，没有才用命令本身，
  单行，超宽用 CSS 截断。
- 单击卡片展开详情：`$ 命令` 区最多 3 行，溢出时出现"展开命令"按钮，展开后最多 10 行、可滚动；
  输出框最多 5 行、可滚动，运行中自动跟随到底部。
- 连续多条命令合并成一组："N 条命令 · N 个失败"。
- "办公模式"下只显示"已运行命令"，不显示命令也不能展开；"编程模式"（默认）才显示命令和输出。

## grok-cli（社区版）

- 标签就是命令本身，`trunc(command, 80)`：按字符截到 80 加 `…`，不处理换行。
- 对话里不显示 bash 输出，也没有展开机制。后台命令显示 `Background: 命令`（截到 70）。

## tango 现状

- `⚙ bash <命令全文，最多 4000 字符> · 5ms`，多行命令原样多行显示（`call_summary`，`tui/src/app.rs`）。
- 输出存在 `line.output`，双击或 Ctrl+O 打开查看器，但查看器只有输出，没有命令。Ctrl+T transcript 用 `line.text`，
  所以命令全文目前只靠工具行本身保存。
- 权限对话框已有 `briefCommand`（`src/core/bash-brief.ts`），会把 heredoc 概括成"向 X 写入 N 行文本"。
