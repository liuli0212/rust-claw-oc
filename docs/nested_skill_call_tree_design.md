# Rusty-Claw 多层 Skill 调用设计

## 1. 背景

当前仓库已经引入了 `call_skill`，允许一个 skill 通过受限子会话委派另一个 skill 执行。这个方向是合理的，因为它复用了已有的：

- `AgentLoop`
- `SkillRuntime`
- `build_subagent_session()`
- timeout / step budget / transcript / event log

但当前实现本质上仍是**单层委派**：

- 主 skill 可以调用子 skill
- 子 skill 不能继续调用 skill
- 也没有显式的防环机制

如果要支持多层 skill 调用，核心问题不是“如何允许任意递归”，而是：

1. 如何支持常见的分层委派场景，例如 `A -> B -> C`
2. 如何阻止 `A -> B -> A`、`A -> A` 这类环
3. 如何防止多层调用在 budget、权限和工具集上失控

本文档提出的方案是：**把多层 skill 调用建模为有界调用树，而不是任意递归图。**

## 2. 设计目标

### 2.1 必须达成

1. 支持多层 skill 调用，例如 `A -> B -> C`
2. 显式阻止 skill 调用环
3. 为每层 skill 继承并收缩运行 budget
4. 保证子 skill 的工具能力不会超过父 skill 的可见能力
5. 让错误信息、日志和调试信息足够清晰
6. 尽量复用当前 `subagent + SkillRuntime` 架构，不额外发明第二套执行系统

### 2.2 第一版明确不做

1. 不支持通用递归
2. 不支持无界 skill 调用图
3. 不支持在第一版按“相同 skill + 不同参数”做精细去环
4. 不支持放开回到祖先 skill 的重入行为

## 3. 推荐模型：有界 Skill Call Tree

### 3.1 基本原则

多层调用不应建模为任意图，而应建模为一棵有界调用树。

允许：

- `A -> B`
- `A -> B -> C`

拒绝：

- `A -> A`
- `A -> B -> A`
- `A -> B -> C -> B`

### 3.2 为什么用树而不是图

skill 不是普通函数：

- skill 由 LLM 驱动，行为不是确定性的
- skill 可能有副作用
- skill 可能产生用户交互、文件操作、子任务派发
- 即使逻辑上“应该会停”，模型仍可能在边界条件下绕圈

因此，第一版更稳的目标不是“允许递归但想办法收敛”，而是“允许分层委派，但禁止回到祖先”。

## 4. 核心机制：Skill Lineage

### 4.1 引入显式调用链

每次 `call_skill` 时，运行时都应携带一条显式 lineage：

```text
["office_hours", "plan_ceo_review", "plan_eng_review"]
```

lineage 不应只存在于 prompt 中，而应属于运行时上下文的一部分。

### 4.2 推荐数据结构

```rust
pub struct SkillCallFrame {
    pub skill_name: String,
    pub call_id: String,
    pub parent_call_id: Option<String>,
    pub depth: usize,
    pub args_digest: Option<String>,
}

pub struct SkillCallContext {
    pub lineage: Vec<SkillCallFrame>,
    pub total_skill_calls: usize,
    pub root_session_id: String,
}
```

### 4.3 挂载位置

推荐把 `SkillCallContext` 放在 sub-session 构建时可继承的上下文中，而不是只放在 `ActiveSkillState` 内。

原因：

- `call_skill` 在 tool 执行路径中就需要判断是否允许发起调用
- 子 session 启动前就需要拿到 ancestry 和 budget 信息
- 调试和 tracing 也需要这份信息

## 5. 防环策略

### 5.1 第一版规则

第一版直接按 **skill name 是否已出现在 ancestry 中** 判环：

- 若目标 skill 已经出现在 lineage 中，则拒绝调用
- 若目标 skill 等于当前 active skill，则拒绝调用

即：

```text
target_skill in lineage => deny
```

### 5.2 为什么第一版不按“skill + 参数”判环

例如是否允许：

- `planner(topic=a) -> search(a) -> planner(topic=b)`

这类策略过早引入会带来很多复杂性：

- 需要定义参数标准化
- 需要决定哪些字段参与等价比较
- 需要解释“看起来不同但本质回环”的情况

