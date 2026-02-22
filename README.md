# Rusty-Claw

A Rust-based implementation of OpenClaw, following the provided design document.

## Architecture

This project implements the core components specified in the `rusty-claw-design.md`:

1.  **`core` (AgentLoop & ContextManager)**:
    *   State machine orchestrating the Think -> Act -> Observe loop.
    *   Context management with structured messages.
2.  **`llm_client` (Gemini API adapter)**:
    *   Uses `reqwest` for HTTP requests.
    *   Implements Server-Sent Events (SSE) parsing via `tokio::sync::mpsc::channel` for streaming the LLM response asynchronously.
    *   Supports structured Tool Call extraction directly from the stream.
3.  **`tools` (Tools Sandbox)**:
    *   Uniform `Tool` trait defining `name`, `description`, `schema` and `execute`.
    *   `BashTool`: Executes commands using `tokio::process::Command` with intelligent log truncation (top 500 lines, bottom 500 lines) and strict timeout control.
    *   `schemars` is used to automatically generate JSON Schema from Rust structs.
4.  **`memory` (File-based Storage)**:
    *   `WorkspaceMemory`: Read/Write abstraction for `MEMORY.md`.
    *   Registered as `read_workspace_memory` and `write_workspace_memory` tools for the LLM.
5.  **`gateway/cli`**:
    *   Interactive REPL built with `rustyline`.

## Prerequisites

*   Rust toolchain (cargo, rustc)
*   A valid Gemini API key.

## Setup & Run

1.  Create a `.env` file in the root directory and add your API key:
    ```
    GEMINI_API_KEY=your_api_key_here
    ```
2.  Build and run the agent:
    ```bash
    cargo run
    ```
3.  You will be dropped into an interactive REPL `>> `. Start chatting!

## Design Decisions vs Spec

*   **PTY**: For the MVP, `std::process::Command` (via `tokio::process::Command`) is used instead of `portable-pty` for the `BashTool`. It still handles timeouts and stdout/stderr correctly.
*   **Vector Store**: The `MEMORY.md` workspace memory is implemented. The `lancedb` RAG implementation is a placeholder for future iterations due to the complexity of embedding generation inside a single MVP.
*   **Native Function Calling**: Instead of the proposed fallback "JSON Extractor", this implementation correctly utilizes Gemini's native structured `functionCall` SSE response format for high reliability.

## Code Quality Verification

*   All networking is fully async using `tokio` and `reqwest`.
*   Data structures are strictly typed using `serde`.
*   Error handling is implemented using `thiserror`.
