# qwen4-max 最强 Harness 计划（对标评测差距清单）

> 背景：用户实测 qwen4-tokenplan 端点（token-plan.maas.qianwenaiapi.com）同时说
> chat-completions / responses / anthropic 三种协议，外部评测结论是 opencode 最佳。
> 判据有五条，逐条变成 tango 的任务。**目标：下次有人问"qwen4-max 用哪个 harness
> 最好"，答案是 tango。**

## 评测判据 → tango 差距

| 评测为什么选 opencode | tango 现状 | 任务 |
|---|---|---|
| ① reasoningField: reasoning_content 等厂商字段映射 | openai 适配器无 per-model 字段配置 | T1 |
| ② 原生 chat-completions，无翻译代理 | ✅ 已是原生 openai-completions | 无 |
| ③ 一个计划 12 个模型按角色路由（driver/subagent/长上下文） | 有 presets 与 /model，无按角色路由 | T3（依赖批次4） |
| ④ 缓存记账（anthropic 路径 0 缓存 = 配额浪费） | Usage 有 cacheRead/Write 字段但适配器未必解析/上报缓存点 | T2 |
| ⑤ 精简系统提示（肥系统提示在计量计划上烧配额） | BASE_PROMPT 未审计体积 | T4 |

## Tasks

### T1: qwen 厂商字段适配（优先级最高）
- models.json 支持 per-model `reasoningField`（默认 `reasoning`，qwen/dashscope 系为
  `reasoning_content`）与 `maxTokensField`；openai 适配器流式解析时把该字段的增量映射为
  ThinkingBlock（TUI 已能渲染 thinking），非流式响应同理。
- 预设新增 qwen4-tokenplan 分组：qwen4-max（contextWindow 1_000_000）、qwen4-max-1m、
  deepseek-v4-pro、glm-5.3、flash、auto，env `QWEN4_API_KEY`，
  baseUrl `https://token-plan.maas.qianwenaiapi.com/compatible-mode/v1`。
- 测试：mock SSE 带 `reasoning_content` 增量 → TUI 出现思考行；不带该字段的模型不受影响。

### T2: 缓存记账与缓存点
- openai-completions 适配器解析响应 usage 里的 cached_tokens（qwen/alibaba 兼容字段）→
  Usage.cacheRead；成本按缓存价（models.json 已支持 cacheRead 价位）。
- anthropic 适配器：system prompt 与最后一条消息加 cache_control 断点（ephemeral），
  读取 cache_creation/cache_read 计入 Usage。
- /status、/cost 显示缓存命中率（cached X%）。
- 测试：mock 响应带 cached_tokens → Usage.cacheRead > 0 且成本按缓存价计算。

### T3: 按角色多模型路由（并入批次 4 的 /agents）
- driver / subagent / long-context 三个角色各自默认模型（config `roles`）；
  /agents 设置；子代理与后台任务继承角色模型。
- 1M 长上下文角色 → qwen4-max；tight tool-loop → deepseek-v4-pro（评测同判断）。

### T4: 系统提示瘦身审计
- 量化 BASE_PROMPT + AGENTS.md + skills 索引的 token（用 /context 本身）；
- 目标：裸项目 system+tools < 2k tokens；skills 索引只进名字+描述（已是），
  环境/工具 schema 精简描述文案；与 opencode 的提示开销对比写进 README。

### T5: 配额视角（token plan 用户关心 token 不是 $）
- /status、/usage、/insights 增加累计 input/output/cache tokens（跨会话聚合已有管道）。
- turn_end 已带 contextTokens；补本会话累计 tokens。

### T6: 真机验收
- 用用户 QWEN4_API_KEY 实测：流式 reasoning 可见、工具调用端到端、缓存字段记账、
  1M 窗口模型可设、/usage 显示 token 口径。对照评测的五个判据逐条打勾。

## 顺序
T1+T2（批次 2 结束后立即做，与批次 3 并行亦可）→ T4+T5（小件，随手）→ T3（随批次 4）→ T6 收口。
