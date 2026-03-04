# Claw-Context Profiler 🛡️

**JaviRust 架构出品 - 工业级 LLM 上下文审计与优化利器**

`Claw-Context Profiler` 是一个专门为 `rusty-claw-oc` 设计的高性能上下文分析引擎。它通过静态启发式规则和因果链追踪，深度扫描大模型会话历史，识别并标记“上下文膨胀”（Context Bloat）、“冗余工具输出”以及“逻辑断裂”，帮助开发者优化 Token 消耗并提升模型遵循指令的精准度。

---

## ✨ 核心特性

-   **🔍 深度膨胀扫描 (Bloat Detection)**：
    -   **Grep Bloat**: 识别由高频 grep/find 命令产生的无效冗余信息。
    -   **System Bloat**: 监测系统级提示词（System Prompt）在长会话中的占比偏移。
-   **⛓️ 因果链追踪 (Causal Chain Analysis)**：
    -   分析“用户指��� -> 思考过程 -> 工具调用 -> 结果观察”的完整链路，识别无效的重试死循环。
-   **📊 Token 时间轴可视化**：
    -   提供各回合（Turn）Token 占用的趋势图，精准定位导致上下文爆炸的“元凶”。
-   **🚀 极速驱动 (Powered by uv)**：
    -   原生支持 `uv`，实现零克隆、零等待的环境初始化。

---

## 🛠️ 安装与就绪

推荐使用 **`uv`** 以获得最佳性能和最省磁盘空间的体验。

```bash
cd context-profiler

# 极速创建虚拟环境并安装开发模式依赖
uv pip install -e .
```

---

## 📖 使用指南

### 1. 分析实时会话记录 (JSONL)
这是最常用的场景，用于审计 `rusty-claw` 产生的真实对话链路：

```bash
# 分析 CLI 会话
uv run python run_profiler.py ../rusty_claw/sessions/cli.jsonl

# 分析特定 Telegram 会话
uv run python run_profiler.py ../rusty_claw/sessions/telegram_<chat_id>.jsonl
```

### 2. 执行综合上下文审计
```bash
uv run python claw_audit.py audit debug_context.json
```

### 3. 进行上下文差异比对 (Context Diff)
```bash
uv run python claw_audit.py diff dump_v1.json dump_v2.json
```

### 4. 使用注册的快捷命令
安装后，您可以直接调用 `pyproject.toml` 中定义的 Entry Point：
```bash
uv run ctx-profile audit debug_context.json
```

---

## 🏗️ 架构设计

项目采用高度模块化的**规则驱动引擎**：
-   **`src/context_profiler/engine.py`**: 核心调度器，负责加载数据流并分发给规则链。
-   **`src/context_profiler/rules/`**: 独立插件式规则库。您可以轻松编写自定义的 `base.py` 子类来实现新的分析逻辑。
-   **`src/context_profiler/models.py`**: 严格的 Pydantic 数据模型定义，确保解析的鲁棒性。

---

## ⚖️ 最佳实践

1.  **定期巡检**：建议在进行大规模重构任务前后，使用本工具对比上下文的变化。
2.  **Token 降本**：如果报告中出现大量的 `[BLOAT]` 警告，说明需要调整 `AgentContext` 的 `Squashing`（压缩）阈值。
3.  **调试助手**：当 Agent 陷入“死循环”或“胡言乱语”时，运行 Profiler 通常能直接找出是因为哪个工具输出占据了 90% 的上下文。

---
*Powered by JaviRust Engineering Systems*
