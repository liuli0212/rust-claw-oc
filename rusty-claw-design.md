# Rust-Claw Design & Architecture Specification

## 1. System Overview
Rust-Claw is a high-performance, resilient AI agent framework written in Rust. It aims to solve the latency and fragility issues common in Python/JS-based agents by leveraging Rust's concurrency, type safety, and efficient resource management.

## 2. Core Architecture Principles
1.  **Non-Blocking Core:** The main event loop must never block on I/O. RAG retrieval, history compaction, and tool execution must occur asynchronously or in parallel with generation where possible.
2.  **Hybrid Intelligence:** Combine deterministic code tools (fast, reliable) with probabilistic LLM generation (flexible, creative).
3.  **Self-Contained State:** Minimize external infrastructure dependencies. Prefer embedded databases (SQLite, LanceDB) over external services (Pinecone, Qdrant) for ease of deployment.

4.  **Context-Aware Resilience:** Context is treated as a limited, high-value resource. The system must proactively manage token budgets through intelligent truncation and background compaction without losing critical task state.

## 3. RAG & Memory Subsystem (Reference Architecture)

### 3.1 Embedding Strategy
Adopt a **Provider-Agnostic** approach similar to OpenClaw:
-   **Default Remote:** Google Gemini (`gemini-embedding-001`) or OpenAI (`text-embedding-3-small`).
-   **Local Fallback:** Support local ONNX models (e.g., `all-MiniLM-L6-v2`) via `ort` or `rust-bert` for users without API keys or offline usage.
-   **Configuration:** Allow users to specify `provider: "auto" | "openai" | "gemini" | "local"`.

### 3.2 Vector Storage
**Current State:** Naive in-memory or inefficient file storage.
**Target State:** **SQLite + Vector Extension**
-   Use **SQLite** as the primary storage engine.
-   Enable vector search capabilities via **sqlite-vec** (or similar embedded vector search lib like `lance`).
-   **Benefits:**
    -   **Zero Infra:** No need for Docker containers (Qdrant/Milvus).
    -   **ACID Compliance:** Transactional updates for metadata and vectors.
    -   **Hybrid Search:** Trivial to combine SQL `WHERE` clauses (metadata filters) with Vector distance functions.
    -   **Single File:** Easy backup/restore (`agent.sqlite`).

### 3.3 Retrieval Pipeline
Implement a **Hybrid Search** mechanism:
1.  **Keyword Search (FTS5):** Use SQLite's FTS5 module for fast, exact keyword matching (BM25).
2.  **Semantic Search (Vector):** Use embedding similarity for concept matching.
3.  **Reranking (Optional):** Combine results using Reciprocal Rank Fusion (RRF) or a lightweight cross-encoder if latency permits.

### 3.4 Caching Layer
To reduce latency and API costs:
-   **Embedding Cache:** Store `hash(text) -> vector` in a dedicated SQLite table or KV store (Sled/Redb).
-   **Query Cache:** Cache common queries and their retrieved context for short TTL (e.g., 5 minutes).

## 4. Concurrency Model
-   **Actor Model / Tokio Tasks:** Treat the "Planner", "Memory System", and "Tool Executor" as independent actors communicating via channels.
-   **Speculative Execution:**
    -   Start RAG retrieval *immediately* upon receiving user input.
    -   Simultaneously start LLM generation (if model allows streaming without full context, or use a "thinking" placeholder).
    -   Inject context dynamically or wait with a timeout (e.g., max 500ms for retrieval).

## 5. Tooling & Sandbox
-   **Sandboxed Execution:** Use Docker or lightweight virtualization (Wasm/Firecracker) for executing untrusted code tools.
-   **Standard Lib:** Provide a robust standard library of tools (File I/O, Web Search, Bash) that are highly optimized and safe.
## 6. Configuration & Extensibility
- **Config Loader:** `src/config.rs` handles TOML parsing and provider resolution. It supports cascading overrides (CLI > Config > Defaults).
- **Dynamic Providers:** The system abstracts LLM interaction via the `LlmClient` trait, allowing seamless switching between Gemini, Aliyun, and OpenAI-compatible endpoints at runtime. It supports both standard DashScope and specialized Aliyun Coding Plan endpoints by providing the full path in `base_url`.

## 7. Operational Safety
- **Strict Plan Enforcement:** The `TaskPlanTool` is integrated into the system prompt. If a plan exists (`.rusty_claw_task_plan.json`), it is injected into the context, and the model is explicitly instructed to follow it.
- **State Persistence:** Plan state is persisted to disk JSON, ensuring recovery after restart.

## 8. Context Architecture & Management

The `AgentContext` (`src/context.rs`) is the heart of the agent's memory and self-awareness. It employs a tiered strategy to handle long-running sessions.