因此第一版应采用更保守的策略：**只要目标 skill 名字出现在祖先链上，就拒绝。**

### 5.3 错误提示

错误信息必须清晰，例如：

```text
Denied skill call: cycle detected: office_hours -> plan_ceo_review -> office_hours
```

不要只返回泛化的 `ExecutionFailed` 或 `InvalidArguments`。

## 6. 深度与总调用数限制

防环之外，还必须增加有界限制。

### 6.1 最大深度

推荐增加：

```rust
pub const MAX_SKILL_CALL_DEPTH: usize = 3;
```

含义：

- 根 skill 算第 1 层
- 子 skill 算第 2 层
- 孙 skill 算第 3 层

这样已经足够覆盖大多数实际委派场景。

### 6.2 总 skill 调用数上限

即使不存在环，也可能出现横向爆炸：

- `A` 连续调用多个 skill
- 每个 skill 又继续调用多个 skill

因此建议再增加：

```rust
pub const MAX_SKILL_CALLS_PER_ROOT_REQUEST: usize = 6;
```

判断逻辑：

- 从顶层请求开始累计
- 每次 `call_skill` 成功发起前先检查计数
- 超过限制则拒绝

### 6.3 拒绝信息

例如：

```text
Denied skill call: max nested skill depth exceeded (3)
Denied skill call: max total nested skill calls exceeded (6)
```

## 7. Budget 继承模型

仅做防环不够，还必须保证多层调用不会吞掉整次请求的执行预算。

### 7.1 Step Budget 继承

子 skill 的 `max_steps` 不应完全由调用方自由决定。应满足：

```text
child_steps <= min(requested_steps, parent_remaining_steps - reserve)
```

其中：

- `requested_steps` 是当前 tool 参数中请求的值
- `parent_remaining_steps` 是父 skill 当前可用的剩余 budget
- `reserve` 是给父 skill 留出的最小剩余步数

推荐第一版使用简单策略：

- 每次向下委派时，默认只允许拿到父 skill 剩余 budget 的一部分
- 同时保留父 skill 的最小回收预算

例如：

- 根 skill 剩余 12 步，子 skill 最多拿 6 步
- 子 skill 剩余 6 步，孙 skill 最多拿 3 步

### 7.2 Timeout 继承

同理：

```text
child_timeout <= parent_remaining_timeout
```

不能出现父 skill 只剩 20 秒，子 skill 却还请求 120 秒的情况。

### 7.3 默认预算裁剪建议

第一版可用如下简化策略：

- `effective_child_steps = min(requested_or_default, max(1, parent_remaining_steps / 2))`
- `effective_child_timeout = min(requested_or_default, parent_remaining_timeout)`

这比完全自由分配更稳。

## 8. 权限与工具集模型

这是多层 skill 调用最关键的边界之一。

### 8.1 当前单层模型的问题

当前 `call_skill` 接口如果继续让 caller 直接传 `allowed_tools`，会导致：

- 父 skill 必须知道子 skill 内部需要什么工具
- skill 封装被破坏
- 工具可见性容易和 skill 定义不一致

### 8.2 推荐规则：子 skill 工具集取交集

推荐：

```text
effective_child_tools =
    callee_declared_tools
    ∩ parent_visible_tools
    ∩ runtime_policy_allowed_tools
```

含义：

- 子 skill 只能拿到自己声明需要的工具
- 子 skill 不能获得父 skill 不可见的能力
- 运行时硬约束始终生效

### 8.3 `allow_subagents` 的语义

建议保持逐层生效：

1. 若当前 active skill 的 `allow_subagents=false`，则它不能调用 `call_skill`
2. 若子 skill 被成功激活，但其自身 `allow_subagents=false`，则它不能继续向下委派

这样权限模型是单调收紧的，容易推理。

### 8.4 关于 async subagent tools

第一版如果只想支持 `call_skill` 的多层树，而不想支持更复杂的异步任务编排，应明确：

- 被 `call_skill` 拉起的子 skill 可以继续 `call_skill`
- 但不自动开放 `spawn_subagent` / `get_subagent_result` / `cancel_subagent` / `list_subagent_jobs`

否则系统会从“skill 委派”演化为“多层 skill + 异步子任务图”，复杂度会陡增。

