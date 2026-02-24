# Rusty-Claw

An industrial-grade, highly autonomous CLI AI Agent built entirely in Rust. Designed to be lightweight, secure, and easily distributable as a single compiled binary without requiring a heavy runtime environment.

This project is a Rust-native implementation of an AI agent, closely following the "Zero-Trust & Sandbox" architecture design. It uses the latest LLM models (via Gemini API) to understand natural language instructions, plan tasks, and execute them safely on your local machine using its suite of tools.

## 🚀 Key Features

- **⚡ High-Performance Core:** Built on Rust's Tokio runtime. The main event loop is non-blocking, ensuring the agent remains responsive even during heavy I/O operations.
- **🧠 Hybrid RAG Memory:**
  - **In-Memory Vector Cache:** Startup loads all embeddings into RAM for sub-millisecond similarity search.
  - **SQLite + FTS5:** Integrated full-text search for precise keyword matching (BM25).
  - **Auto-Persistence:** All memory chunks are ACID-persisted to a local SQLite database (`.rusty_claw_memory.db`).
- **🛡️ Secure Bash Sandbox:** Employs a true pseudo-terminal (`portable-pty`) wrapper for executing bash commands. It handles interactive TTY commands, strips ANSI codes, and enforces timeouts.
- **🔄 Resilient Context Management:**
  - **Async Compaction:** History summarization runs in the background, never blocking the user's next turn.
  - **Soft Limits:** Allows temporary context overflow to maintain conversation flow while cleanup happens asynchronously.
- **🔌 Multi-Platform:** Supports CLI, Telegram, and Discord concurrently.

## 🛠️ Setup & Configuration

### Prerequisites
- Rust Toolchain (`cargo`, `rustc`)
- A valid **Gemini API Key** (Google AI Studio).

### 1. Installation
Clone the repository:
```bash
git clone https://github.com/liuli0212/rust-claw-oc.git
cd rust-claw-oc
```

### 2. Environment Variables (.env)
Create a `.env` file in the root directory.

**Required:**
```env
# Google Gemini API Key (Required for LLM and Embeddings)
GEMINI_API_KEY=your_actual_api_key_here
```

**Optional Integrations:**
```env
# Web Search via Tavily (Recommended for real-time data)
TAVILY_API_KEY=tvly-xxxxxxxx

# Chat Platform Bots
TELEGRAM_BOT_TOKEN=12345:abcdef...
DISCORD_BOT_TOKEN=MTAw...
```

**Runtime Tuning (Advanced):**
```env
# Enable verbose prompt usage reports in logs (Default: 0)
CLAW_PROMPT_REPORT=1

# Max autonomous steps per user request (Default: 12)
CLAW_MAX_TASK_ITERATIONS=20

# Enable/Disable auto-recovery rules (Default: all)
# Values: "all" or comma-separated list like "missing_command,missing_path"
CLAW_RECOVERY_RULES=all

# Enforce strict <final> tag parsing for cleaner output (Default: 0)
CLAW_ENFORCE_FINAL_TAG=1

# Logging level (fallback when RUST_LOG is not set, Default: info)
CLAW_LOG_LEVEL=info

# Enable file logging (1/0, Default: 1)
CLAW_FILE_LOG=1

# Log directory and file name (Default: logs/rusty-claw.log with daily rotation)
CLAW_LOG_DIR=logs
CLAW_LOG_FILE=rusty-claw.log
```

### 3. Build & Run
Run in release mode for maximum performance (especially for vector search):
```bash
cargo run --release
```

Common runtime flags (override env vars):
```bash
# Choose model from CLI
cargo run --release -- --model gemini-2.0-flash

# Debug logs + file logs in custom location
cargo run --release -- \
  --log-level debug \
  --log-dir logs \
  --log-file rusty-claw.log

# Disable file log, keep stdout logs only
cargo run --release -- --no-file-log

# Force perf/prompt reports without editing .env
cargo run --release -- --timing-report --prompt-report
```

## 🏗️ Architecture Overview

The system follows a non-blocking Actor-like model:

*   **`src/core.rs`**: The `AgentLoop` state machine. It spawns background tasks for RAG (`execute_retrieval_task`) and compaction (`maybe_compact_history`) to keep the critical path clear.
*   **`src/rag.rs`**: The Memory Subsystem.
    *   Uses `fastembed-rs` for local embedding generation (runs on CPU/Metal).
    *   Uses `rusqlite` for persistent storage.
    *   Implements a hybrid scoring algorithm: `Score = 0.7 * Cosine(Vector) + 0.3 * BM25(Keyword)`.
*   **`src/session_manager.rs`**: Manages session state with an in-memory Write-Through cache, flushing to disk asynchronously to avoid I/O blocking.
*   **`src/tools.rs`**: Standard library of tools (Bash, File I/O, Web Fetch).

## 🧩 Dynamic Skills
You can teach the agent new tools without recompiling.
1.  Create a `.md` file in `skills/`.
2.  Add YAML frontmatter describing the tool.
3.  Write the prompt/logic in Markdown.
The agent loads these at startup.

## License
MIT
