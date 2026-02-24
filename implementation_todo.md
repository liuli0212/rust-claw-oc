# Implementation Plan: Rust-Claw Performance Optimization

## Phase 1: Critical Path Optimization (Completed)

- [x] **Step 1: Make `maybe_compact_history` Async & Non-Blocking**
- [x] **Step 2: Parallelize RAG Retrieval (`hydrate_retrieved_memory`)**
- [x] **Step 3: Optimize `should_run_memory_retrieval` Heuristics**

## Phase 2: RAG Subsystem Upgrade (Completed)

- [x] **Step 1: Implement `EmbeddingCache` Struct (In-Memory + SQLite)**
- [x] **Step 2: Add Keyword Search Layer (FTS5)**
- [x] **Step 3: Implement Hybrid Scoring**

## Phase 3: Infrastructure & Cleanup (Completed)

- [x] **Step 1: Optimize Session Registry I/O**
- [ ] **Step 2: Final Code Review & Cleanup**
    - [ ] **Action:** Update `README.md` to reflect the new Architecture.
    - [ ] **Action:** Run final check.

---

I'm proceeding to update `README.md` to document these massive performance improvements.
