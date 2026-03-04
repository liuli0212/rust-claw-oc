# Rusty-Claw Context 管理深度评审报告 v3

> **评审日期**: 2026-03-04  
> **分析范围**: `context.rs` (1300+行)、`core.rs` (421行)、`main.rs` (400行)、`llm_client.rs` (978行)、`context-profiler/` (Python 子项目)  
> **对比基线**: v2 报告 (2026-03-04_10:00)、GitHub 最新拉取 (5b8b9e4)

---

## 0. 变更概述（v2 → v3）

自上次评审以来，Rust 侧 Context 模块完成了**关键的风险修复与重构**：

| 变更项 | 旧 (v2) | 新 (v3) | 评价 |
|--------|---------|---------|------|
| `maybe_compact_history` | 空操作 stub，静默丢弃历史 | **实现 `rule_based_compact`** | ✅ P0-1.1 修复，具备安全兜底 |
| `get_detailed_stats` | 独立处理文本，与 Prompt 不一致 | **抽取 `PromptSections` 统一数据源** | ✅ P0-1.3 修复 (DRY) |
| `/context dump` | JSON 仅包含顶层聚合数据 | **包含 `detailed_stats` 分项数据** | ✅ P2-1.11 修复 |
| Task Plan 指令 | System/Project 重复注入 | **有活跃Plan时跳过 Project 中的段落** | ✅ P1-1.4 修复 |
| 历史引线触发 | 仅英文关键词 | **支持 12 个中文关键词** | ✅ P1-1.5 修复 |

---

## 1. Rust 侧 Context 管理评审 (当前状态)

经过最新一轮的重构，Rust 侧 Context 管理的**主要 P0 级设计缺陷已被修复**。历史能安全压缩，Token 统计与实际发出内容严格一致。

### 🟡 残留的性能与逻辑优化点 (P1/P2)

#### 1.2 `get_context_status` 重复调用 `build_history_with_budget` 造成性能浪费
- **现状**: 虽然本次修复在 `core.rs` 的循环中加了短路保护（每 Turn 只检查一次压缩），但在 `build_llm_payload` 时依然会做全量序列化和过滤，计算开销大。
- **目标**: 将 `build_history_with_budget` 结果做到真正的 Turn 级别缓存。

#### 1.6 `reconstruct_turn_for_history` 中 model 文本在有 tool call 时被完全丢弃
- **现状**: 如果 `new_part.function_call.is_some()`，则完全忽略 model 的文本部分（通常是 `<think>`）。
- **目标**: 历史摘要中也应该保留 Agent 为什么触发该工具的简短分析文本。

#### 1.8 Focus Booster 注入阈值过低
- **现状**: `history_turns_included >= 1` 就开始注入 "FOCUS ON THIS NEW USER MESSAGE"。
- **目标**: 提高阈值至 5 轮以上，减少多余的噪音 Token。

#### 1.9 Identity 硬编码 "Gemini 3.1 Pro"
- **现状**: 提示词身份部分仍是硬编码。

---

## 2. Context Profiler (Python) 深度评审

Rust 侧数据已经精准完备（特别是新增了 `detailed_stats` 的 Dump 输出），但 Profiler (Python 侧) 自身的设计和架构依然存在**严重缺陷**，未能达到设计目标的要求。

### 🔴 P0：Profiler 核心架构缺陷 (✅ 已修复 - 2026-03-04)

#### 2.1 数据模型不兼容，Rule 系统完全断裂未能执行 (✅ 修复)
- **问题**: `rules/base.py` 引用了 `ContextMessage` 和 `OptimizationSuggestion`，但 `models.py` 中并没有定义这些类（反而定义的是 `TurnStats` 和 `AuditReport`）。
- **结果**: `engine.py` 的 `run_audit()` 方法中**完全没有调用任何 Rule**。整个 Rule 诊断引擎是一个无法运行的空壳架子。
- **修复**: 统一了 `models.py`，打通了 `engine.py` 到 `BaseRule` 的调用链，现在 Rule 系统已完全激活并在末尾渲染了 `Optimization Suggestions`。

