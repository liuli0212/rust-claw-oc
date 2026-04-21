# Rusty-Claw Trace Center 设计方案

> 状态: 待实施  
> 目标级别: 日常可用版（非 MVP）  
> 最后更新: 2026-04-03

---

## 1. 背景

当前 `rusty-claw-oc` 已经具备零散的可观测性基础：

- `AgentLoop::step()` 已天然形成 `turn -> llm -> tool round -> reconcile` 的执行骨架
- `AgentContext` 已维护 `turn_id`
- `TaskStateSnapshot` 已维护 `task_id`
- subagent 已有 `events.jsonl`、`SubagentDebugSnapshot` 与 `/trace <job_id>` 文本回放
- `ACP` 已有 SSE 通道和网页面板

但这些能力仍不足以支撑“日常使用”：

1. 主 Agent 与 subagent 的事件模型不统一
2. 事件多数仍停留在“文本日志”层，而非可检索的结构化 trace
3. `telemetry.rs` 只有粗粒度 span，无法解释一次任务为什么卡住、为什么重试、为什么走错工具路径
4. ACP 仅展示文本流，不展示调用树、时间线、上下文压缩和产物链路
5. 历史回放缺少索引、筛选、搜索和对比能力

因此，本方案的目标不是“再加一点 log”，而是构建一套可长期常开、可检索、可回放、可实时观察的本地 Trace Center。

---

## 2. 目标

### 2.1 本期必须达成

1. 支持主 Agent、subagent、skill、tool、LLM、context 管理的统一 tracing
2. 支持任务执行中的实时观察（live trace）
3. 支持历史 run 回放（replay）
4. 支持按 `session / task / turn / tool / status` 检索
5. 支持查看调用树、时间线、上下文变化和产物
6. 默认可常开，性能成本可控

### 2.2 非目标

1. 本期不做分布式 tracing
2. 本期不强依赖 Jaeger / Tempo / Grafana 等外部平台
3. 本期不把完整 prompt / 完整工具输出默认永久存储
4. 本期不做跨进程恢复未完成 run
5. 本期不做多租户或远程 SaaS 观测平台

---

## 3. 日常可用版的设计原则

### P1. 事件优先，不以字符串日志为中心

“能看见”不是指打印更多日志，而是指系统产生统一结构化事件，再由 UI 和索引层决定如何展示。

### P2. TraceBus 是唯一入口

主 Agent、subagent、skill、tool、context 压缩、task state 更新都通过同一个总线发事件，避免继续把事件写入逻辑散落在 `AgentOutput`、`tracing::info!` 和 shell log 中。

### P3. 双存储

- `JSONL` 是原始事实流，适合回放、导出和排错
- `SQLite` 是索引和查询层，适合日常筛选、聚合和 compare

### P4. 默认摘要化

日常模式默认只存摘要、统计和 preview，不存全量 prompt / result。只有显式 debug tracing 才提升详细度。

### P5. 语义优先于标准

先定义符合 Agent 语义的事件模型，再考虑导出到 OpenTelemetry。不要反过来让 OTel 的 span 模型限制产品设计。

---

## 4. 当前代码中的可复用基础

### 4.1 标识与执行骨架

- `AgentLoop::step()` 已构成主循环骨架
- `AgentContext.current_turn.turn_id` 已存在
- `TaskStateSnapshot.task_id` 已存在

### 4.2 已有事件能力

- `EventLog` 已能写入/读取 `events.jsonl`
- subagent 的 `CollectorOutput` 已会记录：
  - `llm_request`
  - `llm_response`
  - `subagent_tool_start`
  - `subagent_tool_end`
  - `subagent_error`

### 4.3 已有实时通道

- ACP 已有基于 SSE 的 `/run`
- `SubagentRuntime` 已有通知与 snapshot 能力

### 4.4 已有上下文相关信号

- `take_snapshot()` / `diff_snapshot()`
- `rule_based_compact()`
- `compress_current_turn()`
- `truncate_current_turn_tool_results()`

这些都说明当前仓库并不缺“可视化的原料”，缺的是统一的事件协议、索引层和界面。

### 4.5 当前必须先修的缺口

以下问题不是“未来优化”，而是 trace 能否可信落地的前置修复：

