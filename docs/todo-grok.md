# 向 grok-cli / OpenTUI 学习的 TODO

来源：`~/dev/github/src/grok-cli`（superagent-ai/grok-cli，提交 `fb97af8`）和
`~/dev/github/src/opentui`。这次对照的是源码和 CHANGELOG，不只是 README。
做完一项就勾上，并写上提交号。
每一项都要在同一个提交里更新文档：README（参考手册）和 docs/handbook.md（用户指南）。
`node scripts/docs-check.mjs` 必须没有缺口，docs-guard hook 会在回合结束时检查。

## 已完成

- [x] Hooks：PreToolUse / PostToolUse / UserPromptSubmit / Stop，matcher、超时、exit 2 拦截（`bec61fd`、`374a4eb`、`d422dd4`）
- [x] 项目级配置三层合并加信任确认（grok 的 `.grok/settings.json` 和 `workspace-trust.json`）（`3041200` 到 `9d42abb`）

## P1：价值高，先做

- [ ] **1. 模型自己派子 agent（`task` 工具）和自定义命名子 agent**
  - grok：模型可以调用 `task`，选 `explore`（只读、快速调查）或 `general`（多步执行），也能选配置里 `subAgents: [{name, model, instruction}]` 定义的子 agent。系统提示鼓励它主动派活。见 `src/grok/tools.ts:366`、`src/utils/subagents-settings.test.ts`、`src/ui/agents-modal.tsx`。
  - tango：`/subtask`、`/bg` 只能由用户发起，模型没有派活的工具。`/agents` 只能按角色选模型，不能带指令。
- [x] **2. 后台 shell 进程**
  - grok：`bash` 有 `background: true`，立即返回进程 ID。另有 `process_logs`（看最后 N 行）、`process_stop`（先 SIGTERM，3 秒后 SIGKILL）、`process_list`，最多 8 个。见 `src/tools/bash.ts:177`、`src/grok/tools.ts:142`。
  - tango：`bash` 只能前台跑，最长 120 秒。启动开发服务器、跑 watch 时会卡住整个回合。
- [x] **3. 压缩（compact）升级**
  - grok：保留最近约 2 万 token 原文，只总结更早的部分。总结用固定格式（Goal / Constraints / Progress / Key Decisions / Next Steps / Critical Context）。再次压缩时在旧总结上增量更新。送去总结的工具结果截到 2000 字符。单个回合太大时，把这一回合的前半段单独总结。见 `src/agent/compaction.ts`。
  - tango：`Agent.compact()` 把全部消息压成一段总结，最近的原文也不保留，总结输出上限 1200 token（`src/core/agent.ts:230`）。
- [x] **4. Hooks 第二期：stdout JSON 和更多事件**（已完成：stdout JSON 的 `decision` / `additionalContext` / `continue`，grok 的 17 种事件全部接上。Subagent* 挂在 `/subtask` 上、Task* 挂在 `/bg` 上；#1 的 `task` 工具合并后，也要在它那里发这两组事件）
  - grok：hook 可在 stdout 输出 JSON：`decision: approve|block`、`additionalContext`（追加给模型看的上下文）、`continue: false`。共 17 种事件，我们还缺 `PostToolUseFailure`、`SessionStart`、`SessionEnd`、`StopFailure`、`SubagentStart`、`SubagentStop`、`TaskCreated`、`TaskCompleted`、`PreCompact`、`PostCompact`、`Notification`、`InstructionsLoaded`、`CwdChanged`。见 `src/hooks/types.ts:150`、`src/hooks/executor.ts:165`。
  - tango：只认退出码和 stderr，只有 4 种事件。按 hook-guard 的规则，每加一个事件都要补 e2e 测试。
- [x] **5. 无界面运行的 JSON 事件流和轮数上限**
  - grok：`--format json` 逐行输出 `step_start`、`text`、`tool_use`（含耗时）、`step_finish`（含 token 用量）、`error`。`--max-tool-rounds N` 按次设上限。见 `src/headless/output.ts`。
  - tango：一次性运行只输出给人看的文字。`maxTurns` 只能在 config.json 里改。

