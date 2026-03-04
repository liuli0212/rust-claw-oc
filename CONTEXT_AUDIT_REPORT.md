# Rusty-Claw Context 管理深度评审报告 v2

> **评审日期**: 2026-03-04  
> **分析范围**: `context.rs` (1224行)、`core.rs` (421行)、`main.rs` (400行)、`llm_client.rs` (978行)、`context-profiler/` (Python 子项目)  
> **对比基线**: v1 报告 (2026-03-02)、GitHub 最新拉取 (d996e59)

---

## 0. 变更概述（v1 → v2）

自上次评审以来，代码经历了**重大重构**：

| 变更项 | 旧 (v1) | 新 (v2) | 评价 |
|--------|---------|---------|------|
| `core.rs` 行数 | ~650 行 | **421 行** | ✅ 大幅精简 |
| RAG 调用方式 | 内嵌在迭代循环中 | **RAG 变成独立 Tool** (由 Agent 决定调用) | ✅ 解决了 v1 的 P0-2.1 |
| Compaction | 独立 LLM 调用做摘要 | **空操作 stub** (只打日志) | ⚠️ 功能退化 |
| `RunExit` 枚举 | `CompletedWithReply`, `HardStop{reason}` 等 | `Finished(String)`, `StoppedByUser` 等 | ✅ 更清晰 |
| `end_turn()` 覆盖 | 多个退出路径遗漏 | **所有路径已覆盖** | ✅ 解决了 v1 的 P0-2.16 |
| `/context` 命令 | 无 | `audit`/`diff`/`inspect`/`dump`/`compact` | ✅ 新能力 |
| Context Profiler | 无 | Python 子项目 (claw_audit.py + engine) | ✅ 新工具 |

---

## 1. Rust 侧 Context 管理评审

### 🔴 P0：关键问题（立即关注）

#### 1.1 `maybe_compact_history` 实质上是空操作

