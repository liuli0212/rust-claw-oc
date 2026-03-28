# Subagent Observability Execution Plan

## 1. 背景

当前异步 subagent 已经具备：

- 后台运行与结果回收
- 父 session 下一轮收到完成通知
- `claimed_paths` 冲突检测
- `consume` 语义与受控写权限

但在“可观测性”上仍然偏弱。当前主要问题不是功能不可用，而是：

1. subagent 运行过程对主 agent 几乎不可见
2. subagent 出错后缺少足够现场信息，难以定位失败阶段
3. 日志跨 `tokio::spawn` 后上下文容易丢失，链路不完整
4. 父 agent 收到通知后，只知道“有结果”，但很难判断“为什么失败”或“下一步该怎么做”

这会直接影响：

- 主 agent 的恢复能力
- 人类开发者的排障效率
- 后续扩展复杂编排时的稳定性

---

## 2. 目标

本计划的目标不是单纯“多打点日志”，而是把 subagent 从“黑盒后台任务”提升为“可检查、可诊断、可恢复”的运行单元。

具体目标：

1. 让主 agent 能读到比 `running/finished/failed` 更有用的运行快照
2. 让人类开发者能直接查看 subagent 的失败现场，而不是只能翻模块级日志
3. 让 tracing 能正确串起 `runtime -> llm client -> tools` 的链路
4. 不引入过重的主动唤醒或复杂调度语义

非目标：

1. 本期不做实时 UI 面板
2. 本期不做跨进程恢复
3. 本期不做完整远程调试协议
4. 本期不要求主 agent 被后台任务完成事件自动打断

---

## 3. 核心判断

### 3.1 可观测性不等于 tracing

tracing 很重要，但不是第一优先级。

如果只能二选一：

- `A. subagent 独立调试快照 / transcript`
- `B. 更完整的 tracing span`

优先做 A。

原因：

- tracing 更适合开发期和在线排查
- 调试快照和 transcript 同时能服务主 agent 与人类开发者
- 只有 tracing 时，很多问题仍然需要人工在大量日志中拼接现场

### 3.2 调试快照首先是给主 agent 用的

调试快照不是“给开发者看的额外面板”。

它首先服务主 agent 的运行时决策，例如：

- 要不要重试 subagent
- 要不要取消 job
- 是否是 timeout 而不是逻辑失败
- 是不是卡在某个工具
- 是否应该切换拆任务方式

人类开发者会直接受益，但设计上应先以“帮助主 agent 决策”为目标。

### 3.3 当前通知机制足够作为入口，但不足以承担完整调试职责

现在的 `SubagentNotificationExtension` 已经可以在父 session 下一轮注入提示，这很好。

但它的定位应是：

- “提醒父 agent 去看某个 job”

而不是：

- “承载全部调试信息”

真正的调试信息应来自 runtime 快照 / transcript / event log。

---

## 4. 总体方案

建议分为 3 个阶段推进。

### Phase A：最小调试可见性

目标：让主 agent 和人类都能看见 subagent 当前在做什么、失败在什么阶段。

交付物：

1. `SubagentRuntime` 增强调试快照
2. `get_subagent_result` / `list_subagent_jobs` 暴露调试字段
3. 通知中附带更明确的动作提示和失败摘要

### Phase B：独立运行记录

目标：让 subagent 自己拥有可追溯过程，而不是只靠 runtime 内存状态。

交付物：

1. subagent 独立 transcript
2. subagent 独立 event log / tool event 摘要
3. 父 job metadata 与 `sub_session_id` 强绑定

### Phase C：trace 链路补全

目标：把运行日志按 job/session 串起来，服务开发期与线上排查。

交付物：

1. `SubagentRuntime::spawn_job()` 的 span 传播
2. `GeminiClient::stream()` / `OpenAiCompatClient::stream()` 的 spawn span 继承
3. 关键 runtime/tool/llm 日志统一携带 `job_id` / `sub_session_id`

---

## 5. Phase A：调试快照设计

### 5.1 新增调试快照结构

建议新增：

```rust
pub struct SubagentDebugSnapshot {
    pub state_label: String,
    pub failure_stage: Option<String>,
    pub step_count: Option<usize>,
    pub last_model_text: Option<String>,
    pub last_thought_text: Option<String>,
    pub last_tool_name: Option<String>,
    pub last_tool_args_summary: Option<String>,
    pub last_tool_result_summary: Option<String>,
    pub last_error: Option<String>,
    pub updated_at_unix_ms: u64,
}
```

