# OpenClaw Context & Memory Management: Deep Dive

This document provides an in-depth analysis of how OpenClaw manages Agent Context and Memory, based on the source code. In complex Agent systems, the "selection and assembly" of memory and the "storage and lifecycle management" of context are the core factors determining the lower bound of an Agent's intelligence.

OpenClaw adopts a **Dual-Layer Architecture**, physically separating short-term session context (Short-term Context) from long-term knowledge base memory (Long-term RAG Memory), and combining them via a sophisticated prompt assembly pipeline before feeding them into the LLM.

---

## 1. Architecture Overview

OpenClaw's context system consists of three core layers:

1.  **Short-Term Memory (Session History)**: Responsible for multi-turn dialogue history within a single session, persisted in JSONL format to ensure immediate conversational coherence.
2.  **Long-Term Memory (RAG)**: Responsible for cross-session knowledge accumulation. Implemented using SQLite (`sqlite-vec` + `FTS5`) for hybrid search combining vector similarity and full-text keyword matching.
3.  **Context Assembly Pipeline**: Responsible for dynamically assembling, pruning, and formatting the System Prompt, Tool Definitions, Short-term History, Long-term Memory, and Workspace State into the final Prompt before every LLM call.

---

## 2. Short-Term Memory (Session History) Management

To solve the problem of "poor memory selection and assembly," OpenClaw exercises extremely fine-grained control over short-term dialogue history.

### 2.1 Storage Layer: JSONL Transcripts
Session history is not kept in a simple in-memory array but is persisted to `~/.openclaw/sessions/<sessionId>.jsonl` via the `SessionManager`.
Every turn of dialogue appends a JSON line (containing role, content, timestamp, token usage, etc.).
**Design Advantage**: Extremely low write overhead and perfect recovery after Agent crashes/restarts.

### 2.2 Registry Layer: Session Store
The `src/config/sessions/store.ts` maintains a central registry `sessions.json`. This registry holds `SessionEntry` records (defined in `src/config/sessions/types.ts`), tracking:
*   `sessionId`: Unique identifier.
*   `inputTokens` / `outputTokens`: For monitoring context window consumption.
*   `compactionCount`: How many times the current session has been "compacted/summarized".

### 2.3 Context Assembly & Truncation (Sanitation)
Simply reading JSONL and throwing it at the LLM is insufficient (prone to context explosion). OpenClaw implements a processing pipeline:

*   **Turn Limits**: Strictly limits the number of history turns passed to the LLM based on configuration, pruning overly old dialogue.
*   **Tool Result Truncation**: A critical design. If a tool (like `Bash` or `Read`) returns massive text (e.g., a 5MB log), retaining it in history would crash subsequent turns due to token limits. This module uses heuristic algorithms to truncate overly long tool outputs, keeping only the head or tail key information.
*   **Provider Sanitization**: Different models have different history requirements. For example, Gemini requires strict User/Model alternation, while Anthropic may not support certain specific Role combinations. Before sending to the model, the system cleans the format (fixing turn ordering, dropping thinking blocks).

---

## 3. Long-Term Memory (RAG) Management

OpenClaw builds a built-in local RAG system designed to solve "cross-session memory" and "knowledge precipitation" problems.

### 3.1 Data Structure (SQLite Driven)
Core implementation lies in `src/memory/manager.ts` and `src/memory/memory-schema.ts`. OpenClaw uses SQLite as the local storage engine:

*   **`files` table**: Tracks indexed original files (like `MEMORY.md` or history Sessions), recording Hash and modification time for incremental updates.
*   **`chunks` table**: Slices long text into chunks, recording source, line number, and original Embedding data.
*   **`embedding_cache` table**: Persistent cache for embeddings. Calling OpenAI/Gemini APIs for embeddings is slow and costly; this table avoids re-computation for identical content.
*   **`chunks_vec` (Virtual Table)**: Introduces the `sqlite-vec` extension, specialized for vector search based on Cosine Distance.
*   **`chunks_fts` (Virtual Table)**: Introduces SQLite native `FTS5` (Full-Text Search) for keyword-based BM25 retrieval.

### 3.2 Hybrid Search Strategy
To address the issue where pure vector search often "misses" specific method names or variable names, OpenClaw employs hybrid retrieval in `src/memory/manager-search.ts`:

