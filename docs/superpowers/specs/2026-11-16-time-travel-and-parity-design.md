# Tango 全量时间旅行 + 命令全量补齐 设计文档

日期：2026-11-16 · 状态：待用户批准（设计问题已由用户拍板，见 §3.4 决定记录）

## 一、意图与为什么

Tango 要有一个别家只有半成品、用户却离不开的能力，并把 OpenCode / Claude Code / Codex / Grok Build 的斜杠命令全部补齐。

**护城河选型：全量时间旅行。** 依据：
- 四家都有 `/rewind` 或 `/undo`，但每家都只做了一半（Claude Code 回滚代码+对话但不分叉；OpenCode 的 /undo 只退最后一条消息；Codex /fork 不带代码状态；Grok /rewind 只回对话）。用户对"agent 搞坏了怎么办"的恐惧是这类工具最大的心理摩擦，全量方案消除它。
- tango 的架构恰好是最优土壤：JSONL 会话日志已是唯一事实源（DeepSeek harness 原则："模型可见即已记录"）；`tango serve` 是唯一会话/文件边界（检查点逻辑放 serve，UI 纯下游）；git 仓库在手（代码快照不需要额外基础设施）。

**补齐范围：全部硬补**，含大件（后台任务、子代理、worktree、MCP、hooks）。云端专属能力（账户、付费、云分享）做本地替代并写明。

## 二、目标 / 非目标 / 反目标

**目标**
1. 时间旅行完整形态：每回合自动检查点，代码+对话+上下文三位一体可回滚、可分叉、可重放。
2. 四家竞品命令集的并集全部实现（约 60+ 新命令/能力），本地可行的做到同等或更好。
3. 吸收 DeepSeek harness 三原则：日志即真理（一切从日志投影）、UI 纯下游（逻辑在 serve，TUI 只收发协议帧）、明确的不做清单。

**非目标（本期）**
- 云账户/订阅/团队协作；云链接分享（`/share` 用本地 HTML 导出替代）。
- 插件市场（`/plugins` 做本地插件目录加载，不做在线市场）。
- Web/IDE 前端（协议保持可扩展即可）。

**反目标（做失败的标准）**
- 回滚丢用户工作区里与 agent 无关的未提交修改。
- 检查点拖慢回合体感（每次写工具增加的延迟必须 < 100ms 常规仓库）。
- 命令补齐破坏现有输入/权限/Neovim/会话恢复/Vim 行为（此前 11 项任务保住的一切）。
- 时间旅行污染模型上下文或污染日志（被放弃的未来必须可完整找回）。

## 三、核心设计：全量时间旅行

### 3.1 检查点模型：每回合 = 一个检查点（粒度已定：每回合一次）

一个检查点 = `(git_tree_snapshot, message_index, 上下文投影位置)` 三元组，写入 JSONL 会话日志：

- **粒度（用户已定）**：每回合在**第一个有写入副作用的工具调用之前**抓一次快照，`bash` 一律算写入（保守起见，快照成本低）。无写入的回合不抓 git 快照，但仍出现在时间轴上（该节点只回对话）。`/undo` 一步 = 一个回合。不做"每个写工具一次"的密集模式（如将来需要再加配置）。
- **归属层（建造者检查修正）**：检查点逻辑放在核心层 `makeAgent`/事件总线（`src/host.ts` 的 `before_tool` 事件 seam），不是字面上的 serve.ts —— REPL 与定时任务守护进程的会话同样获得快照，serve/TUI/REPL 全部下游化。
- **代码快照**：用临时 index（`GIT_INDEX_FILE` 指向临时文件，`git add -A` 含未跟踪文件、不含 .gitignore 忽略文件）+ `git write-tree` + `git commit-tree` 生成快照 commit，挂到隐藏引用 `refs/tango/checkpoints/<sessionId>` 的链上，**不动用户的工作区和分支**。快照 hash 记入日志条目。
- **对话位置**：JSONL 消息序号即天然检查点，无需额外存储。
- **上下文投影**：上下文 = 日志前缀的纯投影（deriveMessages 原则）。rewind 后截断到目标序号，模型自然只看到过去。压缩摘要也作为日志条目参与投影，回滚自动回到当时的压缩状态。