### 8.1 Multi-Layer Context Assembly
Every prompt sent to the LLM is dynamically assembled from multiple sources:
1.  **Identity & System Prompts:** Core persona and hard constraints.
2.  **Runtime Environment:** OS, Architecture, Current Working Directory.
3.  **Project Context:** Static files (`README.md`, `AGENTS.md`) providing domain knowledge.
4.  **Task Plan:** Real-time state of the `.rusty_claw_task_plan.json`.
5.  **Dialogue History:** Previous turns, compressed via intelligent truncation.
6.  **Retrieved Memory:** Semantic snippets from the RAG subsystem.

### 8.2 Intelligent Token Budgeting & Compaction
To prevent context overflow while maintaining logical continuity:
-   **Token Estimation:** Uses `tiktoken-rs` for precise pre-flight token counting.
-   **Budget Allocation:** History is allocated ~85% of the `max_history_tokens`.
-   **Tiered Truncation:**
    -   **Tool Result Compression:** Large tool outputs (e.g., `cat` of a huge file) in history are compressed to "Head 6k + Tail 6k" chars to preserve context and potential errors while discarding middle noise.
    -   **History Squashing:** Oldest turns are dropped first if the budget is exceeded.
-   **Focus Booster:** When history is long, the system injects a `[SYSTEM NOTE]` into the latest user message to refocus the model on the new instruction.

### 8.3 Context Persistence
-   **Session Transcripts:** Every turn is serialized to a `.jsonl` transcript file (`logs/{session_id}.jsonl`).
-   **Atomic Recovery:** Sessions can be fully restored from transcripts, allowing the agent to "resume" work across restarts with perfect memory of previous tool results.

## 9. Dual-Phase Task Execution Flow

To optimize for both intelligence and token efficiency, Rusty-Claw implements a two-stage processing pipeline for every user request:

### Phase 1: Pre-Analysis (The Lead Architect)
- **Tool:** `src/core.rs -> analyze_request()`
- **Prompt:** Minimalist "Senior Technical Lead" instruction.
- **Context:** User input only (no project files or environment data).
- **Output:** A JSON plan containing `is_complex`, `reasoning`, and a list of `plan` steps.
- **Goal:** Determine if the task needs a structured multi-step approach without wasting tokens on the full identity prompt.

### Phase 2: Execution (The Engineer)
- **Tool:** `src/core.rs -> step()` loop.
- **Prompt:** Full "Rusty-Claw Engineer" identity.
- **Context:** Comprehensive assembly including `AGENTS.md`, `README.md`, Runtime Environment, Task Plan, and RAG-retrieved memory.
- **Goal:** Execute the plan, handle errors, and autonomously call tools until the goal is achieved.

## 10. Self-Healing & Rethink Mechanism

Rusty-Claw doesn't just execute; it monitors its own progress and intervenes when it detects failure patterns.

### 10.1 Proactive Intervention Triggers
-   **Low Energy Alert:** Notifies the model when `energy_points <= 5` to encourage wrapping up.
-   **Stagnation Detection:** Triggers a `SYSTEM ALERT` if the model takes 15 steps without completion or loops on identical tool calls.
-   **Failure Analysis:** After 3 consecutive tool failures, the system injects a "STOP and THINK" prompt, forcing the model to analyze the error before retrying.

### 10.2 Reliable Network Layer
The `LlmClient` implements an **Exponential Backoff Retry** strategy for network-level failures:
-   **Transient Errors:** 429 (Rate Limit) and 5xx (Server Error) trigger up to 5 retries.
-   **Visibility:** Detailed diagnostic logs record every attempt, status code, and error body for transparent debugging.
## 8. Dual-Phase Task Execution Flow

To optimize for both intelligence and token efficiency, Rusty-Claw implements a two-stage processing pipeline for every user request:

### Phase 1: Pre-Analysis (The Lead Architect)
- **Tool:** `src/core.rs -> analyze_request()`
- **Prompt:** Minimalist "Senior Technical Lead" instruction.
- **Context:** User input only (no project files or environment data).
- **Output:** A JSON plan containing `is_complex`, `reasoning`, and a list of `plan` steps.
- **Goal:** Determine if the task needs a structured multi-step approach without wasting tokens on the full identity prompt.

### Phase 2: Execution (The Engineer)
- **Tool:** `src/core.rs -> step()` loop.
- **Prompt:** Full "Rusty-Claw Engineer" identity.
- **Context:** Comprehensive assembly including `AGENTS.md`, `README.md`, Runtime Environment, Task Plan, and RAG-retrieved memory.
- **Goal:** Execute the plan, handle errors, and autonomously call tools until the goal is achieved.
