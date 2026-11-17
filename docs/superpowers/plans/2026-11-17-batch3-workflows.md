# 批次 3 · 工作流模式 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** /plan /goal /loop /effort /review /security-review /memory /remember /skills /hooks（/plugins 移交批次 4，与插件目录统一设计）。

**Architecture:** 模式类（plan/effort/goal）是 serve 会话状态 + 请求参数/系统指令注入；审查类（review/security-review）是"取 diff + 指令化 prompt"的组合命令；记忆类复用全局 AGENTS.md（tango 的长期记忆本来就是它）；hooks 是 config 声明的工具前后 shell 钩子（事件总线 seam）。

**Tech Stack:** 同前。**Spec:** 设计文档 §四 批次 3。

## Global Constraints

- 不破坏 TS 100 / Rust 91 基线；每命令 README+/help 同步。
- /goal 与 /loop 有硬性防失控：goal 最多自动续 N 轮（默认 5，/goal max N 调），loop 必须有间隔下限（≥10s）且 interrupt 即停。
- /plan 模式下写副作用工具一律 veto（理由明示"plan 模式"），readonly 模式优先级仍最高。
- 钩子脚本失败只产生 stderr 警告，绝不阻塞工具调用。

## Review Focus

1. /goal 自动续轮的中断路径：interrupt 必须立刻停止续轮（代数计数复用）。
2. /loop 计时器在 serve 退出/EOF 时必须清理，不留孤儿进程。
3. /plan 模式 + 权限规则冲突：规则不放大写权限越过 plan veto。
4. /remember 注入位置：进全局 AGENTS.md（所有会话可见），单行原子追加。
5. hooks 的 argv 注入：钩子输入经 stdin JSON 传入，不拼进命令行。

---

### Task 1: /plan 计划模式
serve：`set_plan on|off`（config 不持久，会话内）；plan 模式：① gate 对 writeEffect 工具 veto（在规则评估之前——RF#3）② hello/turn_end 携带 plan 标志。TUI：/plan 切换，状态栏 PLAN 徽标；plan 模式下提示词系统注入"先研究并产出计划，等待批准"。测试：plan on 后 write 工具被拒（含规则放行时也拒）、read 工具正常、off 后恢复。

### Task 2: /effort 推理力度
ModelSpec 请求参数：serve `set_effort {low|medium|high|off}` 会话内；openai 适配器 body 加 `reasoning_effort`（存在时）；anthropic 加 `thinking: {budget_tokens}` 映射（low=4k/medium=8k/high=16k）。测试：mock 断言请求体带对应字段；off 不带。

### Task 3: /review 与 /security-review
serve：`review {scope?}` = 取工作区 diff（复用 diffTool.execute）+ 指令模板（正确性/安全各自）作为 prompt 一次执行（写入会话，正常计费）。空 diff 直接答复。TUI：/review [paths] /security-review 发命令，结果即普通回答流。测试：改动两文件后 review 的 prompt 包含 diff 文本与审查指令；clean 仓库返回提示。

### Task 4: /memory 与 /remember
`memory` → 返回 ~/.tango/AGENTS.md 内容（无则空+提示）；`remember {text}` → 单行追加（时间戳前缀注释）。TUI：/memory 打开 viewer（ctrl+e 可编辑——viewer 可编辑路径已有）；/remember 一句话追加并确认。测试：remember 后 memory 内容含该行；新会话 buildContext 包含它（全局记忆生效闭环）。

### Task 5: /skills
`skills` → 列出全局+项目技能（name+description+来源路径）。TUI：/skills 打开 picker（只读展示，选中显示路径）。测试：临时技能目录下 list 正确、来源标注。

### Task 6: /goal 持续目标
serve：`goal {text}` 设定、`goal off/pause/resume`、`goal max N`；回合结束后（result 后）若 goal 活跃且未达最大轮数：用 agent.ask 无工具判断"目标是否已达成（YES/NO）"，NO → 自动发下一轮 prompt（注入目标与上轮摘要），YES 或超限 → 通知并清除。interrupt 清 goal 自动续轮（RF#1）。TUI：/goal 命令族 + 状态栏显示 ●goal。测试：mock 脚本让第一轮 ask 回 NO、第二轮 YES → 断言恰好一次自动续轮且随后清除；interrupt 后不再续。

### Task 7: /loop 定间隔重复
serve：`loop {everySec} {prompt}` ≥10s；内部 setTimeout 链发 prompt；`loop off` 与 interrupt 停止；stdin EOF 清理（RF#2）。TUI：/loop 设置/停止。测试：2s 间隔（测试专用下限豁免参数）跑 3 次后 off → 恰好 3 个 prompt 结果；interrupt 即停。

### Task 8: hooks 工具钩子
config `hooks?: { beforeTool?: string; afterTool?: string }`；serve 安装总线监听：调用 `sh -c <cmd>`，工具调用 JSON 经 **stdin** 传（RF#5），失败 stderr 警告不阻塞、不 veto。测试：beforeTool 写标记文件 → 工具执行后文件存在且内容含工具名；坏命令不阻塞。

### Task 9: README + 全量 + tmux 实测
README 批次 3 一节；全量测试；tmux 真实走：/plan 拒写、/effort 设置、/review 出报告、/remember 后 /memory 可见、/goal 两轮达成、/loop 3 次停止、hooks 标记文件。
