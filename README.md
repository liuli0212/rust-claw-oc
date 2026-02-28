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
  - **Smart Token Limits:** Automatically detects modern models (Gemini 2/3, Qwen, DeepSeek) and allocates appropriate context windows (1M+ for Gemini, 128k for others).
  - **Crash-Proof Configuration:** Gracefully handles missing API keys or malformed configs without crashing the agent.
- **⚡ Dual-Phase Task Execution:**
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
- **🛡️ Active Context Curation:**
  - **Smart Stripping:** Automatically compresses historical tool outputs like `read_file` or `ls` to keep only essential summaries. This saves lots of tokens.
  - **Focus Booster:** Injects attention prompts like "Focus on this new message" when history gets long.
  - **Safety Buffer:** Keeps the last 3 turns in full detail to handle references like "what did that error say?" while optimizing older history.
- **🔄 Reliable & Self-Healing:**
  - **Exponential Backoff:** API calls automatically retry with exponential delays on failure (429, 5xx).
  - **Dynamic Context Window:** Detects model limits like 1M for Gemini or 128k for GPT-4o and adjusts buffers to prevent overflow.
- **🌐 Full-Featured Browser Automation:**
  - **Persistent Session:** Keeps a browser instance across turns for complex workflows like login, navigation, and extraction.
  - **See-Act Loop:** Uses `snapshot` to parse DOM into JSON and `act` to interact with elements using stable IDs.

## 🧰 Built-in Tools

Rusty-Claw comes equipped with a comprehensive suite of engineering tools:

| Category | Tool | Description |
|----------|------|-------------|
| **System** | `execute_bash` | Execute shell commands in a secure PTY environment. |
| | `finish_task` | Signal completion of the user's request. |
| **File I/O** | `read_file` | Read file contents with automatic large-file handling. |
| | `write_file` | Create or overwrite files with precision. |
| **Browser** | `browser` | Full browser automation: `start`, `stop`, `navigate`, `snapshot` (DOM extraction), `act` (click/type). |
| **Web** | `web_search` | Real-time internet search via Tavily API. |
| | `web_fetch` | Fetch webpages and convert HTML to Markdown. |
| **Planning** | `task_plan` | Manage the structured `.rusty_claw_task_plan.json` (add/update/complete steps). |
| **Memory** | `rag_search` | Semantic search over the project's vector database. |
| | `rag_insert` | Index new knowledge into long-term memory. |,
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

> **Note:** CLI arguments (e.g., `/model deepseek-coder`) now take precedence over `config.toml` settings.
> **Note:** The default context window is now **128,000 tokens** (up from 32k) for unknown models.

**Example `config.toml`:**
You can configure providers using a `config.toml` file in the current directory or `~/.config/rusty-claw/config.toml`.

**Example `config.toml`:**
```toml
default_provider = "deepseek"
context_window = 64000 # Optional: Override auto-detected window size

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
