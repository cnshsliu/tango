# 批次 4 · worktree 隔离、后台、子代理、MCP、插件 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** /worktree /bg /tasks /stop /subtask /agents /mcp /plugins + 按角色模型路由（qwen4 T3）。

**Architecture:** 见附录设计 `docs/superpowers/specs/2026-11-17-batch4-appendix-design.md`（权威）。worktree 是唯一隔离单元；后台/子代理都是"专属 worktree + 独立 serve 子进程"；MCP 是 stdio JSON-RPC 客户端桥进工具注册表；插件是本地目录的 hooks/skills 装载。

**Tech Stack:** 同前。**Spec:** 附录设计 + 主设计 §四 批次 4、§8 #5。

## Global Constraints

- 主工作区永久单写者：任何后台/子代理都不得在主工作区执行写工具（spawn 时 cwd=worktree）。
- 登记文件（worktrees.json / tasks-bg.json）写入走 tmp+rename 原子替换。
- MCP 服务器崩溃/超时绝不影响 serve 存活；其工具调用返回错误文本。
- 不破坏 TS 108 / Rust 98 基线。
- 远端：每个任务提交，批次合并后 push main。

## Review Focus

1. 孤儿后台进程：serve 退出后 bg 子进程必须可被 /tasks 识别为 dead 并允许清理。
2. worktree remove 的安全闸：脏 worktree 不 --force 不删；merge 冲突不吞。
3. MCP JSON-RPC 的超时与半开连接（服务器 hang）：request 带超时，超时后该服务器标记断连。
4. 子代理写回主会话的竞态：主会话正跑回合时 lazy-check 不追加（下一轮再查）。
5. roles 配置缺模型/key：回落当前模型并在结果里注明。

---

### Task 1: worktree 核心（core + serve + TUI）
`src/core/worktrees.ts`：登记读写（原子）、create/list/remove/merge（spawnSync git，worktree 目录 `~/.tango/worktrees/<name>`，分支 `tango/<name>`）。serve：`worktree_create/list/remove/merge`（immediate）。TUI：/worktree picker + new/rm/merge 子命令。测试：真实 git 仓库 e2e——create 建 worktree+分支、脏 worktree remove 拒绝、merge 真实合并、--force 删。

### Task 2: 后台任务 /bg /tasks /stop
`src/core/bgtask.ts`：spawn 独立 serve（`node cli serve --cwd <worktree>`，stdin 喂 prompt 后保持开启、stdout 日志文件）、登记、pid 存活检测、孤儿回收。serve：`bg {prompt,name?}`、`tasks`、`stop {taskId}`。TUI 三命令。测试：mock SSE + 真实子进程跑完 → tasks 状态 done、其会话文件存在且可 open；stop 中断运行中任务。

### Task 3: roles 角色路由 + /agents（qwen4 T3）
config `roles?: {driver?, subagent?, longContext?}`；serve `agents_show`/`agents_set {role, model}`（校验模型存在+key，回落说明）。/subtask 默认 subagent 角色、/bg --long 用 longContext。TUI：/agents viewer+set。测试：roles 设 flash → bg 子进程的 hello.model=flash（日志/会话断言）；非法模型拒绝。

### Task 4: /subtask 子代理写回
bg 的特化：`subtask {prompt, name?}` = bg + 完成后把子会话最后 assistant 消息摘要追加进主会话（`message` 条目，前缀 `⑂ subtask <name>:`）。lazy 检查：任意 serve 命令到达时扫 running subtasks，完成的做写回（主会话 busy 时跳过本轮）。TUI：/subtask。测试：子进程完成后主会话 history 含 ⑂ 摘要；运行中主回合不打扰。

### Task 5: MCP 客户端与工具桥接
`src/core/mcp.ts`：JSON-RPC stdio 客户端（initialize/tools-list/tools-call，10s 超时，断连标记）。serve 启动按 config `mcpServers` 拉起并注册 `mcp_<server>_<tool>`（writeEffect true）。`mcp_list` 命令。TUI /mcp。测试：tests/fixtures/mock-mcp-server.mjs（echo 工具）→ hello.tools 含 mcp_mock_echo、prompt 调用它返回 echo 结果；服务器死亡后调用返回错误文本 serve 不死。

### Task 6: /plugins 最小版
`~/.tango/plugins/<name>/plugin.json` {name,description,enabled?,hooks?,skills?}；serve 启动装载（enabled!==false）：hooks 合并、skills 目录并入发现路径。`plugins_list`。TUI /plugins。测试：临时插件目录带 hook 脚本 → 工具调用时 hook 执行；enabled:false 不装载。

### Task 7: README + 全量 + tmux 真机
README 批次 4 一节（含不做清单）；全量测试；tmux：/bg 完成→/tasks done→/resume 其会话；/subtask 摘要回主会话；/mcp 接 mock 服务器调一个工具；/worktree merge 真合并。
