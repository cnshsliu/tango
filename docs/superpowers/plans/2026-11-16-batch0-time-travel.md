# 批次 0 · 全量时间旅行 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 每回合自动检查点（git 快照 + 日志位置 + 上下文投影），`/rewind /undo /redo /fork /branch /tree /find /export /transcript /copy /rename /recap` 全家族落地，被放弃的未来永不丢失。

**Architecture:** 检查点在核心层事件总线（`installCheckpoints` 订阅 `before_tool`，`prompt_start` 重置回合标志），快照用临时 git index（不动用户分支/索引）；回滚/分叉全部是 JSONL 日志条目（`checkpoint`/`rewind`），`messages()` 按"最后 rewind 的目标"做水位线投影；serve 只加协议命令，TUI 纯下游收发。

**Tech Stack:** TypeScript/Node（core + serve）、Rust ratatui（TUI）、node:test、tmux + mock SSE 端到端。

**Spec:** `docs/superpowers/specs/2026-11-16-time-travel-and-parity-design.md`（§3 检查点模型与恢复语义、§8 决定记录）

## Global Constraints

- 粒度：每回合一次，首个 `writeEffect` 工具前；`bash` 一律算写入；无写回合只进时间轴不抓快照（决定记录 #3）。
- 智慧删除：回滚删"不在目标快照但在更晚快照里"的文件；不在任何快照的文件与 .gitignore 文件一律保留并报告（#4）。
- 检查点逻辑放核心层（makeAgent/EventBus），REPL 与任务守护进程同样受益（建造者检查修正）。
- 单一写者：本批次不改并发模型（#5 属批次 4）。
- 快照不碰用户分支、不碰用户 index（`GIT_INDEX_FILE` 临时文件）、含未跟踪不含忽略文件。
- 不破坏既有 60 TS + 69 Rust 绿色基线；每个命令 README 同步。
- 无快照的旧会话：/rewind 降级为仅对话，时间轴标注"无代码快照"。
- `/find` 只搜对话文本（含被放弃分支），不搜快照文件内容。

## Review Focus

1. **中断撞上快照**：before_tool 快照进行中用户按了 ctrl+c —— 快照 await 不得阻塞或吞掉 abort 路径（snapshot 完成或失败都继续工具执行/中断）。测试：Task 5。
2. **无写回合与回滚目标**：/undo 的目标回合可能没有快照 —— 对话仍要退到该回合开始处（用户消息前），代码不动。测试：Task 7。
3. **rewind 跨 compaction**：回滚目标在 compaction 条目之前 —— 投影必须重新包含压缩前的完整消息（水位线盖过 compaction 扫描）。测试：Task 6。
4. **会话在别的目录 resume**：快照/恢复以 `store.meta().cwd` 的 git 仓库根为准，不是进程 cwd。测试：Task 2。
5. **fork 后两线独立回滚**：fork 出的会话 rewind 不得影响原会话的投影或工作区（共享的是不可变快照对象，不是状态）。测试：Task 8。

---

### Task 1: 工具写入效应标记

**Files:**
- Modify: `src/core/types.ts`（`Tool` 接口）
- Modify: `src/core/tools/fs.ts`、`src/core/tools/bash.ts`、`src/core/tools/office/index.ts`
- Test: `tests/session.test.ts`（追加）

**Interfaces:**
- Produces: `Tool.writeEffect?: boolean`（缺省 false）。标记为 true 的工具：`write`、`edit`、`bash`、`office_write`。Task 5 的检查点模块按 `call.name → tool.writeEffect` 判断。

- [ ] **Step 1: 失败测试** — 在 `tests/session.test.ts` 追加：`buildTools()` 后 `write/edit/bash/office_write` 四个工具 `writeEffect === true`，其余（`read/grep/find/ls/web_search/web_fetch/office_read/notify/diff`）无 `writeEffect` 或为 false。

```ts
test('write-effect tools are marked for checkpointing', async () => {
  const tools = buildTools();
  for (const name of ['write', 'edit', 'bash', 'office_write'])
    assert.equal(tools.get(name)?.writeEffect, true, name);
  for (const name of ['read', 'grep', 'find', 'ls', 'diff', 'notify'])
    assert.notEqual(tools.get(name)?.writeEffect, true, name);
});
```