## P2：价值中等，改动不大

- [ ] **6. LSP 工具**
  - grok：`lsp` 工具支持跳转定义、查引用、hover、文档符号、工作区符号、实现和调用层级。内置语言服务器清单，缺的会通过 npm 自动安装。见 `src/lsp/builtins.ts`、`src/lsp/manager.ts`、`src/grok/tools.ts:290`。
  - tango：只在写文件后跑 tsc / pyright / cargo 这类检查（`src/core/diagnostics.ts`），模型找定义只能靠 grep。
- [ ] **7. 远程 MCP 服务器和常用清单**
  - grok：MCP 除了 stdio，还支持 HTTP / SSE 加自定义请求头。内置 GitHub、Supabase、Notion、Sentry、Figma 等常用服务器清单，可以在 `/mcps` 里添加和管理。见 `src/mcp/runtime.ts:11`、`src/mcp/catalog.ts`、`src/ui/mcp-modal.tsx`。
  - tango：只支持 stdio（`src/core/mcp.ts`），只能手写 config。
- [x] **8. `AGENTS.override.md`**（按目录替换，不限于 git 根以下，沿用 tango 一直往上找的做法）
  - grok：从 git 根目录往下合并到当前目录，某一层有 `AGENTS.override.md` 时，这一层用它代替 `AGENTS.md`。见 `src/utils/instructions.ts:44`。
  - tango：从当前目录一直合并到文件系统根，只能叠加，不能覆盖（`src/core/context.ts:59`）。
- [ ] **9. `@` 文件提及：模糊搜索和行号范围**
  - grok：按 `git ls-files` 建索引（30 秒刷新，跳过二进制），按文件名打分模糊匹配。支持 `@file:10-20` 只引用几行。见 `src/utils/file-index.ts`、`src/utils/at-mentions.ts`。
  - tango：`@` 只能逐级补全目录（`tui/src/app.rs` 的 `file_candidates`），不支持模糊搜索和行号。
- [ ] **10. 兼容 `.agents/skills` 目录**
  - grok：技能放在 `.agents/skills/<name>/SKILL.md` 和 `~/.agents/skills/`。这是几家工具共用的位置，并解析 YAML frontmatter 里的 `name`、`description`。见 `src/utils/skills.ts`。
  - tango：只读 `~/.tango/skills` 和 `.tango/skills`。
- [ ] **11. 模型可调用的定时任务工具**
  - grok：`schedule_create`、`schedule_list`、`schedule_remove` 等工具，可以用自然语言创建定时任务。TUI 里 `/schedule` 可以浏览。见 `src/tools/schedule.ts`、`src/ui/schedule-modal.tsx`。
  - tango：只有 `tango task add` 命令行，模型和 TUI 都管不到。
- [ ] **12. `/commit-push` 和 `/commit-pr`**
  - grok：一条命令让模型整理改动、提交并推送，或者提交后开 PR。见 `src/ui/app.tsx:2270`。
  - tango：没有。实现上是预置提示词加 slash 命令。
- [ ] **13. 结构化计划和多选问题**
  - grok：`generate_plan` 工具输出标题、步骤和问题，问题分单选、多选、自由文本，在 TUI 里作答。见 `src/grok/tools.ts:925`、`src/ui/plan.tsx`。
  - tango：`/plan` 只是只读模式。`?choice` 只能单选。
- [ ] **14. 每种模式的默认模型**
  - grok：`modeModels` 可以给 agent / plan / ask 模式分别指定模型。见 CHANGELOG 1.1.5-rc4。
  - tango：`roles` 按 driver / subagent / longContext 分，`/plan` 不能自动换模型。

## P3：工作量大，以后再排