并将其挂到：

```rust
pub struct SubagentJobSnapshot {
    pub meta: SubagentJobMeta,
    pub state: SubagentJobState,
    pub consumed: bool,
    pub consumed_at_unix_ms: Option<u64>,
    pub debug: Option<SubagentDebugSnapshot>,
}
```

### 5.2 调试字段分层

应区分三类字段。

#### A. 主 agent 真正需要的字段

- `failure_stage`
- `last_tool_name`
- `last_error`
- `step_count`
- `updated_at_unix_ms`

#### B. 人类调试很有帮助的字段

- `last_model_text`
- `last_tool_args_summary`
- `last_tool_result_summary`

#### C. 不建议第一期加入的字段

- 完整 prompt
- 全量 tool args 原文
- 大段 raw streaming 内容
- 全量思维链文本

原则：

- 保留足够现场
- 避免把 runtime 快照做成第二份 transcript

### 5.3 failure_stage 规范

建议使用固定枚举值：

- `build_subagent_session`
- `llm_stream_start`
- `llm_stream_read`
- `tool_call`
- `tool_result_parse`
- `finish`
- `timeout`
- `cancelled`
- `unknown`

这样主 agent 才能稳定做策略判断，而不是解析自由文本。

### 5.4 更新时机

推荐在以下时机刷新 debug snapshot：

1. subagent 开始运行时
2. 每次模型输出文本时
3. 每次 tool call 开始时
4. 每次 tool result 返回时
5. 每次 error 时
6. 进入 terminal state 前最后一次落盘

---

## 6. Phase A：接口层建议

### 6.1 `get_subagent_result`

建议返回：

```json
{
  "job_id": "...",
  "status": "failed",
  "consumed": false,
  "state": { ... },
  "debug": {
    "failure_stage": "tool_call",
    "last_tool_name": "write_file",
    "last_error": "permission denied"
  }
}
```

### 6.2 `list_subagent_jobs`

建议返回轻量版调试摘要，不要只列状态：

- `job_id`
- `status`
- `goal`
- `updated_at_unix_ms`
- `failure_stage`
- `last_tool_name`
- `last_error`

这样主 agent 在编排多个 job 时，能先粗看，再决定要不要 `get_subagent_result`。

### 6.3 可选新增 `inspect_subagent_job`

如果现有接口开始变臃肿，可以后续增加：

- `inspect_subagent_job`

语义：

- `list` 给列表概览
- `get_subagent_result` 给结果回收
- `inspect_subagent_job` 给调试详情

本期不强制新增，但这是自然演进方向。

---

## 7. Phase B：独立 transcript / event log

### 7.1 为什么需要

调试快照只能回答“最后发生了什么”。

当需要回答：

- 中间做过哪些步骤
- 哪次 tool call 之后开始异常
- 模型前后输出如何变化

就需要 transcript 或 event log。

### 7.2 设计建议

每个 subagent 都已经有 `sub_session_id`，因此可以直接把它视为一个轻量 session。

建议：

1. 为 subagent 分配 transcript 路径
2. 为 subagent 记录 tool start / tool end / error 事件
3. 在 `SubagentJobMeta` 中明确暴露 `sub_session_id`

### 7.3 最小落地方式

不必一开始复制完整父 session 基础设施。

可以先做：

1. transcript
2. 若干简化事件

例如：

```rust
pub struct SubagentEventSummary {
    pub kind: String,
    pub tool_name: Option<String>,
    pub text: String,
    pub at_unix_ms: u64,
}
```

runtime 内只保留最近 N 条，磁盘上可保留完整 transcript。

---

## 8. Phase C：Tracing 传播

### 8.1 目标

让以下日志链路保持在同一个 span 下：

- `SubagentRuntime`
- `GeminiClient` / `OpenAiCompatClient`
- tools
- runtime notification / terminal state

### 8.2 关键改动

#### A. `subagent_runtime.rs`

在 `spawn_job()` 中创建 span：

```rust
let span = tracing::info_span!(
    "subagent_run",
    job_id = %job_id,
    parent_session_id = %parent_ctx.session_id,
    sub_session_id = %sub_session_id,
);
```

