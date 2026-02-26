# Rusty-Claw Agent Operating Guidelines

Welcome, Agent. You are operating within `rusty-claw-oc`, a high-performance, single-binary AI Agent OS built in Rust. This document outlines the commands, architecture, and coding conventions you must follow to succeed here.

## 🛠️ Build, Lint, and Test Commands

* **Build the project**:
  ```bash
  cargo build
  ```
* **Run the agent in development**:
  ```bash
  cargo run
  ```
* **Run all tests**:
  ```bash
  cargo test
  ```
* **Run a single test (Highly Recommended for iteration)**:
  ```bash
  cargo test <module>::tests::<test_name> -- --exact
  # Example: cargo test context::tests::test_token_budget_truncation -- --exact
  ```
* **Run Linter (Clippy)**:
  ```bash
  cargo clippy -- -D warnings
  ```
* **Format code**:
  ```bash
  cargo fmt
  ```

## 🏗️ Architecture Overview

Rusty-Claw relies on a minimal-dependency, highly synchronous/asynchronous hybrid architecture:
- **`src/core.rs`**: Contains `AgentLoop`, the core state machine. It implements a **Dual-Phase Execution** strategy: `analyze_request` for initial planning (Lead Architect mode) and a main `step` loop for turn-by-turn execution (Engineer mode). Includes Auto-Compaction for long context windows.
- **`src/context.rs`**: Manages the conversational turns, dynamic System Prompt assembly (injecting OS/Arch, WorkDir, and local MD files), and intelligent Token Budgeting (history squashing).
- **`src/tools.rs`**: Contains the implementations of the Agent's capabilities (`BashTool`, `WriteFileTool`, `ReadFileTool`, etc.). Tools implement the async `Tool` trait.
- **`src/llm_client.rs`**: Handles communication with the Gemini API (default: `gemini-3.1-pro-preview`). Supports both streaming and single-shot generation.
- **`src/rag.rs`**: Hybrid Memory implementation. Uses `rusqlite` + `FTS5` for BM25 keyword matching and `fastembed` for semantic vector search, blending the scores.

## 📝 Coding Style & Conventions

### 1. File Modification
- **DO NOT** use brittle bash heredocs (`cat << 'EOF'`) to write code. 
- **ALWAYS** use the native `write_file` tool to overwrite or create files.
- For precise edits, use the `edit` tool (if available in your capabilities) or `sed`/`awk` ONLY for trivial 1-liners. Otherwise, use `write_file`.

### 2. Rust Formatting & Idioms
- Follow standard `rustfmt` guidelines. Indentation is 4 spaces.
- **Naming**: `snake_case` for functions/variables/modules. `PascalCase` for structs/enums/traits. `SCREAMING_SNAKE_CASE` for constants.
- Avoid large `unwrap()` or `expect()` calls in production paths (`src/core.rs`, `src/tools.rs`). Return proper `Result` types instead. `unwrap()` is acceptable in tests.
- **Imports**: Group standard library (`std::`), external crates, and internal modules (`crate::`).

### 3. Error Handling
- Use `thiserror` for library-level error definitions (e.g., `ToolError`, `LlmError`).
- Use the `?` operator extensively to propagate errors up to the caller.
- Do NOT swallow errors silently. If an error must be ignored, log it explicitly or comment why it is safe to ignore.

### 4. Asynchronous Programming
- The project heavily uses `tokio`. 
- Be mindful of `std::sync::Mutex` vs `tokio::sync::Mutex`. Do not hold `std::sync::Mutex` guards across `.await` yield points. If you must hold a lock across an await, use `tokio::sync::Mutex`.
- Use `#[async_trait]` when adding async methods to traits (like `Tool`).

### 5. Adding New Tools
To add a new tool to Rusty-Claw:
1. Define the argument struct and derive `Serialize, Deserialize, JsonSchema`.
2. Define an empty struct for the tool state (or one with dependencies if needed).
3. Implement `#[async_trait] impl Tool for MyNewTool`. Provide `name`, `description`, `parameters_schema` (via `schemars`), and the `execute` function.
4. Register the tool in `src/main.rs` inside the `tools.push(...)` block.

## 🧠 Agent Behavioral Rules
1. **Be Proactive**: You have full system access. Do not ask "Would you like me to write the code?". Just write it, test it, and report the result.
2. **Handle Context Size**: If a command produces massive output (like `npm i` or `cargo build`), use tools like `grep` or redirect output to a file and `head`/`tail` it. `Rusty-Claw` will auto-truncate outputs over 15k chars, but being precise saves your own token budget.
3. **Diagnose and Fix**: If a test or command fails, do not just apologize. Read the error, understand the root cause, apply a fix, and run the test again until it passes.