1. `turn_id` 虽已存在于 `AgentContext.current_turn`，但当前写入 `EventLog` 时多数路径仍传 `None`
2. skill tracing 已在 `tools/skills.rs` 中产生多类 `skill_call_*` 事件，但尚未纳入统一模型
3. ACP 当前丢弃 `on_tool_start()` / `on_tool_end()`，导致网页端看不到工具执行状态
4. `CorrelationIds` 目前没有 `run_id`，无法稳定表达“一次执行实例”
5. subagent 虽有 `parent_session_id`，但尚未形成贯穿 parent-child 的统一 trace 关联

---

## 5. 总体架构

```text
┌───────────────────────────────────────────────────────┐
│                Trace Producers                        │
│ AgentLoop / Tool / Skill / Subagent / Context / ACP  │
└───────────────────────┬───────────────────────────────┘
                        │
                ┌───────▼────────┐
                │    TraceBus     │
                │ start_span      │
                │ end_span        │
                │ record_event    │
                └───────┬────────┘
                        │
          ┌─────────────┴─────────────┐
          │                           │
  ┌───────▼────────┐         ┌────────▼────────┐
  │ JSONL Sink      │         │ SQLite Index    │
  │ raw event log   │         │ query + compare │
  └───────┬────────┘         └────────┬────────┘
          │                           │
          └─────────────┬─────────────┘
                        │
               ┌────────▼────────┐
               │ ACP Trace API    │
               │ live + replay    │
               └────────┬────────┘
                        │
               ┌────────▼────────┐
               │ Trace Center UI  │
               │ timeline/tree/etc│
               └──────────────────┘
```

---

## 6. 核心对象模型

### 6.1 Run / Trace / Span / Event

本方案将一次用户任务拆成四层：

1. `Run`
   - 一次面向用户请求的执行实例
   - 一个 session 内可有多个 run
2. `Trace`
   - 与 run 一一对应
   - 用 `trace_id` 贯穿主 Agent 与所有 subagent / skill
3. `Span`
   - 有开始和结束，带耗时
   - 适合表示：run、turn、iteration、llm 请求、tool 执行、subagent 生命周期
4. `Event`
   - 离散瞬时事件
   - 适合表示：plan 更新、上下文压缩、工具结果截断、重试、yield、finish summary

### 6.2 推荐主键与标识

```rust
trace_id
run_id
span_id
parent_span_id
session_id
task_id
turn_id
sub_session_id
job_id
skill_call_id
iteration
```

### 6.3 为什么必须引入 `run_id`

当前 `session_id` 与 `task_id` 不足以支撑日常使用：

- 同一 session 可承载多个独立用户请求
- task state 可能在恢复、清理或复用时变化
- 对比两个执行实例时，需要稳定的单次 run 标识

因此：

- `session_id` 表示会话容器
- `task_id` 表示任务状态实体
- `run_id` 表示本次执行实例
- `trace_id` 表示贯穿主链路和子链路的 tracing 根

通常第一期可以令 `trace_id == run_id`，保留后续分离空间。

### 6.4 v1 核心字段与扩展字段

第一期不追求把所有关系字段都提升为顶层列。推荐做法：

- 顶层保留稳定索引字段：
  - `trace_id`
  - `run_id`
  - `session_id`
  - `task_id`
  - `turn_id`
  - `iteration`
  - `span_id`
  - `parent_span_id`
- `job_id` / `sub_session_id` / `skill_call_id` / `tool_name` / `provider` / `model` 等上下文信息先放入 `attrs`

这样能降低 schema 复杂度，同时保留后续把高频查询字段上提的空间。

### 6.5 Parent-Child Trace 关联

主 Agent 与 subagent 必须共享同一个 `trace_id`。

- parent run 创建根 `trace_id`
- subagent 启动时继承该 `trace_id`
- subagent 自己拥有新的 span，`parent_span_id` 指向触发它的 tool / skill span
- `job_id` 与 `sub_session_id` 都保留在 `attrs`

注意：当前异步 subagent 的 `job_id` 与 `sub_session_id` 虽然在实现上相同，但语义不同：

- `job_id` 是运行时任务身份
- `sub_session_id` 是子会话身份

因此不建议在设计层面把它们视为永久同义。

---

## 7. 统一事件协议

### 7.1 事件基类

