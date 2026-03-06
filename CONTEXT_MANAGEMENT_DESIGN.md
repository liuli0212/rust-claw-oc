# Context Management Design For Rusty-Claw

## Goal

This document proposes a production-oriented context management architecture for Rusty-Claw as a local coding assistant. The target is not "fit more tokens into the model window", but "keep the model focused on the minimum set of information needed to make the next correct decision".

The design is based on current agent best practices:

- layered memory instead of one flat prompt
- retrieval-on-demand instead of eager prompt stuffing
- artifact-backed execution instead of keeping raw tool outputs in history
- task-state-centric orchestration instead of prompt-centric orchestration
- resumable sessions with explicit handoff state

It also compares the target design with the current implementation and provides concrete migration advice.

## Design Principles

### 1. Prompt is the decision surface, not the storage layer

The prompt should contain the minimum decision-relevant state for the current turn. Anything large, reconstructable, or low-confidence should live outside the prompt.

### 2. State must be structured

Task progress, open questions, evidence references, and durable memory should be structured data. Natural-language summaries still matter, but they should be derived from structured state, not replace it.

### 3. Evidence should be referenceable

Large tool outputs, fetched documents, logs, and file snapshots should become artifacts with stable IDs. The prompt should usually contain summaries plus references, not raw blobs.

### 4. Context assembly must be dynamic

The context needed for a short Q&A is different from the context needed for a multi-step refactor. Context assembly should depend on task type and current task phase.

### 5. Compaction is not enough

Compaction helps, but it is not sufficient on its own. A robust agent needs explicit task state, durable memory, and artifact references so that long runs remain recoverable.

## Target Architecture

Rusty-Claw should move to a five-layer context model.

### L0: Run Context

This is the smallest layer and is always injected.

Contents:

- current user goal
- current turn index
- current task phase
- current working directory and active files
- last tool observation summary
- next intended action

Constraints:

- must stay small and stable
- should be recomputed each turn
- should not contain raw large outputs

### L1: Task State

This is the working memory for the active session or task.

Suggested fields:

- `task_id`
- `goal`
- `status`
- `plan_steps`
- `current_step`
- `open_questions`
- `active_files`
- `evidence_ids`
- `pending_actions`
- `completion_criteria`

Properties:

- structured, session-scoped
- persisted independently from the transcript
- summarized into prompt form by the context assembler

### L2: Durable Memory

This is cross-session, high-value memory.

Suggested namespaces:

- `user_pref`
- `project_rule`
- `codebase_fact`
- `operational_fact`

Suggested fields:

- `id`
- `namespace`
- `fact`
- `source`
- `confidence`
- `updated_at`
- `session_id`
- `project_scope`

Rules:

- only stable, reusable facts should be written here
- one-shot outputs and transient failures should not be stored here

### L3: Retrieval Layer

This layer provides on-demand context.

Sources:

- semantic RAG
- code search
- workspace memory
- web search
- logs
- databases or operational sources

The key design point is that retrieval should return structured evidence objects, not only concatenated text.

Suggested retrieval result format:

```json
{
  "id": "evidence_123",
  "source": "README.md",
  "kind": "file_snippet",
  "score": 0.84,
  "summary": "Build instructions for cargo test and clippy.",
  "content": "cargo test ...",
  "artifact_id": null
}
```

### L4: Artifact Store

This layer stores large outputs that should not remain in prompt or transcript.

Examples:

- large `read_file` results
- build logs
- test failures
- fetched web pages
- raw command output

Suggested storage pattern:

- `rusty_claw/artifacts/<session_id>/<run_id>/...`

Suggested metadata:

- `artifact_id`
- `source_tool`
- `created_at`
- `content_type`
- `summary`
- `path`

The model should normally see only the summary and artifact reference. The raw payload should be loaded only when needed.

## Runtime Flow

### Step 1: Classify the task

At turn start, classify the request into one of a small number of buckets:

- short answer
- code change
- investigation/debugging
- research
- long-running execution

This classification determines the context budget and which layers are loaded.

### Step 2: Build minimal run context

Assemble `L0` first:

- goal
- task phase
- last tool observation
- active step

This becomes the stable skeleton for the turn.

### Step 3: Inject task state summary

Load `L1 Task State` and inject a compact summary, not the full raw JSON.

Example:

