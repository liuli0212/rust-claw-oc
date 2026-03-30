# Subagent & SKILLs 实现 Review

Review 日期: 2026-03-30

---

## 一、功能完整性问题

### [BUG-1] 同步 subagent 的 `allow_writes`/`claimed_paths` 被静默忽略

**文件**: `src/tools/subagent.rs:98-112`

**问题描述**:
`DispatchSubagentArgs` 包含 `allow_writes` 和 `claimed_paths` 字段，但同步路径 `DispatchSubagentTool` 始终传入
`SubagentBuildMode::SyncCompatible`，该模式完全跳过 `FORBIDDEN_ASYNC_SUBAGENT_TOOLS` 检查。
异步路径对 `allow_writes=true` 且 `claimed_paths` 为空的情况有明确校验，同步路径没有。
调用方传入这两个字段不会产生任何效果，也不会报错，行为误导。

**建议修复**:
方案 A（推荐）：保持 sync/async 语义分离。在 `DispatchSubagentTool::execute` 中，如果
`allow_writes=true` 或 `claimed_paths` 非空，则直接返回参数错误或执行错误，明确这两个字段仅适用于
background subagent。

方案 B：如果产品确实需要“同步但受控写入”的 subagent，应单独设计 sync 路径的权限模型和校验逻辑，
不要直接复用 `AsyncControlledWrite`。后者是为后台 subagent 设计的安全模型，直接套到同步路径上可能改变
现有同步行为，不只是“补齐写权限控制”。

核心目标应是消除“参数看起来可用但实际无效”的 API 歧义，而不是悄悄把同步语义切换成异步策略。

---

### [BUG-2] `allow_subagents: false` 约束从未被执行

**文件**: `src/skills/runtime.rs:241-258`, `src/skills/definition.rs:65`

**问题描述**:
`SkillConstraints.allow_subagents` 字段被解析存储，但 `SkillRuntime::before_tool_resolution`
只检查 `forbid_code_write`，从不过滤 `dispatch_subagent`/`spawn_subagent` 等工具。
设置了 `allow_subagents: false` 的 skill 仍然可以让 LLM 自由调用 subagent 工具。

**建议修复**:
在 `before_tool_resolution` 的 `filtered.retain(...)` 之后，如果 `!state.constraints.allow_subagents`，
追加过滤逻辑，并在过滤发生时记录 `tracing::warn!`，避免该约束被静默触发：

```rust
if !state.constraints.allow_subagents {
    let subagent_tools = ["dispatch_subagent", "spawn_subagent",
                          "get_subagent_result", "cancel_subagent", "list_subagent_jobs"];
    filtered.retain(|t| !subagent_tools.contains(&t.name().as_str()));
    tracing::warn!(
        skill = %state.skill_name,
        "Skill constraint allow_subagents=false: filtered subagent tools from tool set"
    );
}
```

---

### [BUG-3] Skill 在 `finish_task` 后从不被 deactivate

**文件**: `src/skills/runtime.rs:74-85`, `src/core.rs:674-691`

**问题描述**:
`deactivate_skill()` 被标记为 `#[allow(dead_code)]`，从未被调用。
当 `before_finish` 返回 `Allow`、`finalize_finished_run` 执行后，skill 仍维持 active 状态。
同一 session 的下一轮请求（如用户继续对话）仍会被注入旧 skill 的 contract/instructions，
导致 LLM 行为混乱。

**建议修复**:
不建议在 `before_finish` 中直接清空 skill 状态，因为 `before_finish` 是校验阶段，后续仍可能有其他 extension
返回 `Deny`。如果在这里提前做副作用，可能出现“任务未真正 finish，但 skill 已被清空”的状态不一致。

更合理的修复方式是在 `core.rs` 中，当所有 extension 都允许 finish、且真正准备执行
`finalize_finished_run` 时，再显式触发 skill cleanup。可选方案：

- 新增一个“finish 已被最终接受”的 extension hook，例如 `after_finish_accepted`
- 或在 finish commit 路径中新增专门的 cleanup 调用点，由 `SkillRuntime` 在该阶段清理状态

总之，deactivate 应发生在 commit 阶段，而不是 validation 阶段。

---

### [BUG-4] `require_question_resume` 约束是死代码

**文件**: `src/skills/definition.rs:68`, `src/skills/runtime.rs` 全文

**问题描述**:
`SkillConstraints.require_question_resume` 被解析和存储，但 `SkillRuntime` 任何 hook
中均未读取。当该字段为 `true` 时，普通的用户输入不会被强制走 `ask_user_question` → resume 流程。

**建议修复**:
先定义清楚该字段的产品语义，再实现运行时约束。当前至少有两种可能解释：

- 解释 A：用户只能回答此前由 `ask_user_question` 发起的结构化问题
- 解释 B：skill 不允许接收自由文本，所有用户输入都必须先通过结构化提问流程采集

这两种语义对应的实现位置和拦截策略不同，不能直接按其中一种写死。建议先在设计上明确该字段含义，
再补 `before_turn_start` / `on_user_resume` 的约束逻辑。

---

### [INCOMPLETE-1] 多个 enum 值和字段是纯设计草图，从未被实现

**文件**: `src/skills/definition.rs`, `src/skills/state.rs`

