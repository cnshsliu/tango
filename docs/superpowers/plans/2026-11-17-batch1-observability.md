# 批次 1 · 可观测命令 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 补齐可观测类命令：/context /queue /timestamps /density /usage /stats /insights /statusline /autocompact /presets。

**Architecture:** 涉及数据的命令在 serve 计算（日志即真理：从 JSONL 聚合），TUI 纯渲染；纯展示开关（timestamps/density）留 TUI 本地。token 估算沿用 `chars/3.5` 与 Agent.estimateTokens 同一口径。

**Tech Stack:** 同批次 0（TS serve + Rust TUI + node:test + cargo test）。

**Spec:** `docs/superpowers/specs/2026-11-16-time-travel-and-parity-design.md` §四 批次 1。

## Global Constraints

- 不破坏 TS 85 / Rust 75 绿色基线；每命令 README + /help 同步。
- 密集数值用与 `Agent.estimateTokens` 一致的 /3.5 估算，不引入新 tokenizer。
- 配置新增键（`statusline`、`autocompact`）走现有 `saveConfig({...loadConfig(), ...})` 模式，缺省行为不变。
- UI 逻辑零业务判断（DeepSeek 原则 #2）。

## Review Focus

1. 空会话/空预设/空队列时命令不崩，给出友好空态。
2. /usage 扫描大量会话文件的性能与容错（坏行跳过，沿用 SessionStore.list 的容错风格）。
3. /statusline 模板占位符未知时原样显示不 panic；超长模板截断。
4. /autocompact 0/off 语义：0 视为 off。
5. /density 三级对恢复的历史同样生效（chat_lines() 单一过滤点）。

---

### Task 1: /context — 上下文构成
serve 命令 `context` → `{ system, skills, tools, history, total, window, pct }`（system=buildContext(cwd) 长度；skills 从 system 中单列——buildContext 拆分或正则截取，实现取简：分开调 buildContext 与 skills 计数，skills 部分允许并入 system 标注 "含 skills"）；tools=所有工具 schema JSON 长度；history=estimateTokens 口径。TUI `/context` 打开只读 viewer：每行 `名字 条数 tok (###条 ASCII 毱条)` + 窗口百分比与压缩阈值。测试：serve 返回字段与和>0；TUI viewer 打开含各段名。

### Task 2: /queue — 排队提示
serve：`queue` 命令返回 `{ running: {text}|null, queued: [{text, waitedMs}] }`（在 rl 分发处记录：prompt 到达即 push 到 pendingTexts，handle 开始时转移 runningText，result 后清空；interrupt 同时清队列对应项）。TUI `/queue`：viewer 或聊天行显示。测试：慢 mock 下连发两个 prompt，queue 显示 1 排队；interrupt 后队列清空。

### Task 3: /usage /stats /insights — 聚合统计
serve：`usage` → 扫 `sessionsDir()` 全部会话（复用 costUsd + 消息 usage），返回 `{ totalCostUsd, byModel: [{model, requests, input, output, costUsd}], bySession: [{id, name, costUsd, messages}], since }`；`stats` 同义。`insights` → 单文件 HTML（内联 CSS、无外链）写 `<cwd>/tango-insights-<date>.html`：按日成本条形（纯 div 宽度）、模型表、会话表。TUI：/usage /stats 打开 viewer 表格文本；/insights 聊天行报路径。测试：两个会话不同模型后 usage 聚合正确；insights 文件存在含两个模型名且无外链。

### Task 4: /timestamps 与 /density（TUI 本地）
ChatLine 加 `ts: Option<String>`（push 时填充）；`timestamps: bool`（/timestamps 切换），开启时行首 `[HH:MM]`。`density: 0|1|2`（/density 循环或带参），作用于 chat_lines() 的工具行渲染：0=仅名称+摘要（现 focus 态近似）、1=默认（摘要+错误首行）、2=完整含输出前 N 行；restore 历史同路径生效。两状态显示在 /status 输出与状态栏 tooltip。测试：push 后开 timestamps 行首含 `[`；density=2 时错误工具行含输出文本、=0 时不含；默认级不受影响。

### Task 5: /autocompact — 自动压缩阈值
config 加 `autocompact?: number | 'off'`（小数或 0-100 整数百分比，缺省 0.75）。serve：hello 携带；`set_autocompact {value}` 校验并 `saveConfig`，生效于 `agent.compactAt`（0=禁用）。TUI：/autocompact 显示当前阈值；带参（off|N%|0.N）设置并确认。测试：serve set_autocompact 0.5 持久化且 agent 下次超限即压（mock 验证 compaction 条目）；off → compactAt 0 不压；非法值报错。

### Task 6: /statusline — 状态栏模板
config 加 `statusline?: string`。占位符：{model} {mode} {cost} {ctx} {pct} {session} {tokens}。serve `set_statusline {template|null}` 持久化，hello 携带。TUI：渲染时模板存在则替换占位符（未知占位符原样保留），缺失项用现状兜底；`/statusline` 无参显示当前+可用占位符，带参设置，`off` 恢复默认。测试：模板渲染替换全部已知占位符、未知原样；off 恢复。

### Task 7: /presets — 提示模板
`~/.tango/presets.json`（`{name: text}`）。serve：`preset_save {name, text}`、`preset_list` → `[{name, preview}]`、`preset_get {name}`。TUI：`/preset save <name>` 把当前输入存为模板并清空确认；裸 `/preset` 打开可过滤 picker，选中把模板文本填入输入（不发送，paste 逻辑复用 restore 路径）；`/preset <name>` 直接填入。测试：serve 存取往返；TUI picker 选中后 input 含模板文本且未发送；重名覆盖。

### Task 8: README + 全量验收
README 命令清单与各命令一节说明；`npm run typecheck && npm test`、`cargo test`、fmt、release build 全绿；tmux 真实走一遍 /context /usage /insights /preset 存取 /density 切换 /timestamps /statusline /autocompact。
