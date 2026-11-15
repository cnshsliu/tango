# tknet.ai Tool Calling — 现状与验收记录

日期：2026-11-14 · 提交：`5d9ac16` + `a08e867` + `f206ec5` + `b85057d`（均已部署生产）

---

## 附 2：币种体系与价格组目录重构（2026-11-14/15，12 提交 `0ff064d`..`1dab09a`，已部署+迁移+验收）

**规则**：apikey.fan 层级价 = 成本；**售价 = 成本 × 1.1**（¥→$ 走后台维护的汇率）；
汇率缺失**拒绝服务**（无硬编码兜底，admin 在 /admin/exchange-rates 维护）。

- **组**：default（真实组=全目录+Plus 层覆盖价，未绑组 key 走它）、Balance（外接层 13 模型）、
  Jackson Hole（JackHo 7 个 anthropic:*）、native（清空，留官渠）
- **default 真实组**：NULL 组解析到 default 目录；目录缺行放行继承价（防全站 403）；
  禁用的 default 视为缺席
- **展示**：zh-CN/zh-TW 界面全 ¥，其余全 $（服务端 ?displayCurrency= 同源换算，汇率缺失按原币显示）
- **支付**：充值币种随界面语言（CNY 结账 ¥，ledger 按 DB 汇率记 USD；payments 记实收币种/金额，迁移 0062）
- **实例成本**：APIKEY.FUN 三实例重同步为站点层级价 ×CNY（含**实例级币种**——计费只读实例币种，
  评审 Critical 拦下的坑），JackHo 不变
- **验收**：未绑组 key 调 opus-4-7 计费精确 = ¥5.5/27.5/0.55(cached) ÷7.2（27 个缓存命中档也算对，
  µ$ 级吻合）；新模型 sonnet-5 路由 OK；中文界面 121¥/0$；平衡组/Jackson Hole 芯片与目录正常
- 计划：`docs/superpowers/plans/2026-11-14-currency-system-and-catalog.md`；独立评审 1C+5I+5M 全处置

---

---

## 附：折扣子系统退役（2026-11-14，提交 `22aada0` + `c685148`，已部署）

**决策**：废弃 `users.billing_rate` 与 `api_keys.multiplier`，计费只走列表价
（`model_pricing`）或所绑价格组的覆盖价。

- **生产数据**：唯一 billing_rate=0.80 用户（liukehong@gmail.com）已改回 1.00
  （备份 `scripts/billing-rate-backup-2026-11-14.json`，事务执行）。
- **代码范围**：31 文件，+219/−240。auth 中间件不再读 billingRate；chat/messages/
  responses/embeddings/batch 五个端点不再传递倍率、删除了"account rate 403"门禁；
  request/stream/batch worker 及 calculator 全部去掉 userMultiplier（billedAmount =
  列表/组价 × 用量）；profit-guard 删除 discountBlocksCaller/accountRateBlocksModel；
  admin users/api-keys、user keys/profile、dashboard estimator/keys 的编辑入口与 zod
  字段全部移除。
- **残留读取**：无。全仓只剩 schema 列定义（列保留不删，历史账单可读）与备份 JSON。
- **全字段成本地板（新保护）**：group_model_pricing 保存时，cached / cache-write
  5m / 1h 三个字段也要 ≥ 对应上游成本档（含 legacy 单层双侧兜底）；继承公共价不是
  绕过口；`PROFIT_GUARD_EXEMPT_MODELS` 中的模型跳过整个地板。测试 64 项覆盖。
- **部署后实测**（https://tknet.ai，1.0× key）：qwen3.8-flash-next 57 in / 42 out →
  billed $0.00001415 = 57×0.075/M + 42×0.235/M，**精确等于列表价 × 用量**。
