# 批次 4 附录设计 · worktree 隔离、后台任务、子代理、MCP、插件

> 依据主设计 §六"大件动工前各过一页附录设计"与 §8 决定 #5（worktree 隔离，主工作区
> 永久单写者）。用户已全权委托（2026-11-17）。

## A. /worktree —— 隔离地基

**模型**：git worktree = 一个受管工作副本。tango 的 worktree 记录在
`~/.tango/worktrees.json`：`{ name, path, branch, createdAt, note }`。目录统一放
`~/.tango/worktrees/<name>/`（不污染用户仓库目录），分支 `tango/<name>`。

**命令（serve）**：
- `worktree_create {name, base?}` → `git worktree add <path> -b tango/<name> <base|HEAD>`；登记。
- `worktree_list` → 登记表 + 各自 `git status --porcelain` 摘要（脏文件数）。
- `worktree_remove {name}` → 存在未提交修改时拒绝（--force 才删）；`git worktree remove` + 清登记 + 删分支（分支有未合并提交时保分支只删副本）。
- `worktree_merge {name}` → 在主工作区 `git merge tango/<name>`（不自动 commit 冲突，冲突原样返回）。

**TUI**：/worktree 打开列表 picker（脏文件数、分支）；`/worktree new <name>`、
`/worktree rm <name>`、`/worktree merge <name>`。

## B. /bg /tasks /stop —— 后台提示

**模型**：后台提示 = 专属 worktree（`bg-<seq>`）里的**独立 serve 子进程**跑同一提示。
主 TUI 零阻塞；结果落成普通会话文件，可 /resume 查看。任务记录进 `~/.tango/tasks-bg.json`：
`{ id, name, prompt, worktree, sessionFile, pid, status: running|done|failed|stopped, startedAt, endedAt }`。

**命令（serve）**：
- `bg {prompt, name?}` → 建 worktree → spawn `tango serve`（同模型/模式，cwd=worktree，
  stdout 重定向到日志文件）→ 发 prompt → 登记。主会话立即返回 `{taskId, worktree}`。
- `tasks` → 列出全部后台任务（状态、耗时、worktree、会话）。
- `stop {taskId}` → kill 进程组、标记 stopped、清 worktree（保留会话文件）。
- 孤儿检测：tasks 时扫 pid 存活状态，进程死了标 done/failed 并解析其会话文件的回合数。

**TUI**：/bg <prompt>、/tasks（viewer 表格）、/stop <id>；完成时下次 /tasks 可见。

## C. /subtask /agents —— 子代理 + 按角色路由（qwen4 T3）

**模型**：子代理 = 后台任务的特化：独立 worktree + 独立会话 + **角色化模型** + 完成后
把摘要写回主会话（作为一条 assistant 消息追加，含 diff 摘要与 worktree 名）。

**角色（config `roles`，qwen4 T3）**：
```json
{ "roles": { "driver": "qwen/qwen4-max", "subagent": "qwen/qwen4-flash", "longContext": "qwen/qwen4-max-1m" } }
```
缺省：driver=当前模型，subagent=当前模型，longContext=当前模型。/agents 查看设置。
`/subtask <描述>` 用 subagent 角色；后台长上下文任务可用 longContext（`/bg --long`）。

**写回**：子代理进程结束时，主 serve（在下一次任意命令时 lazily 检查）把
`<subtask 摘要>`（从子会话最后一条 assistant 消息截取）push 进主会话日志为
`message`（role=assistant，前缀 `⑂ subtask <name>:`），TUI 下次 history 可见。

## D. /mcp —— MCP 服务器桥接

**模型**：config `mcpServers: { <name>: { command, args?, env? } }`。serve 启动时对每个
服务器做 stdio JSON-RPC 握手（initialize → tools/list），把每个 MCP 工具注册为 tango 工具
`mcp_<server>_<tool>`（schema 直传；writeEffect 一律视为 true——外部工具副作用未知）。
调用时 tools/call，结果 text 内容块拼为工具输出。服务器死亡/超时：该服务器的工具标记
不可用（调用返回错误文本，不崩 serve）。

**协议实现（src/core/mcp.ts）**：最小 JSON-RPC 客户端——spawn、id 计数、`request(method,
params)` 带超时、`notification`。握手表：protocolVersion "2024-11-05"、clientInfo tango/版本。

**TUI**：/mcp 列出服务器与其工具（+连接状态）。测试用 node 写的 mock MCP server 脚本。

## E. /plugins —— 本地插件目录（最小版）

**模型**：`~/.tango/plugins/<name>/plugin.json`：`{ name, description, hooks?:
{beforeTool?, afterTool?}, skills?: true }`。serve 启动加载：hooks 合并进 hooks 系统
（与 config hooks 并存，插件 hooks 失败同样不阻塞）；`skills: true` 把插件目录下的
`skills/` 并入技能发现路径。/plugins 列表 + 启停（`enabled: false` 记在 plugin.json）。
不做市场、不做热载。

## F. 不做清单（本批）
- 多 agent 并发写主工作区（永远禁止，worktree 是唯一隔离手段）
- MCP 的 sampling/roots/server→client 能力（只做 client 侧 tools）
- 插件市场、远程插件、签名校验

## 验收
- 自动化：每个命令 mock-SSE e2e（bg/subtask 用真实 git worktree + 真实子进程，等完成断言产物）
- tmux 真机：/bg 跑完可 /resume 其会话；/subtask 摘要写回主会话；/mcp 接 mock 服务器调用一个工具；/worktree merge 真实合并