1.  **Dual Recall**: Simultaneously triggers `chunks_vec` (vector similarity) and `chunks_fts` (keyword matching).
2.  **Dynamic Weighting (Score Merging)**: Normalizes and performs a weighted sum of the two scores using preset `vectorWeight` and `textWeight`.
3.  **Query Expansion**: Before executing FTS search, it attempts to extract core keywords from the user's natural language Query to improve BM25 recall.
4.  **Temporal Decay**: Older data is often less valuable. The system optionally applies a time penalty function to prioritize recent memories.
5.  **Maximal Marginal Relevance (MMR)**: To avoid retrieving three Context chunks that all say the same thing, MMR algorithm is optionally used to increase result diversity.

---

## 4. Final Prompt Assembly Logic

The final link in context management is how to cleanly feed the extracted short memory, long memory, and system instructions to the model. This logic is centralized in `src/agents/pi-embedded-runner/system-prompt.ts`.

**System Prompt Hierarchy**:
1.  **Identity & Soul**: Loads persona and core guiding principles (e.g., from `SOUL.md`) with highest priority.
2.  **Runtime Info**: Injects metadata about the current running environment (OS, Node version, current model). Critical prior knowledge for Agents needing to write code or execute commands.
3.  **Tooling Context**: Dynamically injects the current list of enabled tools and their Schemas. OpenClaw supports dynamic tool enabling/disabling based on Sandbox Policy.
4.  **Skills Context**: Injects specific "skills" actively activated by the user (e.g., `test-driven-development`).
5.  **Workspace Context**: Injects specific convention files from the current working directory (e.g., `AGENTS.md`, `TOOLS.md`).
6.  **Retrieved Memory**: If long-term memory search is triggered, the recalled Top-K Chunks are assembled into this section.

### Context Overload Protection: Compaction
When the assembled tokens approach the model's Context Window limit, `src/agents/pi-embedded-runner/compact.ts` is triggered. It offloads old session history (Transcript) to a cheaper model (like `gpt-4o-mini` or `gemini-flash`) to generate a concise Summary, then replaces dozens of raw old dialogue turns with this Summary. This ensures the system doesn't crash (OOM or error) under extremely long conversations.

---

## 5. Claw-Context Profiler (New!)

To assist developers and users in analyzing the health of their Agent's context, a standalone Python tool **Claw-Context Profiler** has been added to the repository (`context-profiler/`).

### Features
*   **Completeness Audit**: Checks if the context contains the latest user intent, valid system prompts, and RAG memory connections.
*   **Redundancy Detection**: Automatically identifies "High Volume" tool outputs (e.g., 2000+ tokens of logs) and "Oscillation" (repetitive error loops).
*   **Visual Timeline**: Renders an ASCII heatmap in the terminal, showing token growth per turn and highlighting red-flag turns.
*   **File Usage Report**: Analyzes `read_file` calls to show which files are consuming the most context budget.

### Usage
1.  Run `/context dump` in the Rust CLI to generate `debug_context.json`.
2.  Run `python3 context-profiler/run_profiler.py audit debug_context.json`.

---

## 6. Summary & Takeaways

If your Agent system hits bottlenecks in context and memory management, OpenClaw's implementation offers several valuable reference points:

1.  **Don't Mix Long/Short Memory**: Short memory needs coherence and zero latency (JSONL is best); long memory needs fuzzy matching and generalization (SQLite Vec + FTS5 is best).
2.  **Defensive Context Management**: **Tool Result Truncation** must be implemented; this is the Achilles' heel where monolithic Agents die most easily (e.g., accidentally `cat`ing a 10MB bundle.js).
3.  **Hybrid Search is Standard**: Pure Vector retrieval performs poorly for code or precise instruction scenarios. Must combine with Full-Text Search (BM25), which is RAG best practice.
4.  **Dynamic Prompt Assembly**: Don't hardcode System Prompts. Dynamically generating structured System Prompts based on tools, environment, and recalled memory significantly improves the probability of the model following instructions.
5.  **Observability is Key**: Use tools like `Claw-Context Profiler` to visualize *where* your tokens are going. You can't optimize what you can't measure.