```text
Current task:
- Goal: fix session resume bug
- Current step: inspect transcript load path
- Active files: src/session_manager.rs, src/context.rs
- Open question: whether old session paths are still supported
```

### Step 4: Retrieve only needed evidence

Use `L3 Retrieval` to fetch the highest-value evidence for the current turn.

Rules:

- retrieve top-k only
- deduplicate by source
- prefer summaries for large items
- avoid injecting the same source repeatedly if it is already in state

### Step 5: Execute tools and write artifacts

Tool outputs should be normalized before being added to conversational history.

For large outputs:

- write raw output to artifact store
- keep only a structured summary in task state
- add an artifact reference to the session state

### Step 6: End-of-turn memory policy

At turn end:

- update task state
- decide whether any facts qualify for durable memory
- never write transient noise into long-term memory

## Recommended Module Boundaries

The current `AgentContext` is doing too much. The system will be easier to reason about if responsibilities are separated more explicitly.

### `ContextAssembler`

Responsibilities:

- task-type-aware prompt assembly
- token budgeting
- prompt section prioritization
- evidence selection and truncation

Inputs:

- task state
- session transcript
- retrieval results
- durable memory
- runtime info

Outputs:

- prompt messages
- prompt report

### `TaskStateStore`

Responsibilities:

- persist structured session/task state
- update plan progress
- maintain active files, open questions, pending actions

Suggested file:

- `rusty_claw/sessions/<session_id>/task_state.json`

### `MemoryStore`

Responsibilities:

- durable memory write/read
- namespace separation
- confidence-aware storage

This can start as SQLite or structured JSON, but it should not remain a single `MEMORY.md`.

### `ArtifactStore`

Responsibilities:

- persist large tool outputs
- create summaries
- return `artifact_id`
- support later retrieval by ID

### `TranscriptStore`

Responsibilities:

- session transcript append/load
- keep raw conversation history separate from task state

This already exists conceptually, but should remain intentionally distinct from task state and durable memory.

## Comparison With Current Implementation

The current implementation already has several strong foundations:

- transcript persistence and session restore in `src/context.rs` and `src/session_manager.rs`
- context stats and prompt snapshots in `src/context.rs`
- history compaction in `src/context.rs`
- semantic retrieval in `src/rag.rs`
- task planning tool in `src/tools.rs`

These are the right building blocks. The main issue is that they are still organized around prompt assembly rather than around durable state and artifacts.

### Current Strengths

#### 1. Good visibility into prompt size

`AgentContext` already tracks detailed prompt stats and supports snapshots/diffs. This is valuable and should be preserved.

#### 2. Basic compaction exists

The project already supports rule-based compaction and smart truncation of older tool results.

#### 3. Retrieval exists

There is already a workable hybrid retrieval layer using embeddings plus FTS.

#### 4. Session persistence exists

The transcript model already enables resumability.

### Current Gaps

#### 1. System prompt is overloaded

Current behavior in `src/context.rs` assembles runtime info, AGENTS, README, MEMORY, task plan, and retrieved memory into the system prompt on nearly every turn.

This creates three problems:

- static context competes with dynamic task context
- the prompt budget is consumed by broad background material
- task-relevant information is not clearly prioritized

#### 2. Task plan is treated like prompt policy, not task state

The task plan is read from `.rusty_claw_task_plan.json` and injected into the system prompt as a strict directive.

This is too prompt-centric. A task plan should live in task state first, and only a compact summary should be injected into the prompt.

#### 3. Long-term memory is under-structured

`WorkspaceMemory` still treats memory as a single `MEMORY.md` file. This is too coarse for durable memory because:

- facts are not namespaced
- there is no confidence metadata
- there is no scope or source metadata
- updates are overwrite-based

#### 4. Retrieval returns text blobs instead of evidence objects

`VectorStore::search` returns tuples and higher layers convert them into text blobs. This works, but it limits:

- source-aware reasoning
- deduplication
- evidence reuse
- artifact linking

#### 5. Large tool outputs still flow through transcript and context

There is some output stripping and truncation, but the dominant strategy is still "put result in history, then compress it later".

That is workable for a prototype, but not for a long-running assistant. Large outputs should usually become artifacts immediately.

#### 6. `AgentContext` has too many responsibilities

Today it handles:

- transcript load/save
- system prompt assembly
- prompt metrics
- snapshot diffing
- compaction
- memory injection
- tool-result cleanup

This makes the module harder to evolve and harder to test in isolation.