## 9. `call_skill` 的接口建议

### 9.1 不推荐长期依赖 slash command 字符串拼接

当前实现通过构造：

```text
/skill_name args
```

来激活 skill，这种方式可以作为过渡方案，但不适合作为长期内部接口。

问题包括：

- 内部能力耦合用户态 slash command 语法
- 不利于结构化参数扩展
- 不利于携带 lineage、budget、restriction 等继承信息

### 9.2 推荐内部能力

建议为运行时提供显式的 skill 激活 API，例如：

```rust
pub struct NestedSkillRequest {
    pub skill_name: String,
    pub args: Option<String>,
    pub input_summary: String,
    pub inherited_call_context: SkillCallContext,
    pub inherited_budget: SkillBudget,
}
```

然后在内部：

1. 查 registry 拿到目标 `SkillDef`
2. 计算 lineage / depth / total calls
3. 做防环和 budget 校验
4. 根据 callee 定义和父 skill 可见工具计算 `effective_child_tools`
5. 启动子 session
6. 在子 session 中激活目标 skill

slash command 仍然给用户保留，但 skill-to-skill 最好走结构化路径。

## 10. 观测与调试

多层 skill 调用如果缺少可观测性，线上排查会很困难。

### 10.1 tracing 字段建议

每次 skill 调用至少打出：

- `root_session_id`
- `parent_session_id`
- `sub_session_id`
- `target_skill`
- `depth`
- `lineage`
- `remaining_steps`
- `remaining_timeout_sec`

### 10.2 事件日志建议

在 event log 中增加结构化事件：

- `skill_call_requested`
- `skill_call_denied_cycle`
- `skill_call_denied_depth`
- `skill_call_denied_budget`
- `skill_call_started`
- `skill_call_finished`

这样在查看 transcript / event log 时可以快速定位问题。

### 10.3 LLM 可感知的失败契约

仅把失败写入 tracing 或 event log 还不够。对于多层 skill 调用，budget 不足、缺失工具、cycle detected、depth exceeded、timeout 等失败，必须同时对上层 LLM 可见。

换句话说，`call_skill` 的失败需要满足双通道要求：

- 系统侧可观测：能在 tracing / transcript / event log 中被准确定位
- 模型侧可感知：能通过结构化 tool result 返回给父 skill，使父 LLM 知道发生了什么，并据此调整计划

推荐失败结果至少包含以下字段：

```rust
pub struct SkillCallFailure {
    pub kind: SkillCallFailureKind,
    pub message: String,
    pub retryable: bool,
    pub llm_action_hint: Option<String>,
    pub details: serde_json::Value,
}

pub enum SkillCallFailureKind {
    MissingTools,
    BudgetExceeded,
    Timeout,
    CycleDetected,
    DepthExceeded,
    PolicyDenied,
    ChildExecutionFailed,
}
```

其中：

- `kind` 用于让父 skill 快速区分失败类别
- `retryable` 用于避免 LLM 对不可恢复错误盲目重试
- `llm_action_hint` 用于给模型明确的下一步建议，例如“不要重试同一 child skill，请改为总结约束或请求用户帮助”
- `details` 用于附带缺失工具列表、lineage、剩余 budget 快照、子 skill 名称等结构化上下文

### 10.4 失败结果的设计原则

对于 `call_skill`：

- 不应只返回一段模糊文本错误
- 不应让子 skill 的系统级失败以 panic 方式直接中断父 skill
- 应返回一个结构化 envelope，其中 tool 调用完成返回，但结果中清楚表达 `ok=false` 与失败原因

这样父 LLM 才能在当前轮次就感知：

- 这次调用是被策略拒绝，还是执行时失败
- 失败是否可重试
- 如果不可重试，应该换策略、降级、总结，还是请求用户介入

这是多层 skill 调用可用性的核心要求，而不只是调试体验优化。

## 11. 分阶段落地建议

### Phase 1：安全的多层 Skill Call Tree

第一阶段只做：

1. 允许 `A -> B -> C`
2. 按 ancestry 中是否出现同名 skill 判环
3. 增加最大深度限制
4. 增加总调用数限制
5. 增加 step / timeout 继承裁剪
6. 子 skill 工具集按交集计算
7. 被 `call_skill` 拉起的子 skill 可以继续 `call_skill`
8. 对 budget 不足、缺失工具、判环、深度超限、timeout 等失败提供统一的结构化 failure taxonomy，并同时写入系统日志与 tool result

