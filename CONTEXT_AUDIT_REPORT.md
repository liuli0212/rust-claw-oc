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

### 🔴 P0：Profiler 核心架构缺陷

#### 2.1 数据模型不兼容，Rule 系统完全断裂未能执行
- **问题**: `rules/base.py` 引用了 `ContextMessage` 和 `OptimizationSuggestion`，但 `models.py` 中并没有定义这些类（反而定义的是 `TurnStats` 和 `AuditReport`）。
- **结果**: `engine.py` 的 `run_audit()` 方法中**完全没有调用任何 Rule**。整个 Rule 诊断引擎是一个无法运行的空壳架子。
- **修复方向**: 统一 `models.py` 的数据结构，修复 Import 错误，并在 `engine.py` 的主流程中添加 `_run_rules()` 将提取的数据真正喂给各 Rule 执行。

#### 2.2 Profiler 无法检测"关键历史信息丢失"
- **对比设计目标**: "分析出context有没有缺重要的历史信息"。
- **现状**: 当前仅仅简单检查了最后 3 轮有无 `user` 角色 以及 System Prompt 是否为空字符串。
- **缺失的检测能力**:
  - **因果断链检测**: 工具调用了 `patch_file` 尝试修 bug，但因为历史丢弃，上下文里根本找不到 `error`/`failed`/`panic` 的输出。导致 LLM "为了修而修"。
  - **严重裁切预警**: `report.history_turns_included` 远小于对话总轮数，必须明确报警告。
  - **计划偏离检测**: 检查上下文中的 Task Plan 内容是否在最近几次的 Tool Calls 中被实际推进。

#### 2.3 System Prompt 臃肿度深度分析缺失
- **对比设计目标**: "有没有放了太多不需要的信息在里面"。
- **现状**: 尽管 Rust 侧 dump 文件现在已经提供了精准的 `detailed_stats`，Profiler 却**完全没有读取和分析这些分项数据**。
- **缺失的检测能力**:
  - 分析 System Prompt 各个维度（Project, Custom, Runtime, Task Plan）的 Token 占比情况并发出预警。
  - 例如，如果 `system_project` (AGENTS.md 等内容) 占比总额度过高（> 25%），应建议用户删减不相关的知识库。

### 🟠 P1：Profiler 体验与建议缺陷

#### 2.4 冗余检测缺乏"可操作的改进意见" (Actionable Advice)
- **现状**: 现有的冗余检查仅仅打了一个 `High Volume` 标签。
- **设计目标要求**: "能给出明确问题和改进意见"。
- **改进方向**: 规则不仅要标记问题，还需要抛出带引导性的建议方案，比如："Turn X 中 read_file 输出了 3000 Token，建议在 Rust 侧调整 `truncate_old_tool_results` 阈值，或建议 Agent 使用 grep 缩小范围。"

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
| **分析 context 是否合理** | 提供最精准的 Payload 与 Stats 分项 | Timeline 简单展示 | 🟡 40% | 没有结合模型进行上下文成分合理性校验；无健康度打分体系 |
| **有没有缺重要历史信息** | 记录了被截断的内容与标记 | 简单检查 user 角色 | 🔴 10% | 规则系统断裂，完全未实现针对历史丢失的断链追踪分析 |
| **有没有放太多没用信息** | 提供详尽的组件拆分统计与日志 | 按长度进行硬切分标记 | 🟡 30% | 未解析详细的 `detailed_stats` 数据，对 System Prompt 无洞察 |
| **给出明确改进意见** | `/context audit` 已展示全面细节 | Redundancy 标签提示 | 🔴 10% | 缺少 Actionable Advice (明确指导建议) 引领用户排查 |
| **确保 context 好的状态** | 兜底压缩机制已上线 (Rule-compact)| 评分机制真空 | 🟡 50% | Profiler 无法提炼综合指标，缺乏自动化的 Health Check Report |

---

## 4. 下一步行动纲领：重塑 Profiler

Rust 侧 P0 已清零，下一步应 **100% 投入到 Context Profiler 的架构重构与能力加固上**，让其成为真正满足设计目标的专家级审计工具。

### 冲刺计划（预估 1-2 天）

**阶段 1: 修复基建断裂**
1. 统一 `context_profiler/models.py` 与规则系统的模型约束。
2. 重写 `engine.py`：注入 Rule 调用链，把历史 Message 序列和详细 Report 数据喂给 Rule Engine 获取诊断结果。
3. 清理废弃入口。

**阶段 2: 注入灵魂检测模块 (Rules)**
4. 开发 `SystemPromptAnalyzer` Rule：读取通过 Rust 侧新增的 `detailed_stats` 分项，分析各个模块（Identity / Project 等等）的臃肿情况。
5. 开发 `CausalChainDetector` Rule：扫描历史流向，确保如果最近发生对某个文件的修复动作，前文必须有对应的 Error 或失败输出。（否则就是出现了信息剥落）

**阶段 3: 输出实操意见**
6. 将每个检测出的缺陷都封装成带 `Actionable Advice` (下一步怎么做) 的报告。
7. 整合生成 **Context Health Score (0-100)** 评分雷达与优化提示墙，提升界面信息密度。