然后：

```rust
tokio::spawn(async move {
    ...
}.instrument(span));
```

#### B. `llm_client/gemini.rs`

对内部 `tokio::spawn` 使用：

```rust
tokio::spawn(async move { ... }.in_current_span());
```

#### C. `llm_client/openai_compat.rs`

同样使用：

```rust
tokio::spawn(async move { ... }.in_current_span());
```

### 8.3 为什么推荐 `in_current_span()`

原因：

1. 语义更清晰
2. 不需要显式提取当前 span 再手动传入
3. 更适合“当前上下文延续”的场景

### 8.4 span 字段建议

建议保持精简：

- `job_id`
- `parent_session_id`
- `sub_session_id`

不建议默认放：

- `goal`
- `input_summary`

原因：

- 会增加日志噪音
- 可能带来敏感文本泄露

---

## 9. 通知机制的定位

当前通知机制已经有价值，但其职责应明确：

### 应负责

- 告诉父 agent 哪些 job 有更新
- 提示下一步应调用什么接口
- 提供简短失败摘要

### 不应负责

- 承载完整调试现场
- 替代结果回收
- 替代 transcript / event log
- 主动打断当前执行中的父 agent

所以后续通知建议保持：

- 轻
- 明确
- 可执行

而复杂现场应交给 runtime snapshot / transcript。

---

## 10. 代码落点建议

### 必改

- [src/subagent_runtime.rs](/Users/liuli/src/rust-claw-oc/src/subagent_runtime.rs)
  - 扩展 debug snapshot
  - 增加更新入口
  - 挂接 span

- [src/tools/subagent_async.rs](/Users/liuli/src/rust-claw-oc/src/tools/subagent_async.rs)
  - 暴露 debug 字段

- [src/subagent_notification.rs](/Users/liuli/src/rust-claw-oc/src/subagent_notification.rs)
  - 保持动作导向提示

- [src/llm_client/gemini.rs](/Users/liuli/src/rust-claw-oc/src/llm_client/gemini.rs)
  - 内部 spawn 继承当前 span

- [src/llm_client/openai_compat.rs](/Users/liuli/src/rust-claw-oc/src/llm_client/openai_compat.rs)
  - 内部 spawn 继承当前 span

### 第二阶段改动

- `session/factory.rs`
  - 为 subagent 提供 transcript / event sink

- `context` / `event_log` 相关模块
  - 视复用情况接入 subagent 独立记录

---

## 11. 实施顺序

推荐顺序：

1. **先做 Phase A**
   - debug snapshot
   - `get_subagent_result` / `list_subagent_jobs` 可见性增强

2. **再做 Phase C**
   - tracing span 传播

3. **最后做 Phase B**
   - 独立 transcript / event log

理由：

- Phase A 立刻提升主 agent 的恢复能力
- Phase C 能改善开发调试体验，但不直接提升主 agent 决策
- Phase B 最有价值，但也最容易扩散改动面

---

## 12. 验证计划

### 单元测试

1. `SubagentRuntime` debug snapshot 会在 tool call / error / timeout 后更新
2. `get_subagent_result` 返回 debug 字段
3. `list_subagent_jobs` 返回轻量调试摘要
4. tracing 相关逻辑至少不破坏现有流式行为

### 集成测试

1. subagent 失败后，父 agent 能通过 job snapshot 看见 `failure_stage`
2. subagent 超时后，父 agent 能看见 `last_tool_name` 或最后更新位置
3. 父 agent 收到通知后，可基于 notice 调用 `get_subagent_result`

### 手动验证

1. 运行一个成功的 subagent，检查 snapshot 是否包含最近工具和结果摘要
2. 运行一个失败的 subagent，检查 `failure_stage` 与 `last_error`
3. 开启 `RUST_LOG=debug`，确认 span 下日志可按 `job_id` 串联

---

## 13. 最终建议

如果目标是“让 debug subagent 更方便”，最合理的执行顺序不是“先补 tracing”，而是：

1. **把 subagent 变成可检查对象**
2. **再把日志链路串起来**
3. **最后补长期留痕能力**

一句话总结：

> 先解决“看不见发生了什么”，再解决“日志没有上下文”，最后解决“如何长期追溯全过程”。

