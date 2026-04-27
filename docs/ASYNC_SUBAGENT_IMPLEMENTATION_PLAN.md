# Rusty-Claw 异步 Subagent 实施计划 (v3 — 严格评审收敛版)

> **状态**: 待实施  
> **作者**: 原始设计 + 多轮 Code Review 合并  
> **最后更新**: 2026-03-28  

---

## 1. 目标与范围

### 1.1 本期目标

在现有 `AgentLoop` + `ToolContext` 串行模型之外，新增一个**后台任务运行时**，使主 Agent 能够：

1. 以 fire-and-forget 方式启动后台 subagent
2. 主 Agent 继续执行其他工作
3. 后续显式查询 / 回收 subagent 结果
4. 支持取消、超时、并发上限和自动清理

### 1.2 设计原则

| 编号 | 原则 | 说明 |
|------|------|------|
| P1 | 运行时分离 | 异步 subagent 由 `SubagentRuntime` 统一管理生命周期，不靠 tool 局部变量维持 |
| P2 | Builder 统一 | 同步/异步 subagent 复用同一套构建入口，但通过策略参数区分行为，避免兼容性回归 |
| P3 | 显式查询优先 | 第一阶段只支持主 Agent 主动查询结果，不支持对运行中父会话的主动 prompt 注入 |
| P4 | 保守权限 | 异步 subagent 默认只读；同步兼容接口保持原语义，不因 builder 复用而被动收紧 |
| P5 | 渐进迁移 | 保留同步 `dispatch_subagent` 接口，渐进引导模型使用异步接口 |
| P6 | Factory 单一入口 | `session/factory.rs` 是 AgentLoop 的唯一构造入口 (SRP) |
| P7 | 任务所有权明确 | 后台 task 必须有明确所有权与观察点，不能只依赖 cooperative cancellation |

### 1.3 明确不做（Non-Goals）

1. 不支持跨进程持久化恢复后台 subagent
2. 不支持向正在运行中的父 `AgentLoop` 直接注入 prompt
3. 不支持文件级锁
4. 不支持阻塞式 `wait_subagent(block=true)` 作为主路径
5. 不支持无限并发或无上限后台任务
6. 子 Agent **不继承**父 skill 的约束（独立执行环境）
7. 子 Agent 的 artifacts 通过 `SubagentResult.artifacts` 回报，不自动纳入父 `ActiveSkillState`

---

## 2. 当前实现现状

### 2.1 同步阻塞的 `dispatch_subagent`