- [ ] **15. 飞书双向远程控制（对应 grok 的 Telegram）**
  - grok：在 TUI 里配对 Telegram 机器人，之后手机发消息就能驱动 CLI，工具活动同步显示在 TUI，语音自动转文字，还能用 `telegram_send_file` 回传文件。见 `src/telegram/`。
  - tango：飞书只能单向推送（`notify` 工具和定时任务）。
- [ ] **16. `/verify`：自动找到构建和启动方式并做冒烟测试**
  - grok：识别项目类型，构建、跑测试、启动应用、做浏览器冒烟测试，出带截图的报告。见 `src/verify/`。
  - tango：没有。可以先做不带沙箱的版本：识别脚本、跑测试和构建、出报告。
- [ ] **17. 自我更新**
  - grok：`grok update`、`grok uninstall`，启动时检查新版本。见 `src/utils/install-manager.ts`、`src/utils/update-checker.ts`。
  - tango：本地仓库加 `npm link`，靠手动 `git pull` 和构建。

## TUI：从 OpenTUI 学

- [x] **18. 同步输出（DEC 2026）防闪烁**（已完成：`tui/src/main.rs` 的 `draw_synced`）
  - OpenTUI：每帧包在 `CSI ?2026h … ?2026l` 里，终端整帧一次显示。见 `packages/core/src/renderer.ts:157`。
  - tango：没用。crossterm 自带 `BeginSynchronizedUpdate`，改动很小。
- [x] **19. 主题自动检测：用 OSC 11 查询背景色，监听 997 主题变化通知**（已完成启动时 OSC 11 查询：`tui/src/termbg.rs`。997 实时通知没做：crossterm 0.29 会把 `CSI ?997;1n` 当成未完成序列卡住输入）
  - OpenTUI：向终端查询背景色，系统切换深浅色时能收到通知并跟着变。见 `packages/core/src/renderer-theme-mode.ts`。
  - tango：`auto` 只看 `COLORFGBG` 环境变量，很多终端不设这个变量，运行中也不会跟着变。
- [x] **20. OSC 52 剪贴板**（已完成：`tui/src/clipboard.rs`，`/clipboard`，配置 `clipboard`）
  - OpenTUI：通过终端转义序列复制，SSH 和 tmux 里也能用。见 `packages/core/src/lib/clipboard-service.ts`。
  - tango：`tui/src/clipboard.rs` 调用本机剪贴板命令，远程登录时复制不到本地。
- [x] **21. Kitty 键盘协议**（已完成：启动时 `CSI ? u` 探测，支持就推 disambiguate 级别；Shift+Enter 恒为换行；`TANGO_KITTY_KEYS=off` 关闭）
  - OpenTUI：开启后能区分 Shift+Enter、Ctrl+I 和 Tab 这类组合键。见 `packages/core/src/renderer.ts:908`。
  - tango：没开。Shift+Enter 换行在很多终端里识别不了。
- [ ] **22. Markdown 渲染和代码高亮**
  - OpenTUI：完整的 CommonMark 渲染（列表、代码块、表格、任务勾选框），代码块和 diff 用 tree-sitter 高亮。见 `packages/core/src/renderables/Markdown.ts`、`Code.ts`、`Diff.ts`。
  - tango：只去掉 `**` 和反引号、加粗标题、对齐表格（`tui/src/app.rs:2524`），代码块不高亮。
- [ ] **23. 图片缩略图**
  - OpenTUI：支持 kitty 图形协议，不支持时退回 sixel，再退回字符块。见 `packages/core/src/renderables/Image.ts`。
  - tango：贴图只显示一个可点击的标签。这是 11-18 做贴图时你提过、当时放到以后做的功能。

## 不跟

- 控制桌面（computer 子 agent）：只支持 macOS，风险高。
- Shuru microVM 沙箱：只支持 Apple Silicon，太重。我们有权限闸门和 worktree 隔离。
- 只能用 xAI API 的功能：X 搜索、图片和视频生成、Batch API。
- x402 支付和钱包。