- **删列已执行（2026-11-14，迁移 `drizzle/0061_drop_discount_columns.sql`，提交 `8a1f342`）**：
  1. 前置检查：`users.billing_rate` 全为 1.00；`api_keys.multiplier` 仅 1 行非 1.00
     （revoked 状态的 Test Key ×1.50，经批准按历史值直接继续）；无视图 / pg_depend /
     索引依赖。
  2. 备份：服务器 `~/backups/tknet-full-20261114-185602.sql.gz`（25M 全量 pg_dump，
     经 Docker 容器 `tknetai-postgres-1` 导出）；两列全量值（14 users / 25 keys）导出至
     `scripts/dropped-columns-backup-2026-11-14.json`。
  3. schema 定义删除 + 全仓引用清零（i18n 孤儿键一并清理）；typecheck 0/0 ·
     eslint 0 警告 · 1082 测试全过。
  4. 部署顺序：deploy.sh 自动应用待定迁移（先迁移后重启），5 个进程
     （2×cluster/worker/peer-gateway/collab-gateway）全部重启到新构建。
  5. 迁移后验证：两列确认不存在（information_schema 复查 []）；实测对话
     qwen3.8-flash-next 55 in / 31 out → billed $0.00001142 = 列表价 × 用量
     （$0.075/$0.235 × 1.0）；admin/users、admin/api-keys、dashboard/keys、
     dashboard/estimator 页面全部 200；pm2 日志重启以来零 `column does not exist`
     （日志中仅有的两条 `relation "users" does not exist` 为数月前历史错误）。
  6. **实弹补验（ego-browser，2026-11-14）**：经真实 UI 闭环——登录 admin →
     `/dashboard/keys` 点 "+ New Key" → 命名创建 → key 出现在列表（active，
     prefix sk-tknb2…）→ 行内删除成功；admin API 建的测试 key 也已清理，最终零残留。
     顺带发现并修复：admin keys 页表头遗留的 "Multiplier" 列（`3e09da3`，
     待下次部署）。注：创建 key 的 UI 在用户门户 /dashboard/keys，
     admin 的 /admin/api-keys 只做管理列表。

---

## 结论

`/api/v1/chat/completions` 已完整支持 OpenAI 兼容工具调用（function calling），tango 可用。
模型前缀直接用平台名：`tknet/qwen3.8-flash-next`、`tknet/deepseek-v4-pro`、`tknet/anthropic:claude-sonnet-5` 等。

## 根因（为什么之前不工作）

三个断点，tools 参数在链路上被逐个丢弃：

1. **入口验证**（`src/routes/api/v1/chat/completions/+server.ts`）：`CHAT_KNOWN_KEYS`
   识别了 `tools`/`tool_choice` 但构造 jobPayload 时没放进去 → 静默丢弃。
2. **非流式响应**（`src/lib/queue/request-worker.ts`）：响应只取
   `message.content`，`message.tool_calls` / `finish_reason: "tool_calls"` 被丢；适配器的
   Message 规范化也会剥掉它们 → 加了 `extractToolSurface(raw)` 从原始上游响应提取。
3. **流式格式化**（`src/lib/queue/stream-processor.ts`）：SSE formatter 只发
   `delta.content`，`delta.tool_calls` 分片没有输出。

另有两个规范缺口：

4. **Anthropic 翻译**（`src/lib/providers/anthropic.ts`）：tools → `input_schema`、
   tool_choice 映射（`required`→`any`、命名→`{type:'tool',name}`）、多轮回传
   （assistant `tool_calls` → `tool_use` 块、`role:'tool'` → user `tool_result` 块、
   连续 tool 消息合并为同一 user 轮次——Anthropic 要求角色交替）、响应反向映射
   （`tool_use` → `tool_calls`、`stop_reason:tool_use` → `finish_reason:"tool_calls"`）、
   流式（`content_block_start(tool_use)` → 工具身份分片、`input_json_delta` → 参数分片）。
5. **第二轮验证**（`src/lib/validation/schemas.ts`，`b85057d`）：OpenAI 规范 assistant
   发起 tool_calls 时 `content: null`，Zod union 之前拒绝 null → 第二轮整个请求
   400 `Invalid input`。已修（union 加 `z.null()`）。

**tango e2e 收口（2026-11-14）**：`npx tsx src/cli.ts -m tknet/qwen3.8-flash-next "列出这个目录的文件"`
→ agent 实际调用 bash 工具列出目录（含大小）并给出最终中文回答。全程无 profit-guard 违规。
同日追加 `tknet/deepseek-v4.1-flash` preset（`src/core/models/registry.ts`，1M 上下文，
cost 0.135/0.54/0.003）并 e2e 通过：多轮 bash 工具循环 + 结构化目录树回答，零违规。
**tool calling 阻塞项就此关闭（双模型 e2e 验证）。**

## 现在支持的完整形态

- 请求：`tools` / `tool_choice`（`auto` | `none` | `required` | `{type:'function',function:{name}}`）
  原样转发（Anthropic 走翻译）；`stream_options.include_usage` 透传。
- 非流式响应：`message.tool_calls[]`（id/type/function.name/function.arguments-JSON字符串）、
  `finish_reason: "tool_calls"`、`message.reasoning_content` 透传。
