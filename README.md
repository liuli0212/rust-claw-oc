# Rusty-Claw
> **Powered by JaviRust (Elite AI Engineering Agent)**

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
  - **Verification:** Automatically validates context before applying changes, preventing accidental corruption of source files.
- **⚡ Dual-Phase Task Execution:**
  - **Phase 1 (Lead Architect):** Analyzes request complexity and generates a multi-step execution plan using a lightweight, low-token prompt.
  - **Phase 2 (Execution Engineer):** Executes the plan turn-by-turn with full project context (AGENTS.md, README, Environment) and autonomous tool usage.
- **🔌 Multi-Platform & Multi-Provider:**
  - **Providers:** Supports Google Gemini, Aliyun Qwen, and any OpenAI-compatible API (DeepSeek, LocalAI, vLLM).
  - **Platforms:** Supports CLI, Telegram, and Discord concurrently.

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
### 3. Configuration (Optional)
You can configure providers using a `config.toml` file in the current directory or `~/.config/rusty-claw/config.toml`.

**Example `config.toml`:**
```toml
default_provider = "deepseek"

[providers.deepseek]
type = "openai_compat"
api_key_env = "DEEPSEEK_API_KEY"
base_url = "https://api.deepseek.com/v1/chat/completions"
model = "deepseek-chat"

[providers.aliyun]
type = "openai_compat"
api_key_env = "DASHSCOPE_API_KEY"
# Note: For Aliyun Coding Plan, use the full endpoint path
base_url = "https://coding.dashscope.aliyuncs.com/v1/chat/completions"
model = "qwen3.5-plus"
```

### 4. CLI Commands
- `/status`: Show current provider, model, context usage stats, and token count.
- `/new`: Clear current session context and start fresh.
- `exit`: Quit the application.


**Runtime Tuning (Advanced):**
```env
# Enable verbose prompt usage reports in logs (Default: 0)
CLAW_PROMPT_REPORT=1

# Max autonomous steps per user request (Default: 12)
CLAW_MAX_TASK_ITERATIONS=20
```
