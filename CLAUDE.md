# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```bash
cargo build                # compile
cargo run                  # run the agent (reads .env and config.toml)
cargo test                 # run all tests
cargo test module::tests::test_name -- --exact   # run a single test
cargo clippy -- -D warnings   # lint (warnings = errors)
cargo fmt                  # format code
```

No Makefile/justfile — standard Cargo only. Tests use `#[cfg(test)]` inline modules with `serial_test` for shared-state serialization and `tempfile` for filesystem fixtures.

## Architecture

Rusty-Claw is a single-binary, single-crate autonomous AI agent built in Rust. It takes natural language instructions, plans multi-step tasks, and executes them via built-in tools.

### Dual-Phase Execution (core.rs)

Every user request flows through two phases inside `AgentLoop`:
1. **Analyze phase** (`analyze_request`): lightweight prompt produces a step-by-step plan (Lead Architect role).
2. **Execution phase** (`step` loop): executes the plan turn-by-turn with full context, calling tools autonomously (Engineer role). Max iterations controlled by `CLAW_MAX_TASK_ITERATIONS` (default 12).

### Context Management (context.rs + context_assembler.rs)

The largest and most intricate subsystem. `context.rs` handles:
- Dynamic system prompt assembly (injects OS/arch, working dir, local `.md` files)
- Token budgeting with auto-detected per-model limits (1M+ for Gemini, 128k for others)
- Async background compaction (history summarization) with soft-limit overflow tolerance
- Self-Adaptive Context (SAC) — injects attention cues when history is long
- Smart stripping of old tool outputs while keeping last 3 turns intact

`context_assembler.rs` builds the final payload sent to the LLM.

### LLM Client (llm_client.rs)

Unified abstraction over three provider types:
- **Gemini** (native API)
- **Aliyun Qwen** (OpenAI-compatible)
- **Any OpenAI-compatible API** (DeepSeek, LocalAI, vLLM)

Handles streaming + single-shot generation with exponential backoff retries.

### Tool System (tools.rs)

All agent capabilities implement an async `Tool` trait. Tools: `execute_bash`, `read_file`, `write_file`, `browser`, `web_search`, `web_fetch`, `task_plan`, `rag_search`, `rag_insert`, `finish_task`. Bash execution uses `portable-pty` for secure PTY sandboxing with ANSI stripping and timeouts.

### RAG Memory (rag.rs)

Hybrid search over `.rusty_claw_memory.db` (SQLite):
- BM25 keyword search via FTS5
- Semantic vector search via `fastembed` (embeddings loaded into RAM at startup)
- Scores are blended for ranked results

### Multi-Platform (session_manager.rs, telegram.rs, discord.rs)

The same `AgentLoop` core runs concurrently under CLI (rustyline REPL), Telegram (teloxide), and Discord (serenity) via `SessionManager`.

### Task State (task_state.rs)

Structured task plan persisted to `.rusty_claw_task_plan.json`. Managed through the `task_plan` tool with direct JSON mutation.

### Skills (skills.rs)

Reusable skill definitions loaded from `skills/` directory (markdown files like `git-status.md`, `understand_image.md`).

## Configuration

- **`config.toml`**: loaded from working directory or `~/.config/rusty-claw/config.toml`. Defines LLM providers, models, and context window sizes.
- **`.env`**: API keys (`GEMINI_API_KEY` required; `TAVILY_API_KEY`, `TELEGRAM_BOT_TOKEN`, `DISCORD_BOT_TOKEN` optional).
- **`CLAW_PROMPT_REPORT=1`**: enables verbose prompt logging.
- **`CLAW_MAX_TASK_ITERATIONS=N`**: overrides max autonomous steps per request.

## Context Profiler (context-profiler/)

Standalone Python tool for auditing context usage. Workflow: `/context dump` in the agent CLI exports `debug_context.json`, then `python3 context-profiler/run_profiler.py audit debug_context.json` runs the audit.
