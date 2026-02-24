# Rust-Claw Performance Optimization Plan

## 1. Executive Summary
The primary goal is to **reduce user-perceived latency** from 3-5s+ down to sub-1s for initial response. We will achieve this by decoupling blocking RAG operations from the main chat loop and optimizing the critical path.

## 2. Phase 1: Critical Path Optimization (Immediate Impact)
**Goal:** Stop blocking the main thread on every message.

- [ ] **Implement Parallel RAG Execution:**
    - Refactor `step()` to launch `hydrate_retrieved_memory` as a background `Future`.
    - Allow the LLM to start streaming *immediately* with available context.
    - *Constraint:* For strict Q&A, we might still need RAG results before generation. In that case, we implement a **Timeout/Race** strategy: wait max 800ms for local/cache results, then proceed without them or stream "Searching..." to the user.

- [ ] **Optimize `should_run_memory_retrieval`:**
    - Implement a stricter filter. Skip RAG for:
        - Messages < 5 words.
        - Greetings / Acknowledgments ("ok", "thanks").
        - Follow-up questions that clearly refer to immediate context (heuristic).

- [ ] **Asynchronous History Compaction:**
    - Move `maybe_compact_history()` out of the critical request path.
    - Trigger compaction *after* the response is sent to the user, or in a background thread.
    - Use a "soft limit" for context window to allow slightly larger history while compaction runs in the background.

## 3. Phase 2: RAG Subsystem Upgrade (Stability & Speed)
**Goal:** Make retrieval faster and smarter.

- [ ] **Implement Embedding Caching:**
    - Create a simple disk/memory cache (LRU) for query embeddings.
    - Key: `hash(query_text)`. Value: `vector`.
    - This eliminates the API call latency for repeated or similar queries.

- [ ] **Hybrid Search (Keyword + Vector):**
    - Add a simple BM25 or keyword matching layer (using `tantivy` or simple indexing).
    - Run keyword search (fast, <10ms) first. If high confidence matches found, skip vector search.
    - Combine results: `Score = 0.7 * Vector + 0.3 * Keyword`.

- [ ] **Local Embedding Option:**
    - Integrate `rust-bert` or `candle` to run a small embedding model (e.g., `all-MiniLM-L6-v2`) locally.
    - This removes network dependency entirely for the embedding step.

## 4. Phase 3: Infrastructure & I/O
**Goal:** Reduce overhead.

- [ ] **Optimize Session Registry:**
    - Load `sessions.json` once on startup.
    - Keep in memory.
    - Flush to disk asynchronously every 60s or on `SIGINT`/Exit, instead of every turn.

- [ ] **Connection Pooling:**
    - Ensure `GeminiClient` and HTTP clients reuse connections (Keep-Alive) to avoid TLS handshake overhead on every request.

## 5. Implementation Roadmap (Todo)

### Week 1: Unblocking the Loop
- [ ] [Core] Refactor `maybe_compact_history` to be background/async.
- [ ] [Core] Refactor `hydrate_retrieved_memory` to be concurrent/timeout-based.
- [ ] [Logic] Tune `should_run_memory_retrieval` heuristics.

### Week 2: RAG Caching & Hybrid
- [ ] [RAG] Implement `EmbeddingCache` struct (HashMap + Disk persistence).
- [ ] [RAG] Integrate `tantivy` or simple keyword index for local files.
- [ ] [RAG] Implement Hybrid scoring logic.

### Week 3: Cleanup & Polish
- [ ] [Sys] Optimize `SessionManager` file I/O.
- [ ] [UX] Add "Thinking..." or "Searching..." indicators in CLI/UI output during RAG wait.