- [src/tools/subagent.rs](file:///Users/liuli/src/rust-claw-oc/src/tools/subagent.rs)
- 在 tool `execute()` 内部构造临时 `AgentLoop`，`timeout(sub_loop.step(goal))`，一次性返回
- 主 Agent 在 subagent 完成前无法继续执行

### 2.2 主循环的 tool round 是串行模型

- [src/core/step_helpers.rs](file:///Users/liuli/src/rust-claw-oc/src/core/step_helpers.rs)
- `execute_tool_round()` 逐个执行 tool call，因此 `spawn_subagent` 必须立即返回

### 2.3 ToolContext 无法承载异步回注

- [src/tools/protocol.rs](file:///Users/liuli/src/rust-claw-oc/src/tools/protocol.rs)
- `ToolContext` 只有 `session_id` 和 `reply_to`，无 `SessionManager` 或父 `AgentLoop` 句柄

### 2.4 已有后台任务范式

- [src/scheduler.rs](file:///Users/liuli/src/rust-claw-oc/src/scheduler.rs) — 已采用 `tokio::spawn` + `SessionManager` + `agent.step(injected_goal)` 模式
- 项目接受"后台任务由 runtime/service 托管"的架构理念

### 2.5 已有 ExecutionExtension 体系

- [src/core/extensions.rs](file:///Users/liuli/src/rust-claw-oc/src/core/extensions.rs) — `ExecutionExtension` trait 已落地
- [src/session/factory.rs:51](file:///Users/liuli/src/rust-claw-oc/src/session/factory.rs#L51) — `SkillRuntime` 已作为第一个扩展挂载

### 2.6 Session Factory 与 Subagent 的构建逻辑重叠

- `build_agent_session()` ([factory.rs:11-54](file:///Users/liuli/src/rust-claw-oc/src/session/factory.rs#L11-L54)) 和 `DispatchSubagentTool::execute()` ([subagent.rs:94-218](file:///Users/liuli/src/rust-claw-oc/src/tools/subagent.rs#L94-L218)) 有大量重叠步骤

| 步骤 | `DispatchSubagentTool` | `build_agent_session` |
|------|:---:|:---:|
| 创建 AgentContext | ✓ | ✓ |
| 创建 TelemetryExporter | ✓ | ✓ |
| 创建 TaskStateStore | ✓ | ✓ |
| 注入 TaskPlanTool / final-text completion semantics | ✓ | ✓ |
| 加载 MEMORY.md / AGENTS.md | ✓ (手动) | 通过 AgentContext |
| 注入 AskUserQuestionTool | ✗ | ✓ |
| 注入 DispatchSubagentTool | ✗ (禁递归) | ✓ |
| 挂载 SkillRuntime extension | ✗ | ✓ |

**结论**：必须将 builder 抽到 `session/factory.rs` 作为变体构建函数。

---

## 3. 总体方案

### 3.1 架构分层

```
┌───────────────────────────────────────────────────┐
│  Tool 层 (模型可见接口)                              │
│  spawn_subagent / get_subagent_result /            │
│  cancel_subagent / list_subagent_jobs /             │
│  dispatch_subagent (同步兼容)                       │
└────────────┬──────────────────────────────────────┘
             │
┌────────────▼──────────────────────────────────────┐
│  SubagentRuntime (后台任务生命周期管理)               │
│  spawn_job / get_job_snapshot / cancel_job /        │
│  list_jobs / cleanup_expired_jobs                  │
└────────────┬──────────────────────────────────────┘
             │
┌────────────▼──────────────────────────────────────┐
│  session/factory.rs (唯一 AgentLoop 构造入口)       │
│  build_agent_session / build_subagent_session      │
└───────────────────────────────────────────────────┘
```

### 3.2 与 ExecutionExtension 体系的关系

- `SubagentRuntime` **不实现** `ExecutionExtension`，它是独立的全局共享组件
- `build_subagent_session()` 接受可选的 `extensions` 参数，第一阶段传空
- 为 Phase 2+ 预留 `SubagentNotificationExtension`（空实现，挂载到父 session），其 `before_turn_start()` 可检查是否有已完成 job 需通知

### 3.3 工具组

| 工具 | 类型 | 说明 |
|------|------|------|
| `spawn_subagent` | 新增 | 非阻塞创建后台 subagent，立即返回 `job_id` |
| `get_subagent_result` | 新增 | 查询任务状态；若已完成，返回结果 |
| `cancel_subagent` | 新增 | 请求取消后台 subagent |
| `list_subagent_jobs` | 新增 | 查看所有后台任务状态 |
| `dispatch_subagent` | 保留 | 同步兼容接口，改为复用 builder |

### 3.4 依赖注入闭环

v3 明确依赖注入链路，避免实现时出现循环依赖：

1. `app/bootstrap.rs` 继续负责创建“基础工具集（base tools）”，这里不包含：
   - `TaskPlanTool`
   - final-text completion semantics
   - `AskUserQuestionTool`
   - `DispatchSubagentTool`
   - 新增异步 subagent 工具组
2. `SessionManager` 在初始化时持有：
   - 全局默认 `llm`
   - 基础工具集 `base_tools`
   - `SubagentRuntime`
3. `SubagentRuntime` 只依赖：
   - `llm`
   - `base_tools`
4. `session/factory.rs` 在构建正式 session 时注入：
   - runtime 级工具：`spawn_subagent` / `get_subagent_result` / `cancel_subagent` / `list_subagent_jobs`
   - session 级工具：`TaskPlanTool` / final-text completion semantics / `AskUserQuestionTool`
   - 同步兼容工具：`dispatch_subagent`

关键约束：

- `SubagentRuntime` 不依赖完整 session tools，只依赖 bootstrap 产出的 base tools
- `build_subagent_session()` 自己补齐 subagent 所需的最小运行时工具
- 这样可以避免 `SubagentRuntime -> factory -> SessionManager -> SubagentRuntime` 的闭环

---

## 4. 核心数据结构

### 4.1 `SubagentResult`

复用现有结构，不新增 `finish_reason` 字段（由 `SubagentJobState` 枚举派生）：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentResult {
    pub ok: bool,
    pub summary: String,
    pub findings: Vec<String>,
    pub artifacts: Vec<String>,
}
```

### 4.2 `SubagentJobMeta`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentJobMeta {
    pub job_id: String,
    pub parent_session_id: String,
    pub parent_reply_to: String,
    pub sub_session_id: String,
    pub goal: String,
    pub input_summary: String,
    pub allowed_tools: Vec<String>,
    pub timeout_sec: u64,
    pub max_steps: usize,
    pub created_at_unix_ms: u64,
}
```

### 4.3 `SubagentJobState`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SubagentJobState {
    Pending,
    Running {
        started_at_unix_ms: u64,
    },
    Completed {
        finished_at_unix_ms: u64,
        result: SubagentResult,
    },
    Failed {
        finished_at_unix_ms: u64,
        error: String,
        partial: Option<SubagentResult>,
    },
    Cancelled {
        finished_at_unix_ms: u64,
        partial: Option<SubagentResult>,
    },
    TimedOut {
        finished_at_unix_ms: u64,
        partial: Option<SubagentResult>,
    },
}

impl SubagentJobState {
    /// 从枚举变体派生 finish_reason，消除与 SubagentResult 的语义冗余
    pub fn finish_reason(&self) -> &str {
        match self {
            Self::Pending => "pending",
            Self::Running { .. } => "running",
            Self::Completed { .. } => "finished",
            Self::Failed { .. } => "failed",
            Self::Cancelled { .. } => "cancelled",
            Self::TimedOut { .. } => "timed_out",
        }
    }

    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Pending | Self::Running { .. })
    }
}
```

### 4.4 `SubagentJobHandle`

v3 修订：

- 外层仍使用 `Arc<SubagentJobHandle>`
- 保留 cooperative cancellation
- 同时保留后台 task 句柄的所有权，以支持 panic 观测、关停时 abort 和运行态调试

```rust
pub struct SubagentJobHandle {
    pub meta: SubagentJobMeta,
    pub state: tokio::sync::RwLock<SubagentJobState>,
    pub cancelled: std::sync::atomic::AtomicBool,
    pub cancel_notify: tokio::sync::Notify,
    pub created_at: std::time::Instant,
    pub task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
```

说明：

- `task` 不用于日常查询结果，而用于“任务所有权”
- `cancel_subagent` 仍以 cooperative cancellation 为主
- 进程关闭或测试 teardown 时，可选择 `abort` 未完成任务
- 后台 task panic 时，可通过 `JoinHandle` 的 `JoinError` 做日志记录或状态补偿

### 4.5 `SubagentRuntime`

采用 `Arc<Inner>` 模式（类似 `reqwest::Client`），使 `Clone` 语义清晰：

```rust
#[derive(Clone)]
pub struct SubagentRuntime {
    inner: std::sync::Arc<SubagentRuntimeInner>,
}

struct SubagentRuntimeInner {
    jobs: tokio::sync::RwLock<
        std::collections::HashMap<String, std::sync::Arc<SubagentJobHandle>>
    >,
    running_jobs: std::sync::atomic::AtomicUsize,
    max_concurrent_jobs: usize,
    llm: std::sync::Arc<dyn crate::llm_client::LlmClient>,
    base_tools: Vec<std::sync::Arc<dyn crate::tools::Tool>>,
}
```

> **备注**：不引入 `dashmap` 依赖，统一使用 `tokio::sync::RwLock<HashMap<...>>`。

---

## 5. Builder 统一设计

### 5.1 新增 `build_subagent_session()` 到 `session/factory.rs`

```rust
pub enum SubagentBuildMode {
    /// 异步后台 subagent：默认只读，不允许递归，不允许交互
    AsyncReadonly,
    /// 同步兼容接口：尽量保持当前 dispatch_subagent 语义
    SyncCompatible,
}

pub fn build_subagent_session(
    parent_session_id: &str,
    parent_reply_to: &str,
    llm: Arc<dyn LlmClient>,
    base_tools: &[Arc<dyn Tool>],
    mode: SubagentBuildMode,
    allowed_tools: &[String],
    energy_budget: usize,
    input_summary: &str,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
) -> Result<(AgentLoop, Arc<CollectorOutput>), String>
```

**职责**：
1. 构造 subagent 专用 `AgentContext`
2. 加载 `MEMORY.md` / `AGENTS.md`
3. 生成 `sub_session_id`
4. 创建 telemetry
5. 创建 `TaskStateStore`
6. 按 `mode` 过滤 allowed tools
7. 注入 `TaskPlanTool` / final-text completion semantics
8. 创建 `CollectorOutput`
9. 创建 `AgentLoop` 并设置 energy budget + cancel token
10. **不注入** `AskUserQuestionTool`（后台任务不支持交互）
11. **不注入** `DispatchSubagentTool`（禁止递归 subagent）
12. **不挂载** `SkillRuntime` extension

### 5.2 写工具硬限制

在 builder 中对 `allowed_tools` 执行过滤时，按模式执行：

- `AsyncReadonly`：无条件剔除以下工具
- `SyncCompatible`：保持现有 `dispatch_subagent` 语义，不额外引入比当前更强的限制

也就是说，下面的黑名单只适用于异步后台 subagent，不自动施加到同步兼容接口上。

```rust
const FORBIDDEN_WRITE_TOOLS: &[&str] = &[
    "write_file",
    "patch_file",
    "execute_bash",
    "send_file",
    "write_memory",
    "rag_insert",
    "manage_schedule",
    "send_telegram_message",
];
```

后续实现中，`allow_writes: true` 可以按受控模式开放，但必须满足：

1. 至少声明一个 `claimed_paths`
2. 仅放开 `write_file` / `patch_file`
3. 仍然禁止 `execute_bash` 等高风险工具

### 5.3 同步 `DispatchSubagentTool` 改造

改为调用 `build_subagent_session(mode = SubagentBuildMode::SyncCompatible)`，删除内部重复的构造逻辑，但保持当前同步接口的行为兼容。

---

## 6. Tool API 设计

### 6.1 `spawn_subagent`

**参数**：

```json
{
  "goal": "Analyze the parser module",
  "input_summary": "We need a concise architectural review of parser-related code.",
  "allowed_tools": ["read_file", "web_fetch"],
  "timeout_sec": 120,
  "max_steps": 8
}
```

**返回**：

```json
{
  "ok": true,
  "job_id": "subjob_xxx",
  "status": "spawned",
  "sub_session_id": "sub_parent_xxx",
  "message": "Subagent started in background."
}
```

**流程**：
1. 校验参数
2. 执行机会式清理
3. 检查并发上限 (`max_concurrent_jobs = 3`)
4. 生成 `job_id` / `sub_session_id`
5. 注册 `Pending` job
6. `tokio::spawn` 后台执行
7. 立即返回

### 6.2 `get_subagent_result`

**参数**：

```json
{
  "job_id": "subjob_xxx"
}
```

**运行中返回**：

```json
{
  "ok": true,
  "job_id": "subjob_xxx",
  "status": "running",
  "message": "Subagent is still running."
}
```

**已完成返回**：

```json
{
  "ok": true,
  "job_id": "subjob_xxx",
  "status": "completed",
  "finish_reason": "finished",
  "result": {
    "ok": true,
    "summary": "...",
    "findings": ["..."],
    "artifacts": ["src/foo.rs"]
  }
}
```

> `finish_reason` 由 `SubagentJobState::finish_reason()` 派生，不作为 `SubagentResult` 的存储字段。

### 6.3 `cancel_subagent`

**参数**：

```json
{
  "job_id": "subjob_xxx"
}
```

**返回**：

```json
{
  "ok": true,
  "job_id": "subjob_xxx",
  "status": "cancelling"
}
```

**流程**：
1. 查找 job handle
2. 设置 `cancelled = true`
3. `cancel_notify.notify_waiters()`
4. 子 `AgentLoop` 在 `check_loop_guards()` 时检测到取消标志并退出
5. 后台任务将状态写为 `Cancelled`

### 6.4 `list_subagent_jobs`

**返回字段**：`job_id`, `goal`, `status`, `finish_reason`, `created_at_unix_ms`, `parent_session_id`

---

## 7. Runtime 核心流程

### 7.1 后台任务所有权

v3 明确规定：

- `SubagentRuntime` 对每个后台任务保留一份 `JoinHandle`
- `JoinHandle` 不对模型暴露“阻塞等待”语义
- `JoinHandle` 只承担运行态托管职责：
  - 观测后台 task panic
  - 在测试或进程关停时显式 `abort`
  - 避免后台任务所有权悬空

因此：

- 结果查询一律以 `SubagentJobState` 为准
- 取消仍以 cooperative cancellation 为主
- `JoinHandle` 是运行时内部治理能力，不是产品 API

### 7.2 后台任务执行流

```text
spawn_subagent tool
  -> runtime.spawn_job()
    -> cleanup_expired_jobs()
    -> check concurrency limit
    -> create job meta + handle (state=Pending)
    -> insert into jobs map
    -> running_jobs.fetch_add(1) via RunningJobGuard
    -> tokio::spawn(async move {
         let _guard = RunningJobGuard::new(&running_jobs);
         set state = Running
         build_subagent_session(...)
         timeout(agent.step(goal))
         collect outputs
         map to terminal SubagentJobState
       })
    -> store JoinHandle into handle.task
  -> return job_id immediately
```

### 7.3 状态转换

```text
Pending -> Running -> Completed
                   -> Failed
                   -> TimedOut
                   -> Cancelled
```

规则：
- 终态不可逆
- 清理器只处理终态任务

### 7.4 并发计数器 RAII Guard

使用与 `core.rs` 中 `ScopeGuard` 一致的 RAII 模式，确保 panic、builder 失败、timeout、cancel、正常完成都能正确递减：

```rust
struct RunningJobGuard {
    counter: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl RunningJobGuard {
    fn new(counter: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for RunningJobGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
```

规则：

1. 成功占用并发配额后立即创建 guard
2. 后续不允许 scattered `fetch_sub`
3. 所有递减都由 `Drop` 统一完成

---

## 8. 并发控制与资源治理

### 8.1 全局并发上限

- `max_concurrent_jobs = 3`
- 超过上限时 `spawn_subagent` 返回 `ToolError::ExecutionFailed`
- 错误文案：`"Too many concurrent subagent jobs. Wait for existing jobs to finish before spawning more."`

### 8.2 参数上限

| 参数 | 默认值 | 最大值 |
|------|--------|--------|
| `timeout_sec` | 60 | 600 |
| `max_steps` | 5 | 30 |

### 8.3 输出裁剪

| 字段 | 上限 |
|------|------|
| `summary` | 8 KB |
| `findings` 每项 | 2 KB |
| `findings` 总数 | 32 条 |
| `artifacts` 总数 | 128 条 |

---

## 9. 回收与清理策略

### 9.1 清理规则（vNext：支持 `consume` 语义）

| 条件 | 清理延迟 |
|------|---------|
| 终态 job，未消费 | 30 分钟后可删除 |
| 终态 job，已消费 | 5 分钟后可删除 |
| 运行中 job | 不清理 |

说明：

- `get_subagent_result` 新增 `consume: bool = false`
- 终态 job 在 `consume=true` 时会被标记为“已消费”
- 标记消费是幂等的，重复消费不会刷新消费时间
- 终态 job 在 TTL 内仍可重复读取；`consume=true` 只影响清理优先级

### 9.2 清理触发方式

**双保险策略**：

1. **机会式清理**：在 `spawn_job()` / `get_job_snapshot()` / `cancel_job()` / `list_jobs()` 入口前做轻量扫描
2. **兜底定时清理**：在 `SubagentRuntime::new()` 中 spawn 一个低频后台任务，每 5 分钟扫描一次

```rust
// 兜底清理任务
tokio::spawn(async move {
    let mut interval = tokio::time::interval(Duration::from_secs(300));
    loop {
        interval.tick().await;
        runtime_clone.cleanup_expired_jobs().await;
    }
});
```

---

## 10. Workspace 冲突控制

### 10.1 第一阶段策略

1. 默认 `allowed_tools` 仅允许只读工具
2. Builder 层面硬限制写工具（第5.2节 `FORBIDDEN_WRITE_TOOLS`）
3. 所有写操作路径通过 `artifacts` 回报

### 10.2 工具描述约束

在 `spawn_subagent` 的 tool description 中明确写出：

> "Background subagents run with read-only tools by default. When multiple subagents run concurrently, do not grant them write access to overlapping files."

### 10.3 二阶段可选增强

当前实现已支持 `claimed_paths: Vec<String>` 做软冲突检测；若启用 `allow_writes: true`，则必须显式声明该字段。

---

## 11. 与现有同步 `dispatch_subagent` 的关系

### 11.1 保留原因

1. 避免立即破坏现有 prompt/tool 行为
2. 某些任务仍适合同步等待结果
3. 渐进迁移成本更低

### 11.2 改造内容

- 内部改为调用 `build_subagent_session()`
- 删除重复的构造逻辑
- 保留原有参数和返回格式

### 11.3 中长期方向

- 在 tool description 中优先引导模型使用异步接口
- 同步接口作为 fallback

---

## 12. 文件变更清单

### 12.1 新增文件

| 文件 | 职责 |
|------|------|
| `src/subagent_runtime.rs` | `SubagentRuntime`, `SubagentJobHandle`, `SubagentJobMeta`, `SubagentJobState`, `RunningJobGuard`, 清理逻辑, spawn/cancel/result 查询 |
| `src/tools/subagent_async.rs` | `SpawnSubagentTool`, `GetSubagentResultTool`, `CancelSubagentTool`, `ListSubagentJobsTool` |

### 12.2 修改文件

| 文件 | 变更内容 |
|------|---------|
| `src/session/factory.rs` | 新增 `build_subagent_session()` 函数；导出 `CollectorOutput` |
| `src/tools/subagent.rs` | `DispatchSubagentTool::execute()` 改为调用 `build_subagent_session()`；删除内部重复构造逻辑；`CollectorOutput` 移动到 `session/factory.rs` |
| `src/tools/mod.rs` | 新增 `pub mod subagent_async;` 和对应 re-export |
| `src/app/bootstrap.rs` | 产出基础工具集 `base_tools`，供 `SessionManager` 和 `SubagentRuntime` 共享 |
| `src/session_manager.rs` | 持有 `SubagentRuntime` 与 `base_tools`，并在构建 session 时传入 factory |
| `src/main.rs` | 串联 bootstrap、runtime、session manager，不直接重复组装 tools |

### 12.3 不变文件

| 文件 | 原因 |
|------|------|
| `src/core.rs` | 本期不改动主循环 |
| `src/core/extensions.rs` | 本期不新增扩展 |
| `src/core/step_helpers.rs` | 工具执行逻辑无需改动 |
| `src/tools/protocol.rs` | 已有 `ToolExecutionEnvelope` 足够 |

### 12.4 初始化顺序

v3 明确初始化顺序如下：

1. `app/bootstrap.rs` 创建基础工具集 `base_tools`
2. 创建全局默认 `llm`
3. 使用 `llm + base_tools.clone()` 创建 `SubagentRuntime`
4. 使用 `llm + base_tools + runtime` 创建 `SessionManager`
5. `SessionManager::get_or_create_session()` 调用 `build_agent_session(..., runtime.clone(), base_tools.clone(), ...)`

这样：

- runtime 不依赖 factory 产出的 session tools
- factory 不需要反向向 runtime 请求工具集
- 避免构造顺序和所有权死结

---

## 13. 测试计划

### 13.1 Runtime 单元测试 (`src/subagent_runtime.rs`)

| # | 测试场景 | 验证内容 |
|---|---------|---------|
| R1 | 正常 spawn → running → completed | 状态转换正确，result 可读 |
| R2 | 参数错误或 builder 错误 → failed | `Failed` 状态，error 非空 |
| R3 | timeout → timed_out | `TimedOut` 状态，partial result |
| R4 | cancel → cancelled | cooperative 取消生效 |
| R5 | 超过并发上限 → spawn 失败 | 返回 `ToolError::ExecutionFailed` |
| R6 | 终态 job 超时后被清理 | `cleanup_expired_jobs()` 正确删除 |
| R7 | 清理不会误删运行中任务 | 运行中 job 在清理后仍存在 |
| R8 | 并发 spawn + cancel 交叉 | `running_jobs` 计数器始终正确 |
| R9 | builder 失败时 running_jobs 正确递减 | RAII guard 保障 |

### 13.2 Tool 单元测试 (`src/tools/subagent_async.rs`)

| # | 测试场景 | 验证内容 |
|---|---------|---------|
| T1 | `spawn_subagent` schema 参数校验 | 错误参数返回 `InvalidArguments` |
| T2 | `get_subagent_result` 返回 running | status = "running" |
| T3 | `get_subagent_result` 返回 completed | status = "completed", result 非空 |
| T4 | `cancel_subagent` 对未知 job | 返回错误 |
| T5 | 返回 envelope 可被解析 | `ToolExecutionEnvelope::from_json_str()` 成功 |

### 13.3 集成测试

| # | 测试场景 | 验证内容 |
|---|---------|---------|
| I1 | 主 Agent 连续派发两个后台 subagent | 两个 job 同时执行 |
| I2 | 主 Agent 在后续轮次显式回收结果 | 结果完整可用 |
| I3 | 一个 subagent 失败不影响另一个 | 正常 job 不受影响 |
| I4 | 取消中的 subagent 不阻塞主 Agent | 取消后主 Agent 继续 |
| I5 | 超时精度 (`timeout_sec=2`) | 实际超时耗时在合理范围 |
| I6 | CollectorOutput 溢出截断 | 超过 8KB summary 被截断 |

### 13.4 Builder 回归测试

| # | 测试场景 | 验证内容 |
|---|---------|---------|
| B1 | 同步 `dispatch_subagent` 参数校验 | 改造后行为不变 |
| B2 | 同步 `dispatch_subagent` 正常执行 | 结果格式不变 |
| B3 | 异步只读策略生效 | `mode = AsyncReadonly` 时即使传入 `allowed_tools: ["write_file"]`，实际不可用 |
| B4 | 同步兼容策略不回归 | `mode = SyncCompatible` 时不比当前实现额外收紧能力 |

---

## 14. 分阶段 TODO 清单

### Phase 1: Builder 统一 + 数据结构

**目标**：消除构建逻辑重复，建立数据基础

- [x] **TODO-1.1** 将 `CollectorOutput` 从 `src/tools/subagent.rs` 移动到 `src/session/factory.rs`（公开导出）
- [x] **TODO-1.2** 在 `src/session/factory.rs` 新增 `build_subagent_session()` 函数
- [x] **TODO-1.3** 实现写工具硬限制（`FORBIDDEN_WRITE_TOOLS` 常量 + 过滤逻辑）
- [x] **TODO-1.4** 改造同步 `DispatchSubagentTool::execute()` 调用 `build_subagent_session()`
- [x] **TODO-1.5** 新增 `src/subagent_runtime.rs`：定义 `SubagentJobMeta`, `SubagentJobState`, `SubagentJobHandle`, `SubagentResult`, `RunningJobGuard` 数据结构
- [x] **TODO-1.5a** 明确 `SubagentBuildMode::{AsyncReadonly, SyncCompatible}`，禁止 builder 复用造成同步接口语义回归
- [x] **TODO-1.6** 验证：全部现有测试通过 (`cargo test`)
- [x] **TODO-1.7** 验证：`cargo clippy -- -D warnings` 通过

### Phase 2: Runtime 核心

**目标**：实现后台任务运行时

- [x] **TODO-2.1** 实现 `SubagentRuntime::new()` 构造函数（含兜底清理任务 spawn）
- [x] **TODO-2.2** 实现 `SubagentRuntime::spawn_job()` — 创建 job + `tokio::spawn` 后台执行
- [x] **TODO-2.3** 实现 `SubagentRuntime::run_job()` — 调用 builder、执行 step、状态转换
- [x] **TODO-2.4** 实现 `SubagentRuntime::get_job_snapshot()`
- [x] **TODO-2.5** 实现 `SubagentRuntime::cancel_job()`
- [x] **TODO-2.6** 实现 `SubagentRuntime::list_jobs()`
- [x] **TODO-2.7** 实现 `SubagentRuntime::cleanup_expired_jobs()`
- [x] **TODO-2.7a** 保留 `JoinHandle` 以支持 panic 观测、shutdown abort 和运行态托管
- [x] **TODO-2.8** 编写 Runtime 单元测试 R1-R9
- [x] **TODO-2.9** 验证：`cargo test` + `cargo clippy` 通过

### Phase 3: Tool 层 + 注册

**目标**：模型可使用异步 subagent

- [x] **TODO-3.1** 新增 `src/tools/subagent_async.rs`：实现 `SpawnSubagentTool`
- [x] **TODO-3.2** 实现 `GetSubagentResultTool`
- [x] **TODO-3.3** 实现 `CancelSubagentTool`
- [x] **TODO-3.4** 实现 `ListSubagentJobsTool`
- [x] **TODO-3.5** 更新 `src/tools/mod.rs` 导出
- [x] **TODO-3.6** 更新 `src/app/bootstrap.rs`：构造 `SubagentRuntime`
- [x] **TODO-3.7** 更新 `src/session/factory.rs`：在 `build_agent_session()` 中注入 4 个异步 tool
- [x] **TODO-3.7a** 打通 `bootstrap -> runtime -> session_manager -> factory` 的依赖注入链路，避免循环依赖
- [x] **TODO-3.8** 编写 Tool 单元测试 T1-T5
- [x] **TODO-3.9** 编写 Builder 回归测试 B1-B4
- [x] **TODO-3.10** 验证：`cargo test` + `cargo clippy` 通过

### Phase 4: 集成测试 + 文档

**目标**：端到端验证

- [x] **TODO-4.1** 编写集成测试 I1-I6
- [x] **TODO-4.2** 更新 `AGENTS.md` 添加异步 subagent 工具说明
- [x] **TODO-4.3** 最终验证：`cargo test` + `cargo clippy` + `cargo fmt --check` 全部通过

---

## 15. 风险登记表

| # | 风险 | 影响 | 概率 | 应对措施 |
|---|------|------|------|---------|
| 1 | 同步/异步 subagent 行为漂移 | 高 | 中 | Builder 统一 (TODO-1.2) |
| 2 | 后台任务泄漏 (内存堆积) | 中 | 中 | 兜底定时清理 (第9.2节) + RAII guard (第7.3节) |
| 3 | Workspace 写冲突 | 高 | 低 | 运行时硬限制写工具 (TODO-1.3) |
| 4 | 后台取消不及时 | 低 | 中 | 复用现有 cooperative cancellation，视为 best-effort |
| 5 | 模型忙轮询 get_subagent_result | 低 | 高 | 第一阶段接受；Phase 2+ 引入 notification inbox |
| 6 | 并发计数器偏移 | 中 | 低 | `RunningJobGuard` RAII (TODO-1.5) |
| 7 | 新依赖引入 | 低 | - | 不引入 `dashmap`，使用 `tokio::sync::RwLock<HashMap>` |

---

## 16. 二阶段扩展方向（备忘，不在本期实施）

1. **通知机制**：`SubagentNotificationExtension` 挂载到父 session，`before_turn_start()` 检查已完成 job
2. **子 Agent 挂载 SkillRuntime**：builder 接受可选 `extensions` 参数
