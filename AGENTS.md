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
- **PREFERRED**: Use the `patch_file` tool for modifying existing files. It uses the system `patch` command and expects a unified diff format.
  - Example:
    ```json
    {
      "thought": "Update the version number",
      "path": "Cargo.toml",
      "patch": "--- Cargo.toml\n+++ Cargo.toml\n@@ -1,4 +1,4 @@\n [package]\n name = \"rusty-claw\"\n-version = \"0.1.0\"\n+version = \"0.1.1\"\n edition = \"2021\""
    }
    ```
- **FALLBACK**: Use the native `write_file` tool to overwrite or create files ONLY if the file is small or if `patch_file` fails repeatedly.
- For precise edits, use `sed`/`awk` ONLY for trivial 1-liners.

### 2. Rust Formatting & Idioms
- Follow standard `rustfmt` guidelines. Indentation is 4 spaces.
- **Naming**: `snake_case` for functions/variables/modules. `PascalCase` for structs/enums/traits. `SCREAMING_SNAKE_CASE` for constants.
- Avoid large `unwrap()` or `expect()` calls in production paths (`src/core.rs`, `src/tools.rs`). Return proper `Result` types instead. `unwrap()` is acceptable in tests.
- **Imports**: Group standard library (`std::`), external crates, and internal modules (`crate::`).

### 3. Error Handling
- Use `thiserror` for library-level error definitions (e.g., `ToolError`, `LlmError`).
- Use the `?` operator extensively to propagate errors up to the caller.
- Do NOT swallow errors silently. If an e