## Explicit Recommendations

The following changes are the clearest path from the current design to a production-ready design.

### Priority 1: Introduce session-scoped task state

Replace prompt-first task plan usage with a real task-state layer.

Recommended change:

- create `rusty_claw/sessions/<session_id>/task_state.json`
- move plan, current step, open questions, active files, and evidence references there
- inject only a compact task-state summary into the prompt

Expected benefit:

- lower prompt noise
- better resumability
- cleaner separation between execution state and prompt policy

### Priority 2: Add artifact-backed tool output handling

Introduce an artifact store and stop treating large outputs as conversational history by default.

Recommended change:

- if tool output exceeds a configurable threshold, write it to an artifact file
- store only a structured summary in transcript/task state
- add an `artifact_id` reference

Expected benefit:

- major reduction in context load
- more stable long-running sessions
- cleaner postmortem/debug workflows

### Priority 3: Make prompt assembly task-type-aware

Stop using one fixed prompt assembly strategy for all request types.

Recommended change:

- add a lightweight task classifier
- define context profiles such as:
  - `QuickAnswerProfile`
  - `CodeChangeProfile`
  - `ResearchProfile`
  - `DebugProfile`

Expected benefit:

- more efficient prompt budgets
- less contamination from irrelevant background context

### Priority 4: Replace monolithic `MEMORY.md` with structured durable memory

Keep `MEMORY.md` if needed for human readability, but do not use it as the system of record.

Recommended change:

- create a structured store for durable memory
- include namespace, source, confidence, and timestamps
- only promote facts to durable memory through a filter

Expected benefit:

- higher signal memory
- less memory pollution
- easier future memory retrieval logic

### Priority 5: Return structured evidence from retrieval

Recommended change:

- change retrieval interfaces to return evidence objects
- let the prompt assembler choose how to compress them

Expected benefit:

- better prompt composition
- better explainability
- easier artifact integration

### Priority 6: Split `AgentContext` into smaller modules

Recommended change:

- keep `AgentContext` as a thin orchestration facade if desired
- move implementation into:
  - `ContextAssembler`
  - `TaskStateStore`
  - `ArtifactStore`
  - `MemoryStore`
  - `TranscriptStore`

Expected benefit:

- simpler mental model
- easier testing
- faster future iteration

## Suggested Migration Plan

### Phase 1: Low-risk changes

- add `TaskStateStore`
- move task plan persistence there
- keep existing prompt/report logic
- inject compact task state summary instead of full strict plan text

### Phase 2: Artifact introduction

- add `ArtifactStore`
- redirect large tool outputs to artifacts
- keep transcript references only

### Phase 3: Retrieval upgrade

- return structured evidence objects
- teach prompt assembler to select and compress them

### Phase 4: Durable memory upgrade

- add structured durable memory backend
- keep `MEMORY.md` as optional export or human-readable mirror

### Phase 5: Full context assembler split

- move prompt assembly logic out of `AgentContext`
- keep `AgentContext` focused on high-level orchestration

## Concrete File-Level Advice

### `src/context.rs`

Keep:

- prompt reporting
- snapshot and diff utilities

Change:

- remove direct ownership of task plan prompt policy
- reduce responsibility for transcript persistence and retrieval formatting
- move large-output artifact logic out of raw history processing

### `src/core.rs`

Keep:

- turn loop orchestration
- cancellation and recovery logic

Change:

- update task state explicitly after tool execution
- stop assuming history is the main working memory
- route large tool outputs into artifact storage

### `src/session_manager.rs`

Keep:

- session creation and restoration role

Change:

- let it initialize both transcript and task state stores
- keep session path conventions stable and scoped by session ID

### `src/memory.rs`

Change:

- evolve from one overwrite-style markdown file to a structured durable memory store

### `src/rag.rs`

Keep:

- hybrid retrieval approach

Change:

- return richer evidence objects
- support source IDs and dedup-friendly metadata

## Final Recommendation

Rusty-Claw already has the core ingredients of a serious context management system. The next step is not a bigger context window or more aggressive prompt packing. The next step is a structural shift:

- from prompt-centric to state-centric
- from transcript-centric to artifact-centric
- from unstructured memory to typed memory

If only one improvement is implemented next, it should be artifact-backed tool outputs plus session-scoped task state. That change will reduce prompt pressure, improve long-run stability, and create a cleaner foundation for every later improvement.