```rust
pub struct TraceRecord {
    pub schema_version: u32,
    pub record_id: String,
    pub trace_id: String,
    pub run_id: String,
    pub span_id: Option<String>,
    pub parent_span_id: Option<String>,
    pub session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub iteration: Option<u32>,
    pub actor: TraceActor,
    pub kind: TraceKind,
    pub name: String,
    pub status: TraceStatus,
    pub ts_unix_ms: u64,
    pub duration_ms: Option<u64>,
    pub level: TraceLevel,
    pub summary: Option<String>,
    pub attrs: serde_json::Value,
}
```

### 7.2 枚举建议

```rust
enum TraceActor {
    User,
    MainAgent,
    Subagent,
    Skill,
    Llm,
    Tool,
    Context,
    Scheduler,
    System,
}

enum TraceKind {
    SpanStart,
    SpanEnd,
    Event,
}

enum TraceStatus {
    Ok,
    Error,
    Cancelled,
    TimedOut,
    Yielded,
    Retrying,
    Skipped,
    Running,
}

enum TraceLevel {
    Normal,
    Debug,
}
```

### 7.3 必须标准化的事件名

#### Run / Turn / Iteration

- `run_started`
- `run_finished`
- `run_failed`
- `run_cancelled`
- `turn_started`
- `turn_finished`
- `iteration_started`
- `iteration_finished`

#### LLM

- `llm_request_started`
- `llm_request_finished`
- `llm_stream_chunk`
- `llm_tool_call_emitted`
- `llm_retry_scheduled`
- `llm_error`

#### Tool

- `tool_started`
- `tool_finished`
- `tool_failed`
- `tool_timed_out`
- `tool_cancelled`

#### Skill / Subagent

- `skill_call_requested`
- `skill_call_started`
- `skill_call_subagent`
- `skill_call_finished`
- `skill_call_denied`
- `subagent_spawned`
- `subagent_state_changed`
- `subagent_finished`
- `subagent_notification_enqueued`

#### Context / State

- `plan_updated`
- `task_state_changed`
- `context_snapshot_taken`
- `context_compacted`
- `tool_result_truncated`
- `memory_sources_changed`

#### Control Flow

- `yielded_to_user`
- `finish_committed`
- `energy_depleted`
- `autopilot_stalled`

### 7.4 为什么时间必须毫秒级

现有 `EventLog` 和 `TelemetryExporter` 使用秒级时间戳，不足以支撑：

1. waterfall timeline
2. tool 级耗时排序
3. 并发 subagent 的先后关系判断
4. 高频重试的定位

因此本方案统一使用 `unix_ms_now()`。

这不是纯粹的“精度优化”，而是保证 timeline 正确性的前置条件。若继续混用秒级时间戳，则：

- 同一秒内的多个 tool span 无法稳定排序
- waterfall 会出现伪并发与伪串行
- subagent 与 parent event 的先后关系会被抹平

---

## 8. 埋点方案

### 8.1 `AgentLoop::step()`

负责以下 span / event：

- `run_started`
- `turn_started`
- `iteration_started`
- `iteration_finished`
- `yielded_to_user`
- `run_finished`
- `run_failed`
- `energy_depleted`

### 8.2 `collect_stream_response()`

负责：

- `llm_request_started`
- `llm_request_finished`
- `llm_tool_call_emitted`
- `llm_retry_scheduled`
- `llm_error`

其中 `llm_tool_call_emitted` 应在收到 `StreamEvent::ToolCall` 的当下直接发出，而不是等到本轮流式响应结束后再批量补记。这样 live trace 才能真实反映“模型刚刚决定调哪个工具”。

`attrs` 应至少包含：

- `provider`
- `model`
- `message_count`
- `tool_count`
- `approx_prompt_chars`
- `approx_prompt_tokens`
- `stream_attempt`

### 8.3 `dispatch_tool_call()`

负责：

- `tool_started`
- `tool_finished`
- `tool_failed`
- `tool_timed_out`
- `tool_cancelled`

`attrs` 应包含：

- `tool_name`
- `args_preview`
- `remaining_steps`
- `timeout_sec`
- `result_preview`
- `result_size_chars`

### 8.4 `reconcile_after_tool_calls()`

负责：

- `plan_updated`
- `context_compacted`
- `tool_result_truncated`

这是日常排障非常重要的一层，因为很多“模型突然发散”的根因不是工具错了，而是上下文在这里发生了结构性变化。

