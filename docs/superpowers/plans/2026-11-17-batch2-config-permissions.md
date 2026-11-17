# 批次 2 · 配置与权限规则表 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 持久允许规则表 + /permissions /import /editor /multiline /config /keymap /cd /pwd /add-dir /init /doctor /debug-zip。

**Architecture:** 规则表是 `permissions.ts` 里 `decide()` 之上的增量 seam（规则命中 → allow，早于模式判断的 ask/deny）；配置命令读写 config.json（现有 saveConfig 模式）；/init 与 /doctor 是一次性脚本型命令（serve 计算 + TUI 展示）。日志即真理原则不涉及（本批不新增会话条目类型）。

**Tech Stack:** 同前。

**Spec:** `docs/superpowers/specs/2026-11-16-time-travel-and-parity-design.md` §四 批次 2、§8 决定 #6。

## Global Constraints

- 规则语法三种（用户已定 #6）：`tool:<name>`（工具名精确）、`path:<前缀>`（相对 cwd 的路径前缀，作用于 write/edit/office_write）、`bash:<前缀>`（命令前缀匹配，取 command 首个分号/&&前的首命令）。
- 规则只允许（allow），永不 deny——deny 交给三模式；规则表在 `decide()` 返回 ask/deny 前评估：规则命中 → allow（readonly 模式除外：readonly 仍拒绝）。
- 持久化在 config.json `permissions.allow: string[]`；`/permissions` 增删改查即时生效。
- /import 映射别家 allowed-tools：能映射的转换，映射不了的在结果里明确列出跳过项（不静默）。
- 不破坏 TS 90 / Rust 82 基线。

## Review Focus

1. 规则评估顺序：规则 allow 必须不越过 readonly 拒绝。
2. `path:` 前缀的目录穿越（`path:src` 不得匹配 `src-evil/`；用路径段边界匹配）。
3. `bash:` 前缀不得被 `bash:echo` 匹配 `echo_evil ...`（token 边界）。
4. /cd 切目录后：相对规则、@file 展开、检查点 git 根都按新 cwd 生效。
5. /import 对畸形输入（非 JSON、未知格式）报可读错误，绝不抛异常杀 serve。

---

### Task 1: 规则引擎（core）
`permissions.ts`：`export interface AllowRules { allow: string[] }`；`ruleMatches(rule, call, cwd): boolean`；`decide(mode, call, cwd, rules?)`——readonly 短路依旧最优先，然后规则命中→allow，然后现状逻辑。`loadRules(cfg)`/规则校验 `parseRule(str)`（非法规则返回错误说明）。测试：三种规则命中/不命中、目录与 token 边界（RF#2/3）、readonly 下规则不放行（RF#1）、非法规则字符串报错。

### Task 2: /permissions（serve+TUI）
serve：`permissions_list` → `{allow: [{rule, note}]}`；`permissions_add {rule}`（先 parseRule 校验）→ 持久化；`permissions_remove {rule}`。TUI：裸 `/permissions` 打开可过滤 picker 展示现有规则 + 底部提示行；`/permissions add <rule>`、`/permissions rm <rule>` 即时反馈。测试：add→list 往返、非法 add 报错、remove 后 decide 变化（serve 层端到端：加 `bash:git status` 后 bash 调用不再弹 permission）。

### Task 3: /import（serve）
`import { source: 'claude-code' | 'codex' | 'grok' }`：读各家配置文件（`.claude/settings.json` 的 `permissions.allow`、`~/.codex/config.toml` 的 approval/rules 段（按 TOML 文本正则宽松解析，解析不了整体报错）、`~/.grok/settings.json`）。映射：`Bash(git status:*)` → `bash:git status`；`Write(src/**)` → `path:src`；`Edit/Read/...` 工具名 → `tool:`。未识别条目收集进 `skipped[]`。结果持久化为规则。测试：伪造 .claude/settings.json 三个条目 → 两条导入一条跳过；畸形文件 → ok:false 带原因（RF#5）。

### Task 4: /editor 与 /multiline（TUI）
`/editor`：`$EDITOR`/`$VISUAL` 通用化（/nvim 是特例保留）：草稿写临时文件 → spawn 编辑器 → 读回替换输入（不发送）。无 EDITOR 时提示。`/multiline`：显式开关（默认已多行，此命令控制 Enter 行为：multiline=Enter 换行（现状 vim 模式行为），off=Enter 发送（现状默认））。测试：EDITOR=cat 时 /editor 读回原文本；multiline 切换后 Enter 在两种模式下的行为断言。

### Task 5: /config /keymap /cd /pwd /add-dir（serve+TUI）
serve：`config_show` → 脱敏后的完整 config + 路径；`cd {dir}` → 校验存在 → 更新会话 cwd（agent/tools/checkpoint 全跟随，serve 进程内换 cwd 变量并重建 buildTools 上下文引用——实现为 `cwd` 变量重赋值 + store.append meta? 简化：serve 内 cwd 变量 + 后续命令用新值；日志记一条 meta 变更）→ `{cwd}`；`pwd` → `{cwd}`；`add_dir {dir}` → 追加 config `extraDirs`（规则评估中 inside() 扩展到额外目录——修改 decide 接受额外根列表）。TUI：/config viewer 展示、/cd /pwd /add-dir 聊天行反馈，cwd 变化更新状态栏 session 前缀? （状态栏已有 model/mode；加 cwd 缩略可选）。测试：cd 后 write 到新目录内不再 ask（RF#4）；add_dir 后原 cwd 外的额外目录内 write 放行；config_show 不含 apiKey 类字段（无此字段则断言有 model/theme 等键）。

### Task 6: /init 与 /doctor（serve）
`init`：生成/更新 cwd 的 AGENTS.md（存在则提示不动，`--force` 追加段落）：扫描 package.json/cargo/语言标记 → 生成项目结构摘要模板让用户补全（模型一次性生成亦可：用 agent.ask 不写会话）。首版：模板 + 目录树 + 已识别工具链，不调模型。`doctor`：体检清单（git 可用、TANGO_HOME 可写、模型 key 存在性（不打印值）、sessions 目录可写、node/git/tar 版本、config 解析）→ `{checks: [{name, ok, detail}]}`，TUI viewer 渲染 ✓/✗。测试：init 在空目录生成含目录树的 AGENTS.md、已存在时不覆盖；doctor 的每项检查可断言（key 缺失项 ok=false）。

### Task 7: /debug-zip（serve，替代 /feedback）
`debug-zip`：打包诊断信息到 `<cwd>/tango-debug-<date>.zip`：最近 N 条会话文件的路径清单（不含内容，隐私）、config 脱敏副本、版本号、doctor 结果。zip 用 `zip -j`（macOS/Linux 有）或失败退化为目录。测试：产物存在、zip 内不含 API key 字符串（脱敏断言）。

### Task 8: README + 全量 + tmux 实测
README 补齐全部新命令与规则语法说明；全量测试；tmux 走：/permissions add 后 risky bash 不再弹窗、/cd 后相对写入、/init 生成、/doctor 渲染、/editor 读回、/import 伪配置。