- 流式响应：`delta.tool_calls[]` 增量分片（arguments 逐段拼接）、
  末尾 `finish_reason: "tool_calls"` chunk + usage chunk（工具定义计入 prompt_tokens）。
- 多轮：回传 assistant `tool_calls` + `role:'tool', tool_call_id, content` 即可继续。

## 在线验收结果（生产环境，2026-11-14）

| 测试 | 模型 | 结果 |
|---|---|---|
| 非流式 tool_calls | qwen3.8-flash-next | ✅ bash `ls`，prompt 312（工具定义计费） |
| 非流式 tool_calls | deepseek-v4-pro | ✅ bash `ls -la`，prompt 278 |
| 非流式 tool_calls | anthropic:claude-sonnet-5 | ✅ bash `ls -la`，prompt 438 |
| 流式 delta.tool_calls + usage | qwen3.8-flash-next | ✅ 分片拼接完整，finish tool_calls |
| 流式（Anthropic 翻译链） | anthropic:claude-sonnet-5 | ✅ content_block_start/input_json_delta 翻译正确 |
| Round-trip 第二轮（回传工具结果） | deepseek-v4-pro | ✅ 正确消费 tool 结果并回答（finish stop） |
| Round-trip 第二轮（tool_result 翻译） | anthropic:claude-sonnet-5 | ✅ tool_use/tool_result 双向翻译正确 |

单元测试：`tests/tool-calling.test.ts` 10/10 通过。

## 已知问题（与 tool calling 无关）

### ~~deepseek-v4.1-flash 上游 404~~（已修复 2026-11-14）

jinan vLLM 上游把模型别名改成了 `deepseek-v4.1-flash-1017`（10/17 构建），我们还在发
旧名。修复（原值备份在 tknet 仓库 `scripts/jinan-backup-2026-11-14.json`）：
- `model_mappings.upstream_model` → `deepseek-v4.1-flash-1017`（对外名不变）
- `provider_instance_model_costs.upstream_model` → 同步改名
- `provider_instances.model_support`（jinan.gpu-01）追加新别名，保留旧名
- 清断路器状态；映射缓存 30s TTL 自动刷新

修复后经 https://tknet.ai 实测全过：普通对话非流式（"二"，prompt 38）/ 流式（113 tok）/
tool calling 第一轮（bash `ls -la`，finish tool_calls）/ 第二轮回传（正确列举 3 个文件，
finish stop）。

### qwen3.8-flash-next 定价调整（2026-11-14，用户定价）

- Baystone `provider_instance_model_costs`（upstream_model=qwen3.8-flash-next）改为
  OpenRouter 官方价 25%：input 0.0375 / output 0.1175 / cached 0.004 / cache_creation
  (5m/1h 同) 0.05。原值备份同上文件。
- `model_pricing` 卖价不动（0.075/0.235/0.008），`enabled` 已恢复 true。
- 实测（https://tknet.ai）：普通对话 ✅（含 reasoning_content 透传）；tool calling
  第一轮 ✅（bash `ls -la`）；第二轮回传 ✅（正确列举，finish stop）。

### ⚠️ 遗留风险：qwen3.8-27b 映射会再次触发自动禁用（已处置 2026-11-14）

**用户决定：移除 qwen3.8-flash-next → qwen3.8-27b（Baystone）映射。** 已执行：
原值追加备份（同上文件 `qwen_27b_mapping_before`），事务中
`UPDATE model_mappings SET enabled=false`（保留行便于回滚）；`provider_instance_model_costs`
的 27b 成本行与其他模型未动。映射缓存刷新后确认：qwen3.8-flash-next 仅剩
`qwen3.8-flash-next` 一条启用映射。profit-guard 违规计数已清（旧亏损全部来自已移除的
27b 通路）。

**复测（1.0× 费率 key，https://tknet.ai，5 次对话）**：5/5 `actual_model=qwen3.8-flash-next`、
status=success、无新增 profit-guard 违规（计数器为空，alerts 最新条目仍为映射移除前的
旧记录）。计费抽查：billed ≈ 估算成本 2 倍（卖价 0.075/0.235 vs 成本 0.0375/0.1175），
1.0× key 毛利健康。

### 0.4× 折扣 key 在新成本下是否仍亏损（结论）

新成本 = 卖价的 50% → **盈亏平衡点 = 费率 0.5**。0.4× key 计费 0.4×卖价 = 成本的
80% → 每请求仍亏成本约 20%，累计 5 次/小时同样会触发禁用（只是更慢）。0.5× 及以上
费率的 key 则盈利（1.0× key 毛利 = 成本的 100%，已实测验证）。