当前代码已经提供了足够的返回值用于第一期埋点：

- `rule_based_compact()` 返回 `Option<String>`
- `compress_current_turn()` 返回压缩条目数
- `truncate_current_turn_tool_results()` 返回截断条目数

因此第一期无需为它们额外设计复杂事件对象，只需在调用点把这些返回值转成 trace event 即可。

### 8.5 `TaskStateStore::save()`

建议将 task state 的变化统一发出 `task_state_changed`，而不是只由 UI 通过 `on_plan_update()` 被动感知。这样可以支持：

- 历史回放中的 plan 变化
- compare runs 时比较计划演化路径
- 按异常状态筛选 run

### 8.6 `SubagentRuntime`

负责：

- `subagent_spawned`
- `subagent_state_changed`
- `subagent_finished`
- `subagent_notification_enqueued`

同时把 `job_id / sub_session_id / parent_session_id / allow_writes / claimed_paths` 放入统一 attrs。

---

## 9. TraceBus 设计

### 9.1 职责

`TraceBus` 是 tracing 唯一写入口，负责：

1. 生成 record / span id
2. 统一补全上下文字段
3. 异步广播到多个 sink
4. 控制写入等级与采样策略
5. 维护 live subscribers

### 9.2 推荐接口

```rust
pub struct TraceContext {
    pub trace_id: String,
    pub run_id: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub parent_span_id: Option<String>,
    pub iteration: Option<u32>,
}

pub trait TraceSink: Send + Sync {
    fn publish(&self, record: TraceRecord);
}

pub struct TraceBus { ... }

impl TraceBus {
    pub fn start_span(&self, ctx: &TraceContext, actor: TraceActor, name: &str, attrs: Value) -> TraceSpanHandle;
    pub fn record_event(&self, ctx: &TraceContext, actor: TraceActor, name: &str, status: TraceStatus, summary: Option<String>, attrs: Value);
}
```

### 9.2.1 输出包装策略

推荐引入 `TracingOutputWrapper`：

```rust
pub struct TracingOutputWrapper {
    inner: Arc<dyn AgentOutput>,
    bus: Arc<TraceBus>,
    trace_ctx: Arc<dyn TraceContextProvider>,
}
```

职责分工：

- 对现有 `AgentOutput` 实现零侵入复用
- 在 `on_text` / `on_thinking` / `on_tool_start` / `on_tool_end` / `on_llm_request` 等 hook 上自动补发由“输出语义”可推导的 trace event
- 对 run 生命周期、iteration、context compaction、task state diff 这类非输出事件，仍由 `AgentLoop` / `SubagentRuntime` / `TaskStateStore` 直接调用 `TraceBus`

这样可以显著降低 CLI / ACP / CollectorOutput 的改造量，同时避免把 TraceBus 误塞进 `AgentContext`。

设计约束：

- `AcpOutput` / CLI output / Telegram output 继续只负责渲染，不直接依赖 `TraceBus`
- wrapper 负责在调用 `inner.on_tool_start()` 之前先发出 `tool_started` 等 trace 事件
- 这样可保证多端输出行为一致，也避免把 tracing 逻辑散落到每个 output 实现中

### 9.2.2 Trace Context 传播策略

主方案仍采用显式上下文：

- `AgentLoop` 持有当前 run 的 `TraceContext`
- `SubagentRuntime` 在 spawn 时显式继承 `trace_id`
- wrapper / runtime / core helper 通过 `self` 访问 `TraceBus`

可选优化：

- 对特别底层、参数穿透成本高的辅助函数，可以增加 `tokio::task_local!` 作为只读镜像
- 但 `task_local!` 不应成为唯一真相来源

原因：

1. 本仓库大量使用 `tokio::spawn`，task-local 不会自动跨 task 传播
2. subagent / scheduler / background runtime 都需要显式跨边界继承 trace
3. 纯 task-local 容易把关键依赖隐藏起来，增加测试与排障难度

因此推荐顺序是：

- 先建立显式 `TraceContext`
- 如有必要，再用 task-local 降低局部埋点样板代码

### 9.3 为什么不直接复用 `AgentOutput`

`AgentOutput` 是“面向用户输出”的接口，不适合作为 tracing 总线的唯一依托：