这是最值得优先落地的版本。

### Phase 2：更复杂的受控能力图

只有当确实出现业务需求时，再考虑：

- 允许部分 skill 受控重入
- 允许某些白名单边回到先前 skill
- 把 skill 调用图从树扩展为受控 DAG

但这不应成为第一版目标。

## 12. 测试建议

至少补齐以下测试：

1. `A -> B -> C` 成功执行
2. `A -> A` 被拒绝
3. `A -> B -> A` 被拒绝
4. 超过最大深度被拒绝
5. 超过总 skill 调用数被拒绝
6. 子 skill 拿到的 `max_steps` 不超过父 skill 剩余 budget
7. 子 skill 拿到的 timeout 不超过父 skill 剩余 timeout
8. `allow_subagents=false` 时不能继续向下调用
9. 子 skill 的工具集等于 `callee ∩ parent ∩ runtime_policy`
10. 错误信息包含完整 lineage，便于调试
11. 当子 skill 缺失父上下文未授权的必需工具时，`call_skill` 直接 fail-fast
12. 子 skill 的 timeout / max steps / cycle detected 等失败会以结构化结果返回父 skill，而不是中断父 skill 整体执行
13. 结构化失败结果包含 `kind`、`retryable`、`details` 等字段，足以让父 LLM 感知失败类型并调整策略
14. 同一次失败既能在 event log 中检索到，也能在父 skill 的 tool result 中读到相同语义的失败信息

## 13. 对当前实现的具体影响

如果未来决定支持多层 skill 调用，建议按以下方向改造：

1. 在 `ToolContext` 或 sub-session build 参数中加入 `SkillCallContext`
2. 在 `CallSkillTool::execute()` 中增加 lineage / depth / budget admission checks
3. 让 `CallSkillTool` 在调用前读取目标 `SkillDef`，而不是依赖 caller 提供 `allowed_tools`
4. 调整 `filter_subagent_tools()`，让它支持“允许继续 `call_skill`，但受 lineage 与 budget 约束”
5. 为 `SkillRuntime` 或 session context 提供当前 active skill 和 ancestry 的只读视图
6. 为整棵 skill 调用树提供共享的 `total_skill_calls` 状态，而不是沿继承链按值传递
7. 让 `call_skill` 对“子 skill 必需工具缺失”执行 fail-fast 校验，并返回明确拒绝原因
8. 让子 skill 的系统级失败通过结构化 tool result 向上返回，而不是 panic 或强制中断父 skill
9. 为 `call_skill` 定义统一的 failure taxonomy 和结构化返回体，避免 budget / tool / policy / execution 类失败各自散落成不同字符串
10. 在 tracing、event log 和 tool result 三处复用同一份 failure kind / failure details 语义，避免系统观测结果与 LLM 可见结果不一致

## 14. 最终建议

如果要支持多层 skill 调用，推荐的第一版方案是：

- 使用**有界 skill 调用树**模型
- 使用**按 ancestry 判环**的保守策略
- 使用**深度限制 + 总调用数限制 + 预算继承**三重保险
- 使用**子 skill 工具交集 + 缺失必需工具时 fail-fast**，而不是 caller 指定工具集
- 对 budget / tools / policy / execution 失败建立**统一结构化 failure taxonomy**，同时满足系统可观测性和 LLM 可感知性
- 继续复用当前 `subagent + SkillRuntime` 架构

这套方案的优点是：

- 实现复杂度可控
- 调试边界清晰
- 行为容易解释
- 能覆盖最常见的 skill 分层委派需求

它不追求第一天就支持通用递归，而是先把最危险的问题：环、失控 budget、权限膨胀，全部压住。

## 15. 评审反馈与优化建议 (Review Feedback)

基于上述设计方案，以下是进一步完善架构状态管理、权限边界和错误处理的评审意见。它们并不需要全部以同样优先级进入实现，因此这里明确区分：哪些应纳入第一版（V1），哪些更适合作为后续优化。

### 15.1 V1 必须采纳的建议