- [ ] **Step 2: 跑测确认失败** — Run: `node --import tsx --test tests/session.test.ts`。Expected: FAIL（`writeEffect` 不存在）。
- [ ] **Step 3: 实现** — `types.ts` 的 `Tool` 加 `writeEffect?: boolean`；四个工具定义处各加 `writeEffect: true`。
- [ ] **Step 4: 跑测通过** — 同 Step 2 命令，PASS。
- [ ] **Step 5: Commit** — `git add -A && git commit -m "tools: mark write-effect tools for checkpointing"`

### Task 2: git 快照（临时 index，不动用户分支）

**Files:**
- Create: `src/core/git-snapshot.ts`
- Test: `tests/git-snapshot.test.ts`

**Interfaces:**
- Produces:
  - `takeSnapshot(cwd: string): Promise<string | null>` — 返回快照 commit hash；非 git 目录返回 null。用 `GIT_INDEX_FILE=<tmp>` + `git add -A`（含未跟踪）+ `git write-tree` + `git commit-tree -m "tango checkpoint"`（父提交 = 该会话上一快照可选，首版无父即可）。
  - `restoreSnapshot(cwd: string, target: string, later: string[]): Promise<{ restored: number; deleted: string[]; kept: string[] }>` — Task 3 实现删除语义，本任务先做纯恢复。
  - `snapshotFileList(cwd: string, hash: string): Promise<Set<string>>` — `git ls-tree -r --name-only <hash>`。
  - 全部走 `spawnSync('git', ...)`，`cwd` 解析为仓库根（`git rev-parse --show-toplevel`）后再操作。

- [ ] **Step 1: 失败测试** — 临时目录 `git init` + 配 user；写 `tracked.txt` 提交；再写 `untracked.txt`、`ignored.txt`（.gitignore 含 ignored.txt）。`takeSnapshot` 后断言：返回非空 hash；`git status --porcelain` 依旧干净（用户 index 未动）；`snapshotFileList(hash)` 含 tracked+untracked、不含 ignored；当前分支仍指向原 commit（`git rev-parse HEAD` 不变）。
- [ ] **Step 2: 跑测失败** — Run: `node --import tsx --test tests/git-snapshot.test.ts`。Expected: FAIL（模块不存在）。
- [ ] **Step 3: 实现** — 临时 index 放 `os.tmpdir()`；`git add -A` 在临时 index 上跑（环境变量注入）；`commit-tree` 读 write-tree 输出。**非 git 目录降级**（设计 §3.3.3）：`tar -czf ~/.tango/snapshots/<sessionId>/<seq>.tar.gz -C <cwd> .`，返回 `tar:<绝对路径>` 形式的快照 id；`snapshotFileList` 对 `tar:` 前缀用 `tar -tzf` 列表，`restoreSnapshot` 用 `tar -xzf` 到临时目录再按智慧删除规则拷回。
- [ ] **Step 4: 跑测通过 + Review Focus #4** — 追加测试：在仓库的子目录里调用 `takeSnapshot(subdir)`，快照仍覆盖整个仓库（FileList 含仓库根的文件）。
- [ ] **Step 5: Commit** — `git commit -m "core: git tree snapshots via temp index, user branch untouched"`

### Task 3: 智慧删除恢复语义

**Files:**
- Modify: `src/core/git-snapshot.ts`（完成 `restoreSnapshot`）
- Test: `tests/git-snapshot.test.ts`（追加）

**Interfaces:**
- Produces: `restoreSnapshot(cwd, target, later)` 完整语义：恢复 target tree 的全部文件（`git read-tree` 到临时 index + `git checkout-index -a -f`，或对每文件 `git show <hash>:<path>` 落盘——选实现简单者）；删除 = 磁盘上存在 ∧ 不在 target tree ∧ 在任一 later hash 的 tree 里；`kept` = 磁盘上存在 ∧ 不在 target ∧ 不在任何 later tree（用户手写文件），原样保留并列入返回值。