1. 它偏 UI 渲染语义，不是结构化 trace 语义
2. 主 Agent 和 subagent 事件精度不一致
3. context compaction / state diff 等事件并不天然属于 output
4. 未来 ACP、CLI、Telegram 可能有不同展示方式，但 tracing 应保持统一

因此建议：

- `AgentOutput` 保留为用户交互接口
- `TraceBus` 独立为系统观测接口
- `TracingOutputWrapper` 负责桥接 output hook 与 TraceBus
- TraceBus 不能反向依赖 output

---

## 10. 存储设计

### 10.1 原始事件流：JSONL

路径建议：

```text
rusty_claw/sessions/<session_id>/traces/<run_id>.jsonl
```

保留原则：

- 作为事实源
- 便于导出、grep、压缩归档
- 便于实现 replay

实现建议：

- 不再沿用“每次 append 都持有 `Mutex<File>` 并立即 flush”的写法作为最终方案
- JSONL sink 使用专用 writer task
- 业务线程只负责把 `TraceRecord` 发送到 channel
- writer task 负责批量落盘、缓冲和周期性 flush

建议结构：

```rust
TraceBus
  -> mpsc::Sender<TraceRecord>
  -> JsonlWriterTask
  -> BufWriter<File>
```

这样可以避免高频 trace 写入时：

- 热路径争用 `Mutex`
- 每事件 flush 带来的 syscalls 放大
- LLM stream 与 tool 回调互相阻塞

channel 策略建议优先使用有界队列而非无限队列：

- `normal` 级事件可在队列满时做降级或聚合
- `error` / `run_finished` / `subagent_finished` 等关键事件必须保证落盘

是否使用 `unbounded_channel` 可作为实现细节再评估，但设计层面更倾向“专用 writer + 批量写入”，而不是坚持某一种 channel 类型。

### 10.2 索引层：SQLite

建议新增：

```text
rusty_claw/traces/index.sqlite
```

#### 表 1: `runs`

```sql
CREATE TABLE runs (
  run_id TEXT PRIMARY KEY,
  trace_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  task_id TEXT,
  root_goal TEXT,
  status TEXT NOT NULL,
  started_at_unix_ms INTEGER NOT NULL,
  finished_at_unix_ms INTEGER,
  duration_ms INTEGER,
  provider TEXT,
  model TEXT,
  total_events INTEGER DEFAULT 0,
  total_spans INTEGER DEFAULT 0,
  total_tool_calls INTEGER DEFAULT 0,
  total_llm_calls INTEGER DEFAULT 0,
  total_subagents INTEGER DEFAULT 0,
  peak_prompt_tokens INTEGER,
  peak_history_tokens INTEGER,
  last_error_summary TEXT
);
```

#### 表 2: `trace_records`

```sql
CREATE TABLE trace_records (
  record_id TEXT PRIMARY KEY,
  trace_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  span_id TEXT,
  parent_span_id TEXT,
  session_id TEXT NOT NULL,
  task_id TEXT,
  turn_id TEXT,
  actor TEXT NOT NULL,
  kind TEXT NOT NULL,
  name TEXT NOT NULL,
  status TEXT NOT NULL,
  iteration INTEGER,
  ts_unix_ms INTEGER NOT NULL,
  duration_ms INTEGER,
  level TEXT NOT NULL,
  summary TEXT,
  attrs_json TEXT NOT NULL
);
```

#### 索引建议

```sql
CREATE INDEX idx_records_run_ts ON trace_records(run_id, ts_unix_ms);
CREATE INDEX idx_records_session_ts ON trace_records(session_id, ts_unix_ms);
CREATE INDEX idx_records_name_ts ON trace_records(name, ts_unix_ms);
CREATE INDEX idx_records_status ON trace_records(status);
CREATE INDEX idx_records_turn ON trace_records(turn_id);
```

### 10.3 为什么要双存储

只用 JSONL 的问题：

- 查询慢
- 难做 compare
- 难筛选失败模式
- 难聚合“最慢工具/最高上下文峰值”

只用 SQLite 的问题：

- 不利于回放导出
- 调试事实链不够直观
- 不利于保留原始事件顺序

双存储是最稳的工程折中。

但在实施顺序上，不要求 SQLite 与 TraceBus 同时落地：

- S1-S3 可以先以 JSONL + live 内存广播为主
- 当 run 列表、筛选和回放需求稳定后，再补 SQLite 索引层