### 3.2 命令面（时间旅行家族）

| 命令 | 行为 |
|---|---|
| `/rewind` | 打开时间轴选择器：每回合一个节点（时间、首行摘要、token/成本、改动文件数、是否分叉点）；选中即整体回滚 |
| `/undo` `/redo` | 单步后退/前进，代码+对话+上下文一起走 |
| `/fork [name]` | 从当前（或 `/rewind` 选中）节点复制出新会话：新 session 文件 + 复制 git 快照链；原会话不动，继续在 fork 树上生长 |
| `/branch [name]` | 轻量对话分叉：共享代码检查点链，不复制会话文件（适合"试试另一种问法"） |
| `/tree` | fork 树可视化（本会话的祖先+后代） |
| `/find <text>` | 全部会话+全部检查点全文搜索，结果可直接 `/resume` 或 `/rewind` 到位 |
| `/export [html\|md]` | 从日志投影完整对话（含工具、diff、检查点），HTML 单文件本地替代云端 /share |
| `/transcript` | 在 $PAGER/nvim 中打开当次会话完整 transcript |
| `/copy [N]` | 复制最后（或倒数第 N 个）助手回复到系统剪贴板 |

**被放弃的未来不丢失**：rewind 截断的消息移入日志的 `orphaned` 区段（仍是日志的一部分，投影时跳过），`/redo` 恢复投影，`/tree` 可见。文件系统的未来修改同理：rewind 后新检查点照常快照，旧未来分支在 `/tree` 中可再 fork 回来。

### 3.3 恢复语义（关键细节，逐条过）

1. **智慧删除（用户已定）**：回到检查点 N 时——
   - 恢复/覆盖：快照 N tree 里的所有文件；
   - 删除：**不在快照 N、但出现在更晚检查点 tree 里**的文件（即被回滚掉的 agent 工作创建的遗留）；
   - 一律不动并提示"已保留"：不在任何快照里的文件（你手写的新文件）和 .gitignore 忽略文件（.env、构建产物——从未被快照，也永不被回滚触碰）。
   这样"代码真回去了"且零丢失。
2. **恢复操作**：`git read-tree <snapshot> && git checkout-index -a -f`（通过临时 index，或 `git restore --source=<snapshot> --worktree -- :/`）+ 上述删除规则；实现期用真实仓库场景验证，绝不 force-delete 用户分支。
3. **非 git 目录**：快照降级为目录 tar（`~/.tango/snapshots/<sessionId>/<n>.tar.zst`），语义不变。
4. **单一写者（用户已定：worktree 隔离）**：主工作区永远单写者。批次 4 的后台任务/子代理一律在自己的 git worktree 里跑（与 /worktree 命令同一套机制），检查点链按 worktree 各自独立。同一工作区内的并发写一律排队串行化。
5. **性能**：write-tree 是 O(改动文件数) 的对象写入，每回合只做一次，常规仓库 <100ms；不做密集模式故无需 off 配置（保留 `checkpoint: on|off` 开关即可）。
6. **旧会话降级**：没有快照条目的历史会话，/rewind 降级为只回对话不回代码，时间轴节点明确标注"无代码快照"。
7. **/find 范围（已定）**：只搜全部会话的**对话文本**（含 orphaned 未来）；不做对快照文件内容的全文检索。

## 四、命令补齐总清单（全部硬补，按落地批次）

> 每批独立可交付、独立可验证。T=已有，★=时间旅行家族（上节）。

**批次 0 — 时间旅行（第一优先级）**：★/rewind ★/undo ★/redo ★/fork ★/branch ★/tree ★/find ★/export ★/transcript /copy /rename /title /recap

**批次 1 — 会话与上下文可观测**：/context（上下文构成网格：系统提示/skills/工具schema/历史各占多少 token）、/queue（排队中的提示）、/timestamps（时间戳开关）、/density（工具详情 3 级）、/usage /stats（/cost 扩展：分模型/分会话聚合）、/insights（HTML 使用报告）、/statusline（自定义状态栏模板）、/autocompact（阈值自动压缩）、/presets（保存/复用提示模板）