- [ ] **Step 1: 失败测试** — 三个快照场景断言：① agent 建了 `leftover.txt`（在 later 快照里）→ 回滚后被删；② 用户手写 `handmade.txt`（不在任何快照）→ 保留且出现在 `kept`；③ `.env`（.gitignore）→ 不动、不入 restored/deleted/kept 统计之外（可在 kept 里出现）；④ target 里改过的文件内容被恢复为 target 版本；⑤ 非 git 目录的 `tar:` 快照恢复同样满足 ①-④。
- [ ] **Step 2: 跑测失败** — 同 Task 2 命令。
- [ ] **Step 3: 实现** — 恢复用临时 index 的 `read-tree <target> && checkout-index -a -f`；删除集合 = `later hashes 的 FileList 并集 − target FileList − 磁盘不存在的`。
- [ ] **Step 4: 跑测通过** — 全套 `node --import tsx --test tests/git-snapshot.test.ts` PASS。
- [ ] **Step 5: Commit** — `git commit -m "core: smart-delete restore — agent leftovers removed, user files kept"`

### Task 4: 会话日志新条目与 rewind 水位线投影

**Files:**
- Modify: `src/core/types.ts`（SessionEntry 加两种）
- Modify: `src/core/session.ts`
- Test: `tests/session.test.ts`（追加）

**Interfaces:**
- Produces:
  - `{ kind: 'checkpoint'; ts: string; hash: string; note?: string }` 与 `{ kind: 'rewind'; ts: string; to: number }`（`to` = 投影可见的最后一个 entry 下标，含）。
  - `SessionStore.rewindTo(entryIndex: number): Promise<void>` — append rewind 条目。
  - `SessionStore.visibleEntries(): SessionEntry[]` — 水位线投影（见算法），`messages()` 改为基于它（compaction 扫描也在投影内做）。
  - `SessionStore.forkTo(dir: string, newId: string, toIndex: number): Promise<SessionStore>` — 复制 entries[0..toIndex] 到新文件（meta 重建为 `{...原meta, id: newId, forkOf: {sessionId, entryIndex}}`），meta 类型加 `forkOf?: { sessionId: string; entryIndex: number }`。
  - 水位线算法：从尾向前找最后一条 `rewind`，`hi = e.to`，否则 `hi = entries.length`；投影 = `entries.slice(0, hi)`。`allMessages()` 不变（全量，含被放弃的未来）。

- [ ] **Step 1: 失败测试** — ① 追加 3 条消息 + `rewindTo(下标 of 第1条 message entry)` → `messages()` 只含第 1 条；② rewind 后再追加新消息 → 投影 = 第1条 + 新消息；③ `allMessages()` 始终 5 条（被放弃的不丢）；④ 再 `rewindTo(更大下标)`（= redo）→ 被放弃的消息重新可见（Review Focus #2/#3 铺垫）；⑤ compaction 后 rewind 到 compaction 前 → 投影含压缩前全部消息（Review Focus #3）。
- [ ] **Step 2: 跑测失败** — `node --import tsx --test tests/session.test.ts`。
- [ ] **Step 3: 实现** — types.ts 加条目；session.ts 加 `rewindTo/visibleEntries/forkTo`，`messages()` 改用 `visibleEntries()`。
- [ ] **Step 4: 全量回归** — `npm test`（session 系全部绿；既有测试不改动预期）。
- [ ] **Step 5: Commit** — `git commit -m "session: rewind watermark projection, orphaned future preserved in log"`

### Task 5: prompt_start 事件 + 检查点安装器（核心层）

**Files:**
- Modify: `src/core/types.ts`（TangoEvents 加 `prompt_start: { text: string }`）
- Modify: `src/core/agent.ts`（prompt() append 用户消息后 `emit('prompt_start', { text })`）
- Create: `src/core/checkpoint.ts`
- Modify: `src/host.ts`（makeAgent 里 `installCheckpoints(events, { cwd, store })`，config `checkpoint !== 'off'` 才装）
- Modify: `src/core/config.ts`（可选 `checkpoint?: 'on' | 'off'`，缺省 on）
- Test: `tests/agent.test.ts`（追加，用已有 mock adapter 模式）

**Interfaces:**
- Produces:
  - `installCheckpoints(events: EventBus, opts: { cwd: string; store: SessionStore }): void` — 订阅 `prompt_start`（置 `armed = true`）与 `before_tool`（`armed && tool.writeEffect` 时 `await store.append({kind:'checkpoint', hash, ts})` 并置 `armed = false`；快照失败（非 git 目录返回 null）append 也不写）。never veto。
  - `latestCheckpoints(store): { hash: string; entryIndex: number }[]` — 供 Task 7 时间轴与 Task 3 恢复取链。