---

## 11. 采样与详细度策略

### 11.1 默认模式：`normal`

存：

- prompt summary
- tool args/result preview
- token / char 统计
- context diff
- file artifacts
- error summary

不存：

- 全量 system prompt
- 全量 message payload
- 巨大的工具输出正文

### 11.2 调试模式：`debug`

额外存：

- 完整 prompt payload（可裁剪）
- 完整 tool result（有上限）
- 完整 thought 文本 preview
- 更细粒度 stream chunk

### 11.3 保留策略

- 最近 7 天保留全部 run
- 更早数据按 run 状态降级：
  - `error / timed_out / cancelled` 长保留
  - `ok` run 可只保留索引 + 原始 JSONL 压缩包

---

## 12. ACP Trace API

### 12.1 新增路由

#### `GET /trace/runs`

返回 run 列表，支持：

- `session_id`
- `status`
- `tool_name`
- `from`
- `to`
- `query`

#### `GET /trace/run/:run_id`

返回 run 概览：

- 基本信息
- 统计
- 子任务数量
- 错误摘要

#### `GET /trace/run/:run_id/records`

返回按时间排序的 trace records，支持：

- `actor`
- `name`
- `status`
- `turn_id`
- `iteration`

#### `GET /trace/run/:run_id/tree`

返回 span 树，供调用树 UI 直接使用。

#### `GET /trace/run/:run_id/artifacts`

返回：

- 写入文件
- patch 文件
- evidence
- subagent artifacts

### 12.2 Live SSE

#### `GET /trace/live/:session_id`

SSE 推送：

- 新 run 开始
- 新 trace record 到达
- run 状态更新

如果该 session 派生出 subagent，则属于同一 `trace_id` 的子事件也应被合并推送到 parent session 的 live feed。

日常使用时，Trace Center 首页应默认订阅当前活跃 session 的 live feed。

---

## 13. UI 设计

### 13.1 首页：Run Explorer

展示 run 列表：

- 状态
- 用户目标
- 耗时
- 最近错误
- 模型 / provider
- 工具调用数
- 子代理数
- 上下文峰值

支持：

- 搜索目标关键词
- 过滤状态
- 过滤包含某工具的 run
- 过滤慢 run

### 13.2 Run 详情：四栏视图

#### 视图 A: Timeline

按泳道展示：

- User
- Main Agent
- LLM
- Tool
- Subagent
- Context

每个 span 显示：

- 名称
- 状态
- 耗时
- 摘要

点击展开后显示 attrs。

#### 视图 B: Call Tree

树形展示：

- run
  - turn
    - iteration
      - llm request
      - tool
      - subagent

适合看父子关系和“为什么走成这样”。

#### 视图 C: Context

展示：

- prompt tokens 趋势
- history tokens 趋势
- compact / truncate 事件
- memory source 变化
- plan 变化

这是本系统和普通 tracing 最大的区别之一，必须作为一级视图存在。

#### 视图 D: Artifacts

展示：

- 新建/修改文件
- evidence
- subagent artifacts
- 相关 transcript / event 路径

### 13.3 后续可扩展视图

首批日常可用版完成后，可以继续增加：

- Compare 视图
- Slow path 视图
- Error clustering 视图

---

## 14. 与现有模块的映射

### 14.1 新增模块

- `src/trace/mod.rs`
- `src/trace/model.rs`
- `src/trace/bus.rs`
- `src/trace/sinks/jsonl.rs`
- `src/trace/sinks/sqlite.rs`
- `src/trace/query.rs`

### 14.2 重点改造模块

- `src/core.rs`
- `src/core/step_helpers.rs`
- `src/session/factory.rs`
- `src/subagent_runtime.rs`
- `src/task_state.rs`
- `src/schema.rs`
- `src/context/turns.rs`
- `src/acp/mod.rs`
- `src/acp/handlers.rs`
- `src/acp/output.rs`
- `src/acp/index.html`

### 14.3 兼容策略

- `src/event_log.rs` 第一阶段保留，作为过渡层或 JSONL sink 的兼容实现
- `src/telemetry.rs` 调整为一个可选 sink，而不是唯一 telemetry 接口
- `/trace <job_id>` 命令保留，但内部改为查询统一 trace 数据源

关于 TraceBus 的注入位置，推荐：