**未实现的项目**:
- `SkillExecutionState::WaitingSubagent` — 无任何代码设置此状态
- `SkillExecutionState::ValidatingArtifacts` — 同上
- `SkillTrigger::SuggestOnly` / `ManualOrSuggested` — 无 suggestion 机制入口
- `SkillMeta::output_mode` (`Freeform`/`DesignDocOnly`/`ReviewOnly`) — 解析但不强制
- `ActiveSkillState::labels: BTreeMap<String, String>` — 无任何代码写入

**建议**:
短期内用 `#[allow(dead_code)]` + 注释标注哪些是"预留/计划实现"，
避免代码读者误以为这些字段正在被使用。中期按需实现或删除。

---

### [CLEANUP-1] `parameters` 字段在 `SkillMeta` 和 `SkillDef` 中重复

**文件**: `src/skills/parser.rs:69-97`

**问题描述**:
Parser 将同一 YAML `parameters` 值分别写入 `SkillMeta.parameters` 和 `SkillDef.parameters`，
两者完全相同，冗余。

**建议修复**:
选择一个位置保留（建议 `SkillDef.parameters`），删除另一个，
或明确区分两者语义（如 meta 放 schema，def 放 defaults）。

---

## 二、Observability 问题

### [OBS-1] 同步 subagent 的 session 无从追踪

**文件**: `src/tools/subagent.rs:38-44`, `src/session/factory.rs:306-379`

**问题描述**:
同步路径在 `build_subagent_session` 内生成了一个 `sub_{parent}_{uuid}` 的 session ID，
创建了 transcript 和 event_log 文件，但返回的 `SubagentResult` 结构体不含任何 session ID
或文件路径字段。subagent 运行失败时，人类完全无法定位对应的日志文件。

异步路径（`SubagentJobSnapshot.meta`）暴露了 `transcript_path` 和 `event_log_path`，
是正确的做法。

**建议修复**:
在 `SubagentResult` 中增加字段：

```rust
pub struct SubagentResult {
    pub ok: bool,
    pub summary: String,
    pub findings: Vec<String>,
    pub artifacts: Vec<String>,
    pub sub_session_id: Option<String>,  // 新增
}
```

并在 `DispatchSubagentTool::execute` 中填充该字段（`built.agent_loop.session_id` 或
从 `build_subagent_session` 的返回值中取出 sub_session_id）。

---

### [OBS-2] 同步 subagent 执行缺少 tracing span

**文件**: `src/tools/subagent.rs:88-93`, `src/subagent_runtime.rs:321-326`

**问题描述**:
异步路径用 `tracing::info_span!("subagent_run", job_id=..., parent_session_id=..., sub_session_id=...)`
包裹整个 task 执行，使 span 关联的所有日志都携带这些 field，便于 log aggregator 过滤。
同步路径只有两个孤立的 `info!` 日志，无法关联过滤。

**建议修复**:
在 `DispatchSubagentTool::execute` 中用 span 包裹执行：

```rust
let span = tracing::info_span!(
    "subagent_run_sync",
    parent_session_id = %ctx.session_id,
    sub_session_id = %sub_session_id,
    goal = %goal
);
let run_result = tokio::time::timeout(..., async move { ... }).instrument(span).await;
```

---

### [OBS-4] Skill 状态转换缺少 tracing

**文件**: `src/skills/runtime.rs:295-318`

**问题描述**:
`on_user_resume` 将状态从 `WaitingUser` 切换回 `Running` 时无 tracing 日志。
调试 skill 卡在等待状态时，无法从日志中判断 resume 是否被触发。

**建议修复**:
在状态转换后添加：

```rust
tracing::info!(
    skill = %s.skill_name,
    context_key = %pi.context_key,
    "Skill resumed from WaitingUser → Running"
);
```

---

### [OBS-5] `CollectorOutput::on_llm_request` 信息过于粗糙

**文件**: `src/session/factory.rs:221-227`

**问题描述**:
记录的摘要仅为 `"System + N messages"`，不含 token 估算、工具数量等信息。
无法从 debug snapshot 判断 subagent 是否因 context 过满导致异常行为。

**建议修复**:
扩展 `prompt_summary`，包含工具数量和粗略 token 估算：

```rust
async fn on_llm_request(&self, prompt_summary: &str) {
    // 在 collect_stream_response 中调用时传入更多信息：
    // format!("System + {} messages, {} tools, ~{}k chars", msg_count, tool_count, char_count / 1000)
}
```

---

## 优先级汇总

| ID | 优先级 | 类型 | 修复复杂度 |
|----|--------|------|-----------|
| BUG-1 | 🔴 High | 功能 | 低 |
| BUG-2 | 🔴 High | 功能 | 低 |
| BUG-3 | 🔴 High | 功能 | 低 |
| OBS-1 | 🔴 High | Observability | 低 |
| BUG-4 | 🟡 Med | 功能 | 中（需先定义语义） |
| OBS-2 | 🟡 Med | Observability | 低 |
| OBS-4 | 🟡 Med | Observability | 低 |
| INCOMPLETE-1 | 🟡 Med | 代码卫生 | 低（标注） |
| OBS-5 | 🟢 Low | Observability | 低 |
| CLEANUP-1 | 🟢 Low | 代码卫生 | 低 |
