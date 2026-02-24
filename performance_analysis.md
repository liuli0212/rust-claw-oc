# Rust-Claw Performance Analysis

## Root Causes of Latency

Based on the code analysis of `rust-claw-oc`, the "slowness" you are experiencing is likely due to **sequential, blocking operations** that occur *before* the main LLM generation starts.

### 1. Synchronous RAG Retrieval on Every Turn
**Location:** `src/core.rs` -> `step()` -> `hydrate_retrieved_memory()`
**Impact:** High Latency (1-3s+)
**Analysis:**
- The agent calls `hydrate_retrieved_memory` at the very beginning of *every* user interaction (unless it detects "small talk").
- This function executes the `search_knowledge_base` tool, which likely involves:
  1.  Generating an embedding for your query (computationally expensive or API call).
  2.  Searching the vector store.
- This happens **sequentially** before the main LLM is even called. You are waiting for the memory search to finish before the bot starts thinking.

### 2. Token Counting & Potential Compaction
**Location:** `src/core.rs` -> `step()` -> `maybe_compact_history()`
**Impact:** Variable Latency (Low to Very High)
**Analysis:**
- Before every turn, the agent recalculates the token count of the entire history.
- If the history exceeds a threshold (`COMPACTION_TRIGGER_RATIO_NUM`), it triggers an **LLM call** to summarize the history (`summarize` prompt).
- This is a *blocking* operation. If compaction triggers, you wait for a full LLM generation cycle just to compress history, *before* your actual message is processed.

### 3. File I/O in Session Management
**Location:** `src/session_manager.rs` -> `upsert_registry`
**Impact:** Low (but non-zero)
**Analysis:**
- Every time you send a message, the system reads and rewrites the entire `sessions.json` file. While likely fast for small files, this is synchronous I/O that blocks the thread.

### 4. Sequential Tool Execution
**Location:** `src/core.rs` -> `step()` loop
**Impact:** High Latency during tool use
**Analysis:**
- If the agent decides to use multiple tools, they are executed **sequentially** in a loop.
- It waits for the tool to finish, then feeds the result back to the LLM, then waits for the LLM to generate the next token. There is no parallel execution of independent tools.

---

## Recommendations for Optimization

1.  **Parallelize Retrieval:**
    - Move `hydrate_retrieved_memory` to run in parallel with the initial LLM call (speculative execution) or make it asynchronous/non-blocking if possible (though the LLM needs the context).
    - **Quick Fix:** Optimize `should_run_memory_retrieval` to be more strict, or cache embeddings.

2.  **Async/Background Compaction:**
    - Do not block the user's turn for compaction. If history is getting full, trigger a background task to summarize it *after* replying to the user, or use a "soft limit" that allows going slightly over while the background task runs.

3.  **Optimize Registry I/O:**
    - Keep the session registry in memory and only flush to disk periodically or on exit, rather than on every single interaction.

4.  **UI/UX Feedback:**
    - The `CLIOutput` prints "Retrieving relevant memory..." but if the UI doesn't show this clearly, it just feels like lag. Better progress indicators help perceived latency.