- 不放进 `AgentContext`
- 也不要求一开始就在所有调用点层层传参
- 优先通过 `AgentLoop` 持有 TraceBus，并在 session factory 中统一装配 `TracingOutputWrapper`

这样能减少上下文模块与运行时基础设施的耦合。

---

## 15. 实施切片

### S1: 事件模型 + TraceBus 内核

交付物：

- `TraceRecord`
- `TraceContext`
- `TraceBus`
- JSONL sink
- 内存 live broadcaster

验证标准：

- 能手动注入事件并从 JSONL 读回
- 单元测试验证 span start/end 配对和毫秒时间戳

### S2: 主 Agent 埋点

交付物：

- `run / turn / iteration / llm / tool / context` 事件链
- `run_id` 注入 `CorrelationIds`
- `turn_id` 正确写入事件

验证标准：

- 一次普通 `cargo run` 会生成完整主链路事件

### S3: Subagent / Skill 埋点与关联

交付物：

- `CollectorOutput` 改造为通过 `TracingOutputWrapper` + TraceBus 发事件
- `skill_call_*` 事件纳入统一模型
- subagent 继承 parent `trace_id`

验证标准：

- 一次包含 subagent / skill 的 run 会生成跨 session 的 trace 树

### S4: ACP Trace API + Live SSE

交付物：

- `/trace/runs`
- `/trace/run/:id/records`
- `/trace/live/:session_id`
- ACP tool 事件不再丢弃

验证标准：

- 浏览器可看到 run 列表与实时事件流

### S5: Timeline UI

交付物：

- Run Explorer
- Timeline 视图
- Call Tree / Context / Artifacts 详情面板

验证标准：

- 能按泳道查看一次 run 的完整 waterfall

---

## 16. 未来扩展

以下能力有价值，但不属于首批日常可用版的阻塞项：

1. Compare runs
2. 错误聚类
3. 最慢路径分析
4. OTel 导出

---

## 17. 性能与风险

### 16.1 性能风险

1. 每事件同步 flush 会拖慢运行
2. 默认存完整 payload 会导致磁盘膨胀
3. 高频 chunk 事件会使 UI 卡顿

### 16.2 规避策略

1. sink 内部做批量写入
2. 默认仅存 preview 和统计
3. `llm_stream_chunk` 只在 debug 模式开启
4. UI 默认对长事件列表做虚拟滚动

### 16.3 语义风险

如果没有统一 span 树，比较和时间线会很难稳定。因此应先定义父子关系，再补 UI，不能反过来“先画页面再凑数据”。

---

## 18. 测试策略

### 17.1 单元测试

- span start/end 配对
- run / turn / iteration id 继承
- JSONL sink 正确落盘
- SQLite sink 正确索引
- compare 结果稳定

### 17.2 集成测试

- 一次普通 tool run 产生完整 trace
- 一次多轮 tool run 产生多个 iteration
- 一次 subagent run 能串起 parent-child trace
- context compaction / truncation 会产生事件
- task_state 变化能被 replay

### 17.3 UI 测试

- run 列表加载
- live SSE 更新
- timeline 详情展开
- compare 结果展示

---

## 19. 推荐落地顺序

如果资源有限，优先级如下：

1. 统一 `TraceBus`
2. 主 Agent + subagent 事件统一
3. `JSONL + SQLite` 双存储
4. run 列表 + timeline UI
5. compare runs

不要一开始就先接外部观测平台，也不要先做花哨 UI。真正影响日常可用性的关键是：

- 事件模型是否统一
- 历史 run 是否能检索
- timeline 是否能准确解释一次执行过程

---

## 20. 最终结论

`rusty-claw-oc` 现在距离“日常可用 tracing”并不远，缺的不是更多日志，而是把已有：

- `AgentLoop`
- `turn_id`
- `task_id`
- `EventLog`
- `SubagentDebugSnapshot`
- `ACP`
- `context diff / compaction`

这些能力收束到一套统一的 Trace Center 架构中。

最值得投资的方向是：

1. 统一事件协议
2. 独立 TraceBus
3. 双存储
4. ACP Trace UI

这套方案做完之后，你调试“为什么模型重试了三次”“为什么这次 subagent 卡死”“为什么上下文突然退化”“为什么两个 run 结果不一样”会从现在的手工翻 log，变成几分钟内可定位的问题。
