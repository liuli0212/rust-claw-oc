# Rusty-Claw

An industrial-grade, highly autonomous CLI AI Agent built entirely in Rust. Designed to be lightweight, secure, and easily distributable as a single compiled binary without requiring a heavy runtime environment.

This project is a Rust-native implementation of an AI agent, closely following the "Zero-Trust & Sandbox" architecture design. It uses the latest LLM models (via Gemini API) to understand natural language instructions, plan tasks, and execute them safely on your local machine using its suite of tools.

## Key Features

- 🧠 **Native Function Calling:** Uses robust JSON-Schema generation (`schemars`) and Gemini's native structured Tool Calling for zero-hallucination tool execution. 
- 🔒 **Secure Bash Sandbox:** Employs a true pseudo-terminal (`portable-pty`) wrapper for executing bash commands. It handles interactive TTY commands flawlessly, strips ANSI color codes for clean context, and enforces strict timeouts (`SIGKILL`) to prevent zombie processes.
- 💾 **RAG Memory (Semantic Knowledge Base):** Integrated with pure-Rust `fastembed` for local, offline, high-dimensional vector embeddings (no heavy C++ or Python dependencies). The agent can `memorize_knowledge` and `search_knowledge_base` to retain project rules or code snippets indefinitely.
- 📊 **Context Budgeting:** Integrated with `tiktoken-rs`. It automatically calculates token consumption and uses a sliding-window truncation algorithm to ensure the context never exceeds the LLM's budget (e.g., 32k tokens), preventing crashes during long-running sessions.
- 🧩 **Dynamic Markdown Skills:** You can teach the agent new tools without writing Rust code. Drop a Markdown file with a YAML frontmatter into the `skills/` directory, and it will be dynamically parsed and loaded as a fully functional LLM Tool.

## Prerequisites

- Rust Toolchain (`cargo`, `rustc`)
- A valid **Gemini API Key** (or another compatible LLM provider if adapted).

## Setup & Run

1. Clone the repository and navigate into it:
   ```bash
   git clone https://github.com/liuli0212/rust-claw-oc.git
   cd rust-claw-oc
   ```

2. Configure your API key. Create a `.env` file in the root of the project:
   ```env
   GEMINI_API_KEY=your_actual_api_key_here
   ```

3. Build and launch the agent (Release mode recommended for maximum speed):
   ```bash
   cargo run --release
   ```

4. You will be greeted with the interactive REPL:
   ```
   Welcome to Rusty-Claw! (type 'exit' to quit)
   Loaded 1 dynamic skills from 'skills/' directory.
   >> 
   ```
   Try typing: *"Please list the files in the current directory"* or *"Memorize this rule: Always format code with rustfmt."*

## Architecture

*   **`src/core.rs`**: The main `AgentLoop`. Implements the Think -> Act -> Observe state machine.
*   **`src/llm_client.rs`**: Handles HTTP SSE Streaming and asynchronous chunk parsing.
*   **`src/tools.rs`**: The sandbox executor. Defines the `Tool` trait and implements the powerful `BashTool` (PTY) and Memory interfaces.
*   **`src/rag.rs`**: The pure-Rust semantic vector store using `fastembed`.
*   **`src/context.rs`**: Context management and `tiktoken-rs` driven token budgeting.
*   **`src/skills.rs`**: The dynamic Markdown-to-Tool parsing engine.

## License
MIT