- [ ] **Step 1: 失败测试** — mock 适配器让首个回合调用 `write` 工具：① 断言会话日志含 checkpoint 条目且 hash 非空；② 同一回合第二个写工具不再产生第二条 checkpoint；③ 第二个 prompt 的首个写工具又产生一条；④ 纯读回合无 checkpoint 条目；⑤ checkpoint 后 git 仓库工作区内容被恢复验证（快照 hash 能被 `git cat-file -t` 识别为 commit）。
- [ ] **Step 2: 跑测失败** — `node --import tsx --test tests/agent.test.ts`。
- [ ] **Step 3: 实现** — 事件发射 + 安装器 + host 接线 + config 开关。Review Focus #1：`before_tool` 处理器里快照 try/catch 包裹，失败仅静默跳过（不 veto 不抛出），abort 路径不受影响。
- [ ] **Step 4: 跑测通过 + 回归** — `npm test` 全绿。
- [ ] **Step 5: Commit** — `git commit -m "core: per-turn git checkpoints on first write-effect tool, event-bus seam"`

### Task 6: serve 命令 — timeline / rewind / undo / redo

**Files:**
- Modify: `src/serve.ts`
- Test: `tests/serve.test.ts`（追加）

**Interfaces:**
- Produces（协议命令，全部排队外即时执行，与 status 同类）:
  - `timeline` → `{ nodes: [{ entryIndex, ts, firstLine, hasSnapshot, hash?, messageCount, costUsd? }] }` — 每个用户消息 entry 一个节点；`hasSnapshot` = 该回合是否产出 checkpoint。
  - `rewind { entryIndex }` → 找到该节点回合**之前最近的可回滚点**（总是允许：对话必回；若该回合有 hash 则同时 `restoreSnapshot`，取该 hash 为 target、其后全部 hash 为 later），append `rewindTo`，返回 `{ restored: n, deleted: [...], kept: [...] }`（无快照时 `{ codeRestored: false }`）。
  - `undo` / `redo` → 在时间轴节点序上后退/前进一步（等价 rewind 到相邻节点），redo 到"被放弃的未来"用 Task 4 的前进水位线。
  - Frames: `result` 携带上述数据；TUI 无需新帧类型。

- [ ] **Step 1: 失败测试** — 沿用 serve 测试架子（真实子进程 + mock SSE 脚本化两轮 write 工具调用，真实临时 git 仓库）：① `timeline` 返回 2 节点且 hasSnapshot；② `rewind` 回第 1 节点：文件内容回到第 1 轮后状态、agent 第 2 轮建的文件消失（智慧删除）、`history` 只剩第 1 轮；③ `undo`/`redo` 往返后内容与对话一致；④ 无写回合（纯文本回答）出现在 timeline 但 hasSnapshot=false，rewind 它只回对话。
- [ ] **Step 2: 跑测失败** — `node --import tsx --test tests/serve.test.ts`。
- [ ] **Step 3: 实现** — serve.ts 加四个 case；恢复细节复用 Task 2/3。
- [ ] **Step 4: 跑测通过 + 全量回归** — `npm test`。
- [ ] **Step 5: Commit** — `git commit -m "serve: timeline/rewind/undo/redo with real git restore"`

### Task 7: serve 命令 — fork / tree

**Files:**
- Modify: `src/serve.ts`
- Test: `tests/serve.test.ts`（追加）

**Interfaces:**
- Produces:
  - `fork { name?: string, entryIndex?: number }` → `store.forkTo(...)` + `git update-ref refs/tango/checkpoints/<newId> <hash链最新>`（无链则跳过）→ 返回 `{ sessionId: newId }`。TUI 收到后用现有 `TANGO_RESUME` respawn 机制切换。
  - `branch { name?: string }` → 轻量对话分叉（设计 §3.2）：不复制会话文件，等价于 `rewind`（可选 entryIndex）后**就地**继续——即 branch = rewind + 语义提示，`{ branchedAt }`。实现上直接复用 rewind 命令路径，返回里带 `branch: true` 标记，TUI 聊天行提示"已从 #N 分叉（原未来保留，/redo 可回）"。
  - `tree` → `{ nodes: [{ sessionId, name?, createdAt, forkOf?, messageCount, lastTs }] }`，沿 `forkOf` 链构成森林（TUI 渲染）。

