# Implementation Plan: Rust-Claw Performance Optimization

## Phase 1: Critical Path Optimization (Focus: Latency Reduction)

The goal is to stop blocking the main thread on every message for RAG and Compaction.

### Todo List

- [x] **Step 1: Make `maybe_compact_history` Async & Non-Blocking**
    - [x] **Analysis:** Currently, `maybe_compact_history` is awaited in `step()`. If it triggers, the user waits for a full summarization LLM call (~2-5s).
    - [x] **Action:** Move the compaction logic to a background task (tokio::spawn).
    - [x] **Action:** Use a "soft limit" for the context window check in the main loop to allow the conversation to proceed while compaction runs in the background.
    - [x] **Verification:** Logic verified by static analysis.

- [x] **Step 2: Parallelize RAG Retrieval (`hydrate_retrieved_memory`)**
    - [x] **Analysis:** `hydrate_retrieved_memory` runs sequentially before `context.start_turn`.
    - [x] **Action:** Refactor `step()` to spawn `hydrate_retrieved_memory` as a separate future.
    - [x] **Action:** Introduce a timeout (e.g., 800ms). If RAG takes too long, proceed without it (or with partial results) to maintain responsiveness.
    - [x] **Verification:** Logic verified by static analysis.

- [x] **Step 3: Optimize `should_run_memory_retrieval` Heuristics**
    - [x] **Analysis:** Currently runs on almost everything.
    - [x] **Action:** Implement stricter filters (skip short messages, greetings, simple acks).
    - [x] **Verification:** Logic verified by static analysis.

- [ ] **Step 4: Cleanup & Commit**
    - [ ] **Action:** Review code, run `cargo check`, commit changes.

---

I have implemented Steps 1, 2, and 3. The `src/core.rs` file now contains the optimized non-blocking logic.