#### 2.2 Profiler 无法检测"关键历史信息丢失" (✅ 已实装 CausalChainRule)
- **对比设计目标**: "分析出context有没有缺重要的历史信息"。
- **现状**: 已实装 `MISSING_CAUSAL_ERROR` 规则。当监测到最近几次在进行 `patch_file` 动作时（修复 Bug 语境），如果没有在之前的 Context 中找到任何 Error/Panic 的日志，便会发出严重的级别报警。

#### 2.3 System Prompt 臃肿度深度分析缺失 (✅ 已实装 SystemPromptBloatRule)
- **对比设计目标**: "有没有放了太多不需要的信息在里面"。
- **现状**: 工具已介入读取 `detailed_stats`。当发现全局 System Prompt > 20000 Token、System Project (比如 AGENTS.md) 占比过大，或 Custom Instructions 过多时，会抛出精确的阈值警告。

### 🟠 P1：Profiler 体验与建议缺陷 (✅ 进展中)

#### 2.4 冗余检测缺乏"可操作的改进意见" (Actionable Advice) (✅ 修复)
- **现状**: 所有的 Rules 都已被要求输出 `actionable_advice` 字段。输出不仅包括 "冗长"，还告诉了具体的 Unix 管道操作指令（如建议把 `grep` 换成 `grep -l`）。此外，抛弃了在 Rich Box 里会导致布局乱码和截断的横向 `split_row`，改成了流式输出。

#### 2.5 缺乏累积趋势监控与预算警戒线
- **现状**: Timeline 视图只孤立展示单次节点的 Token 数。
- **缺失**: 用户无法直观看到历史 Token 是在哪个环节开始爆炸式增长的，需要建立随时间增长的累积趋势判定或可视化呈现。

#### 2.6 多重入口混乱
- **现状**: 主目录下老的 `claw_audit.py` 和 `src/context_profiler/main.py` 重复存在，但规则都未接通。容易让使用者迷惑到底用哪个。
- **修复方向**: 废弃旧的纯函数式脚本，完全迁移和修复新的面向对象体系 (`src/context_profiler`)。

---

## 3. 设计目标对标矩阵 (v3 状态)

此时 Rust 侧基建已打好，核心瓶颈完全转移至**分析工具 (Profiler) 本身的羸弱**。

| 设计目标 | Rust 侧提供的数据 (Dump) | Profiler 当前分析能力 | 对标进度 | 核心差距 |
|----------|--------------------------|-----------------------|----------|----------|
| **分析 context 是否合理** | 提供最精准的 Payload 与 Stats 分项 | Timeline 简单展示 | 🟡 60% | 已经打磨完初步的 Rules；目前在 Health Check 大指标和 Trend 图表部分有所缺失 |
| **有没有缺重要历史信息** | 记录了被截断的内容与标记 | **缺失因果连检出机制** | 🟢 80% | `MISSING_CAUSAL_ERROR` 已可捕捉修 bug 时失去报错文本的危险 |
| **有没有放太多没用信息** | 提供详尽的组件拆分统计与日志 | **系统级臃肿洞察 Rule** | 🟢 80% | `SYSTEM_PROMPT_BLOAT` 规则已经能分析 `detailed_stats` 提供警告 |
| **给出明确改进意见** | `/context audit` 已展示全面细节 | **已提供 Actionable Advice 模块** | 🟢 90% | 每一个触发红线的 Rule 都附带人工手写的解题方法 |
| **确保 context 好的状态** | 兜底压缩机制已上线 (Rule-compact)| 评分机制真空 | 🟡 50% | Profiler 无法提炼综合指标，缺乏自动化的 Health Check Report |

---

## 4. 下一步行动纲领：重塑 Profiler (部分已完成)

Rust 侧 P0 已清零。Python Profiler 的 Rule 引擎已经完成断点续连。

### 最新冲刺进度
✅ **阶段 1: 修复基建断裂** (统一 Model，接通 `engine.py`，修正 Terminal 的 UI 渲染崩溃和截断报错)
✅ **阶段 2: 注入灵魂检测模块** (编写 `SystemPromptAnalyzer` 和 `CausalChainDetector`)
✅ **阶段 3: 输出实操意见** (添加 Actionable 建议)

### 遗留待办：
- [ ] 整合生成 **Context Health Score (0-100)** 评分雷达。
- [ ] Timeline 累计时间轴预警 (Accumulative view)。
- [ ] 清理废弃入口 (`claw_audit.py` 老脚本)。
