# tknet.ai Tool Calling — 现状与验收记录

日期：2026-11-14 · 提交：`5d9ac16` + `a08e867` + `f206ec5` + `b85057d`（均已部署生产）

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
**tool calling 阻塞项就此关闭。**

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