- [ ] **Step 1: 失败测试** — ① fork 后新 session 文件存在、前缀条目正确、`forkOf` 记录正确；② 原会话继续追加不影响 fork 出的会话，反之亦然（Review Focus #5）；③ `tree` 能看到两者的父子关系；④ fork 目标会话在另一个 TANGO_HOME resume 后 `history` 正确；⑤ `branch` 不新建文件、投影退到分叉点、`allMessages` 完整。
- [ ] **Step 2: 跑测失败** → **Step 3: 实现** → **Step 4: `npm test` 全绿**。
- [ ] **Step 5: Commit** — `git commit -m "serve: session fork with shared snapshot objects, tree ancestry"`

### Task 8: serve 命令 — find / export / transcript / rename / recap

**Files:**
- Modify: `src/serve.ts`
- Test: `tests/serve.test.ts`（追加）

**Interfaces:**
- Produces:
  - `find { text }` → `{ hits: [{ sessionId, entryIndex, ts, firstLine, snippet, orphaned }] }` — 扫描 `sessionsDir()` 全部 JSONL（含 orphaned 段），大小写不敏感，每会话最多 20 条、总量 200 条。
  - `export { format: 'md' | 'html' }` → `{ path }` — 从 `allMessages()` 渲染单文件写到 `<cwd>/tango-export-<id>.<ext>`；HTML 内联 CSS 无外部依赖；含工具调用摘要与 rewind 分隔线（"⟲ 已回滚的未来"段）。
  - `transcript` → `{ text }` — 完整原始投影文本（含 orphaned，标注），TUI 用现有 viewer 展示。
  - `rename { name }` → append meta 条目（name 更新），`{ name }`。
  - `recap` → 用 `agent.ask()`（不写会话）对当前上下文要一行总结，`btw_stream` 复用为流式帧 + result 文本。

- [ ] **Step 1: 失败测试** — ① find 能命中已 rewind 掉的 orphaned 消息且 `orphaned: true`；② export md 文件真实存在且包含两轮工具调用文本；③ rename 后 `sessions` 列表显示新名字；④ recap 返回非空文本且 `store.allMessages()` 字节数不变。
- [ ] **Step 2: 跑测失败** → **Step 3: 实现** → **Step 4: `npm test` 全绿**。
- [ ] **Step 5: Commit** — `git commit -m "serve: find across sessions incl orphaned futures, export, transcript, rename, recap"`

### Task 9: TUI — 时间轴选择器与 /rewind /undo /redo /fork /branch /tree

**Files:**
- Modify: `tui/src/app.rs`（SLASH、Pending、PickerKind::Timeline、run_slash 分支）
- Test: `tui/src/app.rs` tests（追加）

**Interfaces:**
- Consumes: Task 6/7 协议（timeline/rewind/undo/redo/fork/branch/tree）。
- Produces: `Pending::{Timeline, Rewind, Undo, Redo, Fork, Branch, Tree}`；`PickerKind::Timeline`（节点 label = `#N 时间 · 首行 · ✓快照/无快照`，note = tokens/cost，选中 → serve `rewind`）；fork 结果 → 复用 `switch_session` respawn；tree 结果 → `Viewer::read_only` 渲染缩进树。

- [ ] **Step 1: 失败测试** — ① `/rewind` 发 `timeline` 命令并打开 Timeline picker（mock frame 注入节点后 Enter 发 `rewind {entryIndex}`）；② `/undo` `/redo` 直接发命令，result 后聊天行显示 restored/deleted/kept 摘要；③ `/fork` result 后调用 respawn（复用会话切换测试模式）；④ `/tree` 打开只读 viewer 且内容含两个 session 的树形；⑤ `/branch` 发 rewind 命令且 result 带 branch 标记时聊天行提示分叉。
- [ ] **Step 2: 跑测失败** — `cargo test --manifest-path tui/Cargo.toml`。
- [ ] **Step 3: 实现** — 复用 Task 1（palette）的 Picker 过滤/鼠标路由。
- [ ] **Step 4: 跑测通过 + 全 Rust 回归**。
- [ ] **Step 5: Commit** — `git commit -m "tui: timeline picker, rewind/undo/redo/fork/tree wiring"`

