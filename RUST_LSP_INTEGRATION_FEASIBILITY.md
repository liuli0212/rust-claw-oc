# Rust LSP (rust-analyzer) 集成可行性调研报告

## 1. 执行摘要 (Executive Summary)
当前 JaviRust 主要通过 `read_file` 和 `grep` 等基础工具理解代码。虽然在处理小型项目时表现良好，但在面对具有复杂依赖、宏调用和深层调用链的大型 Rust 项目时，缺乏结构化的语义理解能力。集成 **rust-analyzer (LSP)** 将使 JaviRust 具备“IDE 级”的代码洞察力，显著提升其在代码重构、Bug 定位和架构分析方面的表现。

## 2. 为什么需要 Rust LSP？
目前 JaviRust 的局限性：
- **缺乏语义跳转**：无法准确找到跨文件的函数定义或 trait 实现。
- **宏展开黑盒**：无法理解 Rust 宏生成的代码（如 `serde` 的 `Serialize`）。
- **诊断滞后**：必须运行 `cargo check` 才能发现语法错误，无法实时获取上下文诊断。
- **引用查找困难**：难以确定修改一个函数会影响哪些调用方。

**LSP 集成后的优势：**
- **精确导航**：支持 `Go to Definition` 和 `Find References`。
- **实时诊断**：在编写代码过程中即时获取类型检查和语法错误。
- **符号搜索**：快速定位项目中的 Struct、Enum 和 Function。
- **类型洞察**：获取变量的精确类型，即使是复杂的泛型。

## 3. 技术架构方案

### 3.1 核心组件
1. **LSP Server**: `rust-analyzer` 二进制文件（需用户本地安装）。
2. **LSP Client (JaviRust 侧)**: 负责启动 server，通过 JSON-RPC (stdin/stdout) 进行异步通信。
3. **LSP Tools**: 封装为 Agent 可调用的工具集。

### 3.2 推荐实现路径
建议使用现有的 Rust LSP 客户端库以降低开发成本：
- **`codive_lsp`**: 专门为 AI Agent 设计的 LSP 客户端封装，支持多种语言服务器。
- **`lsp-types`**: 提供标准的 LSP 数据结构定义。

### 3.3 拟新增工具集 (Proposed Tools)
- `lsp_goto_definition(path, line, col)`: 返回定义所在的文件和位置。
- `lsp_find_references(path, line, col)`: 返回所有引用位置。
- `lsp_get_diagnostics(path)`: 返回当���文件的编译错误和警告。
- `lsp_get_symbols(path)`: 返回文件内的所有符号树。
- `lsp_hover(path, line, col)`: 返回符号的文档和类型信息。

## 4. 实施路线图 (Roadmap)

### 第一阶段：原型验证 (MVP)
- 在 `rusty-claw-oc` 中引入 `codive_lsp` 或类似的轻量级客户端。
- 实现 `rust-analyzer` 的自动发现与启动。
- 封装 `lsp_goto_definition` 工具并进行实测。

### 第二阶段：深度集成
- 实现全项目索引（Workspace Indexing）。
- 集成实时诊断到 `AgentLoop`，在 `write_file` 后自动触发检查。
- 支持宏展开后的代码查看。

### 第三阶段：多语言扩展
- 基于相同的 LSP 框架，扩展支持 Python (Pyright)、TypeScript (tsserver) 等。

## 5. 挑战与对策 (Challenges & Mitigation)

| 挑战 | 对策 |
| :--- | :--- |
| **资源占用** | `rust-analyzer` 内存占用较高。建议仅在检测到 Rust 项目时按需启动，并支持手动关闭。 |
| **初始化耗时** | 大型项目索引较慢。采用异步初始化，并在工具调用时增加“索引中”的状态反馈。 |
| **环境依赖** | 需要用户安装 `rust-analyzer`。在启动时进行环境检查，若缺失则降级为基础 `grep` 模式。 |

## 6. 结论
集成 Rust LSP 是将 JaviRust 从“高级脚本编写者”提升为“���深 Rust 架构师”的关键一步。技术方案成熟（基于 `rust-analyzer` + `codive_lsp`），开发工作量适中，建议作为下一阶段的核心特性进行开发。

---
**调研人**: JaviRust (Elite AI Engineering Agent)  
**日期**: 2026-03-12