- **位置**: [`core.rs:137-150`](file:///Users/liuli/src/rust-claw-oc/src/core.rs#L137-L150)
- **现象**: 函数体只做了两次 `get_context_status()` 并打日志，**没有任何实际压缩逻辑**。注释写着 "we don't need to do explicit compaction here unless we implement summarization. For now, we just log."
- **影响**: 当历史 Token 超过 85% 阈值后，系统除了日志中记一笔以外什么都不做。长会话场景下，`build_history_with_budget` 的 `break` 会**静默丢弃**最早的历史轮次——未经摘要就直接消失，用户和 Agent 完全无感知。
- **v1 对比**: v1 版本至少有一个调用 LLM 做摘要的实现（虽然只用 `user_message` 做输入），现在连那个都被删掉了。
- **修复方向**: 要么恢复 LLM-based 摘要（并改进输入内容），要么实现一个**无 LLM 的规则化压缩**（例如把最早 N 轮的工具输出全部替换为 `[Compacted: user asked X → tool Y returned success/failure]` 格式的摘要行）。

#### 1.2 `get_context_status` 重复调用 `build_history_with_budget` 造成性能浪费

- **位置**: [`context.rs:841-872`](file:///Users/liuli/src/rust-claw-oc/src/context.rs#L841-L872)
- **调用链**: `maybe_compact_history()` → 调用 `get_context_status()` **两次** (L138 和 L146)；每次 `get_context_status` 内部都调用 `build_history_with_budget()`，而后者是 O(n) 的全量 Tokenize 操作。
- **额外浪费**: `build_llm_payload()` 也调用 `build_history_with_budget()`，且 `PromptReport` 中还内嵌了一次 `get_detailed_stats(None)`（L1078），而 `get_detailed_stats` 内部又调一次 `build_history_with_budget()`。**一次 `step()` 调用可能触发 4-6 次完整的历史 Tokenize 扫描。**
- **修复方向**: 将 `build_history_with_budget` 的结果缓存到 Turn 级别（invalidate on `add_message_to_current_turn`），或至少在 `build_llm_payload` 中复用已算好的值。

#### 1.3 `build_system_prompt` 与 `get_detailed_stats` 仍然是两套独立逻辑 (DRY 违规)

- **位置**: [`context.rs:147-244`](file:///Users/liuli/src/rust-claw-oc/src/context.rs#L147-L244) vs [`context.rs:370-456`](file:///Users/liuli/src/rust-claw-oc/src/context.rs#L370-L456)
- **验证**: 此问题与 v1 的 2.4 完全一致，**未被修复**。两个函数各自独立读取 `.claw_prompt.md`、`.rusty_claw_task_plan.json`、`AGENTS.md`、`README.md`、`MEMORY.md`。
- **精度差异举例**:
  - `get_detailed_stats.system_task_plan` 用**原始 JSON 字符串**算 Token (L176)。
  - `build_system_prompt` 用**格式化后的 Checklist** 算 Token (L401-413)。
  - 两者差异可达 **30-50%**。
- **`/context` 命令影响**: `/context` 默认输出调用的是 `get_detailed_stats`，而 `/context dump` 调用 `build_llm_payload` 内部走的是 `build_system_prompt`。用户看到的数字和实际发送的完全不一样。

---

### 🟠 P1：设计缺陷（应尽快修复）

#### 1.4 Task Plan 指令重复注入

- **位置**: [`context.rs:398-418`](file:///Users/liuli/src/rust-claw-oc/src/context.rs#L398-L418) 和 [`context.rs:422-427`](file:///Users/liuli/src/rust-claw-oc/src/context.rs#L422-L427)
- **现象**: `build_system_prompt` 中：
  - L398-418：从 `.rusty_claw_task_plan.json` 读取 Task Plan 并生成 `## Current Task Plan (STRICT)` Section。
  - L422-427：在 `## Project Context` 中**又**注入了一段 "### CRITICAL INSTRUCTION: Task Planning" 指令。
- **影响**: Agent 同时收到两段语义重叠但措辞不同的 "你必须遵循计划" 指令，浪费 ~200-400 Token，且可能导致 LLM 在两段冲突措辞间犹豫。
- **修复方向**: 将 L422-427 的 Task Planning 指令**合并**到 `Current Task Plan (STRICT)` Section 中，或者当 Plan 文件存在时跳过 Project Context 中的 Planning 段落。

#### 1.5 `is_user_referencing_history` 仍然不支持中文

- **位置**: [`context.rs:582-589`](file:///Users/liuli/src/rust-claw-oc/src/context.rs#L582-L589)
- **验证**: v1 的 2.13 **未被修复**。关键词列表仍然全是英文。
- **影响**: 中文用户说 "修复上次的错误"、"看看之前的输出" 无法触发 `protect_next_turn`。

#### 1.6 `reconstruct_turn_for_history` 中 model 文本在有 tool call 时被完全丢弃

- **位置**: [`context.rs:740-748`](file:///Users/liuli/src/rust-claw-oc/src/context.rs#L740-L748)
- **验证**: v1 的 2.6 **未被修复**。`if new_part.function_call.is_none()` 条件导致 model 的推理分析被跳过。

#### 1.7 `estimate_context_window` 模型覆盖不全

- **位置**: `llm_client.rs:33-50`
- **验证**: v1 的 2.3 **未被修复**。你在之前的 diff 中手动回退了我的修改（`"claude"` → 回退为 `"claude-3-5" || "claude-3-opus"`）。

#### 1.8 Focus Booster 注入策略在 `history_turns_included >= 1` 时就触发

- **位置**: [`context.rs:1030-1046`](file:///Users/liuli/src/rust-claw-oc/src/context.rs#L1030-L1046)
- **现象**: `if history_turns_included >= 1` 意味着只要有 **1 轮**历史，就会在用户消息前注入 `[SYSTEM NOTE...]`。第一轮对话就会注入 `"FOCUS ON THIS NEW USER MESSAGE. Context above is history."`，但此时上下文极少，完全没有 "需要聚焦" 的必要。
- **阈值反直觉**: `history > 20` 用的是最强的 `IGNORE PREVIOUS CONTEXT IF CONFLICTING`，而 `> 10` 用的是较弱的版本。逻辑上应该是历史越多越需要强化，但目前最弱版本反而在 1-10 轮时就注入了无意义的噪声 Token。
- **修复方向**: 将阈值改为 `>= 5` 开始注入，且可以考虑在注入后的**历史重建时清除**这些 marker（`reconstruct_turn_for_history` L728-737 已经做了，但只对旧轮有效）。

---

### 🟡 P2：可维护性与优化

#### 1.9 Identity 硬编码 "Gemini 3.1 Pro"

- **位置**: [`context.rs:124`](file:///Users/liuli/src/rust-claw-oc/src/context.rs#L124)
- **验证**: v1 的 2.9 **未被修复**。

#### 1.10 `truncate_chars` 不是 pub 但被 v1 报告建议在 core.rs 中调用

- **位置**: [`context.rs:354`](file:///Users/liuli/src/rust-claw-oc/src/context.rs#L354)
- **现状**: 函数是 `fn`（私有），v1 报告中建议在 `core.rs` 的 Compaction 逻辑中调用 `crate::context::AgentContext::truncate_chars()`，但这在当前版本中不可行（Compaction 已被 stub 化），且可见性不匹配。保持现状即可。

#### 1.11 `/context dump` 未包含 `detailed_stats` 的分项数据

- **位置**: [`main.rs:323-346`](file:///Users/liuli/src/rust-claw-oc/src/main.rs#L323-L346)
- **现象**: Dump 的 JSON 中 `report` 只包含了 `PromptReport` 的顶层字段，**没有** `detailed_stats` 的分项（如 `system_static`, `system_runtime`, `system_project` 等）。
- **影响**: Context Profiler 无法从 dump 文件中获取 System Prompt 各组件的 Token 分布，只能看到一个总的 `system_prompt_tokens`。这严重限制了 Profiler 对 System Prompt 臃肿问题的诊断能力。
- **修复方向**: 在 dump JSON 中加入 `detailed_stats` 字段。

---

## 2. Context Profiler (Python) 评审

### 整体架构

Profiler 目前有**两套并存的代码**：

| 文件 | 功能 | 状态 |
|------|------|------|
| `claw_audit.py` (288行) | 原始独立脚本，含完整 Audit + Diff + Rich 面板 | 功能完整但未模块化 |
| `src/context_profiler/` | 模块化重构版，engine.py + models.py + rules/ | 架构更好但**规则系统断裂** |

### 🔴 P0：Profiler 核心缺陷

#### 2.1 两套数据模型不兼容，Rule 系统无法运行

- **问题**: `rules/base.py` 引用的是 `ContextMessage` 和 `OptimizationSuggestion`，但 `models.py` 中定义的是 `TurnStats` 和 `AuditReport`。`ContextMessage` 和 `OptimizationSuggestion` **根本不存在于 models.py 中**。
- **consequence**: `from ..models import ContextMessage, OptimizationSuggestion` 会直接 `ImportError`。`GrepBloatRule` 无法实例化。
- **验证**: `engine.py` 的 `run_audit()` 方法中**完全没有调用任何 Rule**。Rule 系统是一个与主分析引擎断裂的"架子"。
- **修复方向**: 
  1. 在 `models.py` 中补充 `ContextMessage`、`MessageRole`、`OptimizationSuggestion` 的定义；
  2. 在 `engine.py` 中增加 `_run_rules()` 步骤，将 `self.messages` 转换为 `List[ContextMessage]` 后传入各 Rule。

#### 2.2 Profiler 无法检测"关键历史信息丢失"

- **对比设计目标**: "分析出context有没有缺重要的历史信息"。
- **现状**: `_check_completeness()` 只检查了两件事：
  1. 最后 3 轮有没有 `user` 消息。
  2. System Prompt 是否为空。
- **缺失的关键检测**:
  - **因果链断裂检测**: 如果 Agent 正在修 Bug（最新 Turn 包含 `patch_file` 调用），但上下文中找不到任何 `error`/`failed`/`panic` 的历史记录，说明错误日志在压缩中丢失了。
  - **目标一致性检测**: 当前 System Prompt 中的 Task Plan 步骤与历史中实际执行的工具调用是否匹配。
  - **历史深度检测**: 如果 `report.history_turns_included` 明显小于实际对话轮数，说明大量历史被静默丢弃了。
  - **工具输出完整性**: 检测历史中是否存在 `[History Compressed]` 或 `[History: Content stripped]` 标记，并评估被压缩的内容是否仍然被后续轮次引用。

#### 2.3 Profiler 对 System Prompt 的分析为零

- **对比设计目标**: "有没有放了太多不需要的信息在里面"。
- **现状**: `engine.py` 将 `system_prompt` 存为一个变量，但在整个流程中**从未分析它**。唯一的引用是 `_check_completeness` 中检查它是否为空。
- **缺失的关键分析**:
  - System Prompt 总 Token 数占全部 Context 的比例（如果 > 40% 应报警）。
  - System Prompt 内部各 Section（Identity / Runtime / Custom / TaskPlan / Project / RAG Memory）的 Token 分布。
  - 检测 `AGENTS.md` / `README.md` / `MEMORY.md` 等项目文件在 System Prompt 中的实际 Token 贡献（目前 dump JSON 中未提供分项数据，参见 1.11）。

---

### 🟠 P1：Profiler 设计改进

#### 2.4 冗余检测缺乏"可操作建议"

- **现状**: `_check_redundancy()` 标记 `is_redundant = True` 并给出 `redundancy_reason`（如 "High Volume (3500)"），但**没有告诉用户该怎么改**。
- **对比设计目标**: "给出明确问题和改进意见"。
- **改进方向**: 每条 Redundancy 结果应包含：
  - 具体工具名和参数（如 "Turn 7: `read_file(/src/context.rs)` 产生了 3500 tokens"）。
  - 可操作建议（如 "建议在 Rust 侧为 `read_file` 增加行数限制，或在 `strip_response_payload` 中对超过 100 行的文件只保留首尾 10 行"）。

#### 2.5 Token Timeline 缺乏"累积视图"

- **现状**: Timeline 视图显示的是每条 Message 的独立 Token 数。
- **缺失**: 无法看到 Token 随轮次的**累积增长趋势**。一个好的 Profiler 应该能展示：
  - 哪些轮次导致了 Token 使用的陡增（"爆发点"）。
  - 累积 Token 何时逼近预算上限（"警戒线"）。

#### 2.6 `claw_audit.py` 与 `src/context_profiler/` 入口重复

- **现状**: `claw_audit.py`（独立脚本）和 `src/context_profiler/main.py`（模块化入口）功能高度重叠，容易让用户困惑该运行哪一个。README 指向 `claw_audit.py`，但 `run_profiler.py` 指向 `src/context_profiler/main.py`。
- **修复方向**: 统一入口，将 `claw_audit.py` 标记为 deprecated 或直接删除。

---

## 3. 设计目标对标矩阵

基于用户提出的设计目标，逐项对标现有实现的覆盖度：

| 设计目标 | Rust 侧 (context.rs) | Profiler (Python) | 覆盖度 | 差距 |
|----------|----------------------|-------------------|--------|------|
| **分析 context 内容是否合理** | `/context audit` 展示了 Token 分项 | Timeline + Table 面板 | 🟡 60% | 缺少 System Prompt 分项分析；缺少"合理性评分" |
| **有没有缺重要的历史信息** | `build_history_with_budget` 会静默丢弃溢出轮次 | `_check_completeness` 只查 user msg 存在性 | 🔴 20% | 缺少因果链断裂检测、错误日志湮灭检测、历史深度预警 |
| **有没有放了太多不需要的信息** | `strip_response_payload` + `truncate_old_tool_results` 做了工具输出压缩 | `_check_redundancy` 标记了 >2000 token 的结果 | 🟡 50% | 缺少 System Prompt 臃肿分析、RAG 相关性评估、重复文件读取检测 |
| **能给出明确问题和改进意见** | `/context audit` 只展示数字 | Redundancy 只给标签无建议 | 🔴 15% | 两侧都缺少可操作的具体建议 |
| **确保 context 是好的状态** | 无健康度评分 | 无综合评分 | 🔴 0% | 需要一个综合 Health Score |

---

## 4. 改进路线图（更新版）

### 阶段 1：补齐核心能力 (Rust 侧) — 预计 1 天

| 编号 | 改动 | 优先级 | 工作量 |
|------|------|--------|--------|
| 1.1 | 实现**无 LLM 的规则化压缩**替代空 stub | P0 | ~40 行 |
| 1.2 | 缓存 `build_history_with_budget` 结果，消除重复 Tokenize | P0 | ~30 行 |
| 1.3 | 抽取 `PromptSections` 统一 Stats 与 Prompt 逻辑 | P0 | ~80 行重构 |
| 1.4 | 合并重复 Task Plan 指令 | P1 | ~10 行 |
| 1.11 | `/context dump` 加入 `detailed_stats` 分项 | P1 | ~15 行 |

### 阶段 2：Profiler 核心规则补齐 — 预计 2 天

| 编号 | 改动 | 对标设计目标 | 工作量 |
|------|------|-------------|--------|
| 2.1 | 统一 Models，接通 Rule 系统 | 基础架构 | ~60 行 |
| 2.2a | 新增 `CausalChainRule`: 检测"修 Bug 但无错误日志" | 缺重要历史 | ~50 行 |
| 2.2b | 新增 `HistoryDepthRule`: 检测丢弃轮数过多 | 缺重要历史 | ~30 行 |
| 2.3 | 新增 `SystemPromptBloatRule`: 分析 System Prompt 各组件占比 | 不需要的信息 | ~40 行 |
| 2.4 | 每条诊断结果附带 `actionable_advice` | 给出改进意见 | ~20 行 |
| New | 新增 `HealthScoreCalculator`: 综合评分 0-100 | 确保好状态 | ~50 行 |

### 阶段 3：高级分析 — 预计 1 周

| 编号 | 改动 | 备注 |
|------|------|------|
| New | RAG 相关性评估（对比当前 Goal 与 Retrieved Memory 的关键词交集） | 依赖 dump 中包含 memory 内容 |
| New | "Lost-in-the-Middle" Canary 测试模式 | 参考 RESEARCH.md 中的建议 |
| New | 累积 Token 增长趋势图 + 预算警戒线 | Rich Panel 增强 |
| 1.5 | `is_user_referencing_history` 支持中文 | 简单但重要 |
| 1.9 | 去除硬编码模型名 | 低优先级 |

---

## 5. 衡量指标（更新版）

| 指标 | 定义 | 当前状态 | 目标 |
|------|------|----------|------|
| **Context Health Score** | Profiler 综合评分 | ❌ 未实现 | 0-100 分制 |
| **System Prompt 占比** | System Token / Total Token | 约 22-30% (未精确) | < 25% |
| **统计精度** | `get_detailed_stats.total` vs 实际 Payload | 偏差 30-50% (Task Plan + Project) | < 5% |
| **历史完整性** | 被丢弃轮次是否经过摘要 | 0% (直接丢弃) | 100% 经过摘要 |
| **Rule 覆盖度** | Profiler 活跃 Rule 数 | 0 (规则系统断裂) | ≥ 6 条活跃规则 |
| **Profiler Actionable 率** | 带具体建议的诊断结果占比 | 0% | > 80% |
| **Tokenize 调用次数** | 每 Turn 的 `build_history_with_budget` 次数 | 4-6 次 | 1 次 (有缓存) |