**批次 2 — 输入与配置**：/editor（$EDITOR 通用化，/nvim 保留为特例）、/multiline 开关、/permissions（用户已定：**建持久允许规则表**——工具名 + 路径前缀 + bash 命令前缀三种规则，作为 decide() 之上的增量 seam，/permissions 增删改查）、/config /settings（打开并校验 config.json）、/keymap（重映射表）、/cd /pwd /add-dir（会话目录与附加目录）、/init（生成 AGENTS.md/CLAUDE.md 脚手架）、/import（导入他家的 allowed-tools 映射到规则表，映射不了的明确报告跳过）、/doctor（环境体检）、/feedback→/debug-zip（本地诊断包替代上传）

**批次 3 — 工作流模式**：/plan（计划模式：agent 只读研究、产出计划、批准后执行）、/goal（持续工作直到条件满足/预算耗尽）、/loop（定间隔重复提示）、/effort（推理力度）、/review（代码审查：diff→结构化报告）、/security-review、/memory（编辑长期记忆文件+自动召回）、/remember（一句话存记忆）、/skills（技能列表/启用，T:/skill 扩展）、/hooks（工具调用前后钩子脚本）、/plugins（本地插件目录）

**批次 4 — 大件**（用户已定：**后台/子代理一律 worktree 隔离**，主工作区单写者）：/bg /background（提示转后台，detach 到独立 worktree）、/tasks /ps（后台任务列表/终端）、/stop（停后台任务）、/subtask（派生子代理分支，独立 worktree）、/agents（子代理类型管理）、/worktree（git worktree 管理：创建/切换/合并，同时是批次 4 的隔离基础设施）、/mcp（MCP 服务器配置与工具桥接进 serve 工具注册表）

**不做（写进 README）**：/login /logout /upgrade /share-云链接 /市场类 /voice /daybreak /imagine-video /teleport —— 属于云账户或与本地单人定位冲突，用本地替代（/export、/insights）。

## 五、DeepSeek 三原则的落地方式

1. **日志即真理**：检查点、fork、压缩、后台任务结果全部是 JSONL 日志条目；`/export /find /transcript /rewind /tree` 全部从日志投影，不另建数据库。每条模型请求可从日志重建。
2. **UI 纯下游**：新命令逻辑全部在 `src/serve.ts`/`src/core/*`，协议加帧类型；TUI 只发命令、渲染结果帧。复审时以"TUI 里有没有业务判断"为红线。
3. **明确不做清单**：见批次 4 末尾；与 DeepSeek "刻意不引入" 文档对应，写进 README 和本文档。

## 六、约束

- 语言/框架不变：TS core（serve）+ Rust ratatui TUI；协议仍 JSONL over stdio。
- 不替换：JSONL 存储、模型适配器、权限系统、agent 工具循环的现有边界（在其上加 seam）。
- 每个命令 TDD：先测后实现；README 同步更新；不动既有 69 Rust + 60 TS 测试的绿色基线。
- 大件（MCP/hooks/子代理/worktree）各自独立设计一页附录，动工前再过一次用户。

## 七、交付顺序与验收

批次 0 → 1 → 2 → 3 → 4，每批：测试绿 + README 更新 + tmux 真实场景验收（含 mock 模型）+ 单独提交。批次 0 验收场景：改文件→rewind 文件回来、从中间 fork 两条路、/find 找回被放弃的未来、/export HTML 离线可读。

## 八、决定记录（2026-11-16 用户拍板）

1. 护城河方向：全量时间旅行（四候选中选定）。
2. 命令补齐范围：全部硬补（含大件），云端类做本地替代。
3. 检查点粒度：每回合一次（首个写副作用工具前；bash 一律算写入；无写回合只进时间轴不抓快照）。
4. 回滚删除语义：智慧删除——agent 遗留删、用户手写与 ignore 文件保留并提示。
5. 后台/子代理并发：worktree 隔离，主工作区永久单写者。
6. 权限：建持久允许规则表（工具名/路径前缀/bash 前缀），decide() 之上的增量 seam。

建造者检查中由实施者自定（用户可否决）：/find 只搜对话文本；检查点逻辑放核心层事件总线（REPL/守护进程同样受益）；旧会话 rewind 降级为仅对话。