### Task 10: TUI — /find /export /transcript /copy /rename /recap 与 README

**Files:**
- Modify: `tui/src/app.rs`、`README.md`
- Test: `tui/src/app.rs` tests（追加）

**Interfaces:**
- Consumes: Task 8 协议。
- Produces: `/find <text>` → hits picker（选中当前会话 → rewind；他 会话 → respawn resume；orphaned 打标）；`/export [md|html]` → result path 聊天行 + 打开提示；`/transcript` → `Viewer::read_only`；`/copy [N]` → 本地 `clipboard::copy` 最近（倒数第 N 个）助手回复；`/rename <name>`；`/recap` → 复用 btw 流式对话框样式。SLASH 表与 `/help`、README 同步（含智慧删除语义、checkpoint 开关、无快照降级说明）。

- [ ] **Step 1: 失败测试** — ① `/find` 打开 picker 且 Enter 对当前会话命中发 rewind、对他会话命中走 respawn 路径；② `/copy` 调用 clipboard 且不影响会话行；③ `/transcript` viewer 文本含 orphaned 标注；④ SLASH 表包含全部新命令。
- [ ] **Step 2: 跑测失败** → **Step 3: 实现** → **Step 4: 全 Rust 回归 + `cargo fmt --check` + release build**。
- [ ] **Step 5: Commit** — `git commit -m "tui: find/export/transcript/copy/rename/recap; README time-travel docs"`

### Task 11: 真实端到端验收（用户要求的真测）

**Files:**
- Test: `tests/e2e-timetravel.test.ts`（新，真实子进程 + mock SSE + 真实 git 仓库，覆盖 Review Focus 全部五项）
- 验收脚本: 手动 tmux 场景清单写入 `docs/superpowers/plans/2026-11-16-batch0-time-travel.md` 本任务下勾选

**Interfaces:** 无新产品接口；这是验收关卡。

- [ ] **Step 1: 自动化真测**（沿用 serve 测试架子，全链路真进程）：
  - 场景 A：两轮真实 write 工具 → `/undo` → 断言磁盘文件回到第一轮后内容、第二轮新文件消失、`history` 对话同步；`/redo` → 全部回来。
  - 场景 B：回合间用户手写 `handmade.txt` → rewind → 文件仍在且 result 的 `kept` 列出它。
  - 场景 C：中途 `/fork` → 两会话各自追加、各自 rewind 互不影响（RF #5）。
  - 场景 D：`/find` 找回被放弃的未来文本（orphaned 命中）→ rewind 前进恢复。
  - 场景 E：`/export html` 文件存在、包含全部轮次、离线浏览器可读（断言无外链 `http` 引用）。
  - 场景 F：ctrl+c 中断恰逢写工具回合 → 下一次 rewind 链仍完整（RF #1）。
  - 场景 G：快照耗时断言 < 100ms（常规小仓库，RF 性能预算）。
- [x] **Step 2: tmux 手动真测**（真实终端跑 `tango` release 二进制 + mock 模型）：逐项走 A-G 的交互版 + 时间轴 picker 鼠标/过滤 + `/tree` 渲染，记录到本文件勾选框。
  - 实测记录（2026-11-17，release 二进制 + 真实 git 仓库 + mock SSE）：两回合真实写盘✓；/undo 代码回滚+junk 智慧删除+聊天行报告✓；/redo 恢复✓；/rewind 时间轴 picker（✓ 代码快照标记）选中回滚✓；/tree 只读查看器✓；/find 跨会话命中（他会话标签）✓；/export md 真实文件含回滚标记✓。
- [ ] **Step 3: 全量验证** — `npm run typecheck && npm test && cargo test && cargo build --release`（tui）+ `git diff --check`，全绿。
- [ ] **Step 4: Commit** — `git commit -m "e2e: real-process time-travel acceptance — undo/redo/fork/find/export real git scenarios"`

---

## 批次 1-4 说明

本计划只覆盖批次 0（护城河）。批次 1（可观测）/批次 2（配置与权限规则表）/批次 3（工作流模式）/批次 4（worktree 隔离的后台与子代理、MCP、hooks）在前一批落地后各出独立计划；批次 4 大件动工前按设计文档 §六 先过附录设计。
