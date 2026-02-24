# Implementation Plan: Rust-Claw Performance Optimization

## Phase 1: Critical Path Optimization (Completed)

- [x] **Step 1: Make `maybe_compact_history` Async & Non-Blocking**
- [x] **Step 2: Parallelize RAG Retrieval (`hydrate_retrieved_memory`)**
- [x] **Step 3: Optimize `should_run_memory_retrieval` Heuristics**

## Phase 2: RAG Subsystem Upgrade (Completed)

- [x] **Step 1: Implement `EmbeddingCache` Struct (In-Memory + SQLite)**
- [x] **Step 2: Add Keyword Search Layer (FTS5)**
- [x] **Step 3: Implement Hybrid Scoring**

## Phase 3: Infrastructure & Cleanup (Focus: Stability & Polish)

The goal is to fix lingering inefficiencies and ensure the new subsystems are robust.

### Todo List

- [ ] **Step 1: Optimize Session Registry I/O**
    - [ ] **Analysis:** `SessionManager` rewrites the entire JSON registry on every interaction.
    - [ ] **Action:** Make `upsert_registry` async/debounced or just keep in memory and write on exit/periodic intervals.
    - [ ] **Verification:** Verify `sessions.json` is not being hammered.

- [ ] **Step 2: Final Code Review & Cleanup**
    - [ ] **Action:** Run `cargo fmt` and `cargo clippy`.
    - [ ] **Action:** Remove any temporary debugging print statements.
    - [ ] **Action:** Ensure error handling is graceful (no panics in background tasks).

- [ ] **Step 3: Update Documentation**
    - [ ] **Action:** Update `README.md` to reflect the new Architecture (Hybrid RAG, Local Embeddings).

---

I will start with **Step 1: Optimize Session Registry I/O**.
I'll modify `src/session_manager.rs` to only write to disk periodically or on significant state changes, rather than every read.