1. **`total_skill_calls` 的全局状态同步**
   - **问题**：如果在向下的继承链中按值传递 `total_skill_calls`，兄弟节点之间的调用计数将无法同步（例如 A 调 B，B 返回后 A 调 C，C 无法感知 B 消耗的计数）。
   - **建议**：`total_skill_calls` 必须是整棵调用树共享的状态。在 Rust 实现中，建议将其设计为跨层共享的引用（如 `Arc<AtomicUsize>`），或者统一托管在 `RootSession` / `AgentLoop` 的核心状态中。每次 `call_skill` 发起前，都向根节点申请/校验全局计数。
   - **结论**：这是 V1 的必要条件；否则“总 skill 调用数限制”只对单条分支有效，无法真正约束整棵调用树。

2. **工具集交集时的“Fail-Fast”策略**
   - **问题**：`effective_child_tools = callee_declared_tools ∩ parent_visible_tools` 在安全上是正确的，但如果子 skill 强依赖工具 `X`（如 `search_web`），而父 skill 没有该权限，静默取交集会导致子 skill 在缺失必要工具的情况下启动，进而引发 LLM 幻觉、空转或重试死循环。
   - **建议**：采用严格校验模式。在 `call_skill` 初始化时，如果发现子 skill 声明的必需工具不在父 skill 当前可见工具集中，应直接拒绝调用，并返回明确错误，例如：`"Denied skill call: child requires tools missing in parent context: [search_web]"`。
   - **补充语义**：在 V1 中，若尚未引入“必需工具 / 可选工具”区分，则应将 `callee_declared_tools` 视为该 skill 的必需能力声明。
   - **结论**：这是 V1 应直接采纳的防退化策略。

3. **子 skill 失败的结构化向上传递**
   - **问题**：当子 skill 发生 Timeout、Max Steps Exceeded、cycle detected 或其他系统级失败时，若直接 panic 或中断父 skill，会破坏多层委派的可恢复性。
   - **建议**：子 skill 的系统级失败应转化为父 skill 可见的结构化 tool result，而不是让父 skill 直接崩溃。推荐方式是：tool 调用本身完成返回，但 envelope 中 `ok=false`，并提供结构化失败原因；父 skill 可以据此决定重试、改走其他路径或总结失败。
   - **补充要求**：结构化结果不应只是为了日志或开发者调试而存在；它必须足够让父 LLM 在当前轮次理解失败类型、可重试性以及推荐动作。
   - **结论**：V1 不应把子 skill 失败设计成 panic 传播；必须允许父 skill 继续决策。

4. **`args_digest` 的语义明确**
   - **问题**：文档中明确第一版不使用参数判环，但 `SkillCallFrame` 中包含了 `args_digest` 字段，容易造成误解。
   - **建议**：在代码注释和设计文档中明确标注其用途，例如：`// For observability and event logging only in V1; not used for cycle detection.`
   - **结论**：这是低成本但必要的语义澄清，应直接纳入 V1 文档与实现说明。

### 15.2 后续优化建议（不阻塞 V1）

1. **预算回收机制 (Budget Reclamation)**
   - **问题**：当前预算向下裁剪策略（如 `parent_remaining_steps / 2`）解决了预算借出问题，但没有定义子 skill 提前完成时未使用 budget 的去向。
   - **建议**：当子 skill 正常或异常结束时，将其未消耗完的步骤数和时间加回父 skill 的可用预算。
   - **原因**：这项优化可以显著提升多层调用下的整体预算利用率，但它会引入更复杂的“借出 / 消耗 / 回收”状态跟踪，不是 V1 闭环所必需。
   - **结论**：建议作为 V1.5 或 V2 优化项推进，而不是阻塞第一版上线。

### 15.3 本节收口结论

综合评审后，推荐对 V1 的要求收口为：

- 全局同步 `total_skill_calls`
- 子 skill 工具交集 + 缺失必需工具 fail-fast
- 子 skill 失败以结构化结果向父 skill 返回，并对父 LLM 清晰可感知
- 明确 `args_digest` 仅用于观测与事件日志，不参与 V1 判环

而预算回收机制可以在第一版稳定后再引入，以避免在 V1 同时处理过多状态机复杂度。
