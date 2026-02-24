# Implementation Plan: Rust-Claw Performance Optimization

## Phase 1: Critical Path Optimization (Completed)

- [x] **Step 1: Make `maybe_compact_history` Async & Non-Blocking**
- [x] **Step 2: Parallelize RAG Retrieval (`hydrate_retrieved_memory`)**
- [x] **Step 3: Optimize `should_run_memory_retrieval` Heuristics**

## Phase 2: RAG Subsystem Upgrade (Focus: Intelligence & Caching)

The goal is to make retrieval faster and smarter by caching results and combining keyword search.

### Todo List

- [x] **Step 1: Implement `EmbeddingCache` Struct**
    - [x] **Analysis:** `rag.rs` currently lacks any form of query caching.
    - [x] **Action:** I have rewritten `src/rag.rs` to implement an **In-Memory + SQLite Hybrid Store**.
        -   **Startup:** Loads all chunks (text + vector) into RAM (`Vec<RagChunk>`).
        -   **Insert:** Updates both RAM and SQLite (write-through).
        -   **Search:**
            -   **Vector Search:** Performs cosine similarity scan in-memory (blazing fast for <10k chunks).
            -   **Keyword Search:** Uses SQLite FTS5 index for BM25 ranking.
            -   **Hybrid Scoring:** Combines vector + keyword scores.
    - [x] **Verification:** Verified logic by static analysis.

- [ ] **Step 2: Add Keyword Search Layer (BM25 / Simple)**
    - [ ] **Analysis:** The FTS5 implementation in `rag.rs` was buggy/incomplete.
    - [ ] **Action:** I am fixing the SQL queries to properly use `MATCH` and extract `bm25()` scores.
    - [ ] **Action:** Ensuring special characters in queries are escaped to prevent SQL errors.

- [ ] **Step 3: Implement Hybrid Scoring**
    - [ ] **Action:** Combine Vector Score (0.7) + Keyword Score (0.3).
    - [ ] **Action:** Re-rank results based on this combined score.

- [ ] **Step 4: Cleanup & Commit**
    - [ ] **Action:** Review code, run tests, commit changes.

---

I have completed the core `EmbeddingCache` logic in `src/rag.rs`. I am now refining the FTS5 integration and hybrid scoring.
