# Context Management Implementation Plan For Rusty-Claw

## Purpose

This document turns `CONTEXT_MANAGEMENT_DESIGN.md` into an implementation plan that can be reviewed before coding starts.

The plan is intentionally concrete. It defines:

- what will be built
- what files and modules will change
- what schemas and storage formats will exist
- what migration steps will be used
- how the work will be validated

The goal is to let reviewers approve or reject a specific execution path, not a vague architecture direction.

## Scope

This implementation covers the context-management foundation for a local coding assistant:

- event-sourced task state
- materialized task-state views
- artifact-backed tool outputs
- structured evidence objects with freshness tracking
- cache-aware prompt assembly
- deterministic context eviction
- typed telemetry and versioned schemas

This plan does not yet include:

- UI dashboards
- remote telemetry backends
- distributed execution
- multi-agent orchestration
- advanced learned memory-ranking models

## Non-Goals

The first implementation should not try to solve everything at once.

Non-goals for this phase:

- replacing the existing agent loop with a new planner
- changing the full tool protocol
- redesigning the RAG ranking algorithm
- introducing protobuf or gRPC immediately
- fully removing legacy transcript behavior on day one

The objective is a controlled migration, not a rewrite.

## Target Outcomes

After this implementation:

- the source of truth for task progression is an append-only event log
- `task_state.json` becomes a derived snapshot, not mutable authority
- large tool outputs stop polluting prompt history by default
- retrieved evidence can be invalidated when workspace files change
- prompt assembly becomes stable-to-volatile to improve caching
- context eviction becomes deterministic and inspectable
- external analysis tools can consume versioned machine-readable state

## Deliverables

### D1. Event Log

Per-session append-only file:

- `rusty_claw/sessions/<session_id>/events.jsonl`

This is the authoritative record of task progression.

### D2. Materialized Task State

Per-session derived snapshot:

- `rusty_claw/sessions/<session_id>/task_state.json`

This is rebuilt or incrementally updated from the event stream.

### D3. Artifact Store

Per-session artifact directory:

- `rusty_claw/artifacts/<session_id>/<run_id>/...`

Large tool outputs are stored here with typed metadata.

### D4. Evidence Model

Structured evidence objects with freshness/version metadata.

### D5. Context Assembler

A dedicated assembly path that:

- orders prompt blocks for cache stability
- reconciles stale evidence
- evicts deterministically under token pressure

### D6. Telemetry Hooks

Machine-readable spans and metrics around:

- retrieval
- prompt assembly
- compaction
- tool execution
- artifact creation
- memory promotion

## Proposed Module Changes

### New modules

The implementation should introduce these modules:

- `src/event_log.rs`
- `src/task_state.rs`
- `src/artifact_store.rs`
- `src/evidence.rs`
- `src/context_assembler.rs`
- `src/schema.rs`
- `src/telemetry.rs`

### Existing modules to modify

- `src/context.rs`
- `src/core.rs`
- `src/session_manager.rs`
- `src/memory.rs`
- `src/rag.rs`
- `src/tools.rs`
- `src/main.rs`

### Responsibility split

#### `src/event_log.rs`

Responsibilities:

- append events
- read events for a session
- filter by task or turn
- replay events into task state

#### `src/task_state.rs`

Responsibilities:

- define task-state structs
- define reducers from event log to current state
- read/write materialized views

#### `src/artifact_store.rs`

Responsibilities:

- write artifacts
- generate artifact metadata
- return artifact references
- support artifact lookup by ID

#### `src/evidence.rs`

Responsibilities:

- define evidence object schema
- manage source version metadata
- validate freshness
- generate tombstones for invalidated evidence

#### `src/context_assembler.rs`

Responsibilities:

- task-type-aware assembly
- stable-to-volatile prompt ordering
- per-layer token budgeting
- deterministic eviction
- build final `PromptReport`

#### `src/schema.rs`

Responsibilities:

- centralize schema versions
- define schema identifiers
- expose helpers for version stamping

#### `src/telemetry.rs`

Responsibilities:

- emit OTel spans
- publish counters and histograms
- isolate telemetry concerns from orchestration logic

## Data Contracts

All persisted state must include `schema_version`.

### Event schema

Suggested structure:

```json
{
  "schema_version": 1,
  "event_id": "evt_123",
  "event_type": "ToolExecutionFinished",
  "session_id": "cli",
  "task_id": "task_456",
  "turn_id": "turn_789",
  "timestamp": 1735689600,
  "payload": {
    "tool_name": "read_file",
    "status": "ok",
    "artifact_id": "art_001",
    "summary": "Read Cargo.toml"
  }
}
```

### Task state schema

Suggested structure:

```json
{
  "schema_version": 1,
  "task_id": "task_456",
  "derived_from_event_id": "evt_999",
  "derived_at": 1735689620,
  "status": "in_progress",
  "goal": "Fix context management",
  "current_step": "Implement event log",
  "plan_steps": [],
  "open_questions": [],
  "active_files": [],
  "evidence_ids": [],
  "artifact_ids": []
}
```

### Evidence schema

Suggested structure:

```json
{
  "schema_version": 1,
  "evidence_id": "ev_123",
  "source_kind": "file",
  "source_path": "src/context.rs",
  "source_version": "sha256:abcd",
  "retrieved_at": 1735689600,
  "score": 0.82,
  "summary": "History compaction logic",
  "content": "..."
}
```

### Artifact metadata schema

Suggested structure:

```json
{
  "schema_version": 1,
  "artifact_id": "art_001",
  "session_id": "cli",
  "task_id": "task_456",
  "source_tool": "execute_bash",
  "content_type": "text/plain",
  "semantic_type": "command-output",
  "path": "rusty_claw/artifacts/cli/run_001/output.log",
  "summary": "cargo test output",
  "created_at": 1735689605
}
```

## Prompt Assembly Strategy

### Assembly order

Prompt blocks should be assembled in this order:

1. system instructions and tool schemas
2. durable memory and stable project rules
3. stable evidence for the current task phase
4. materialized task-state summary
5. volatile run context
6. transcript tail

This order is chosen to maximize prompt-cache reuse and reduce invalidation from highly volatile content.

### Why this order

- system instructions are stable across many turns
- project rules and durable memory change slowly
- evidence often remains stable within a task phase
- task state changes turn-by-turn, but not token-by-token
- run context and transcript tail are the most volatile

### Prompt report extensions

`PromptReport` should be extended to include:

- cache-stable token count
- volatile token count
- evicted item count
- evicted item labels
- stale evidence refresh count

## Deterministic Eviction Strategy

### Candidate model

Each prompt candidate item should have:

- `id`
- `kind`
- `priority_score`
- `token_cost`
- `layer`
- `required`

### Allocation rule

The assembler should:

1. compute token cost for all candidates
2. reserve space for required items
3. sort optional items by stable deterministic priority
4. include items until budget is exhausted
5. emit tombstones for evicted high-signal items

### Determinism rules

To make eviction replayable:

- scores must be explicit
- tiebreakers must be stable
- ordering cannot depend on hash-map iteration

### Tombstones

Tombstones should be short and explicit:

- `[Evidence 'README.md' evicted due to context budget]`
- `[Transcript turn 12 omitted due to low priority]`

## Evidence Reconciliation

Before the prompt is assembled, all referenced evidence objects in task state must go through reconciliation.

### Reconciliation checks

- current file hash equals stored file hash
- current file mtime is not newer than evidence timestamp
- current git blob hash matches, if available
- vector index generation timestamp is compatible

### Outcomes

- `fresh`: evidence is safe to use
- `refresh_required`: re-fetch source and replace evidence
- `invalidated`: evidence removed and tombstone inserted

### Initial implementation choice

The first version should use:

- file mtime
- file content hash

That is enough to prevent most stale-file hallucinations without overcomplicating the first pass.

## Telemetry Plan

### OTel spans

Initial spans:

- `agent.turn`
- `context.assemble`
- `context.reconcile_evidence`
- `retrieval.search`
- `tool.execute`
- `artifact.write`
- `memory.promote`

### Metrics

Initial metrics:

- prompt tokens by layer
- assembly latency
- retrieval latency
- stale evidence count
- evicted context items
- artifact creation count
- long-output artifactization rate

### Logging correlation

Each turn should have:

- `session_id`
- `task_id`
- `turn_id`
- `event_id`

This makes logs, telemetry, and persisted state easy to correlate.

## Coding Task Checklist

This checklist is the intended coding breakdown after the implementation plan is approved.

### Track A: Foundations

- add `src/schema.rs` with schema constants and shared version helpers
- add storage path helpers for sessions, artifacts, state, and event logs
- define correlation ID helpers for `session_id`, `task_id`, `turn_id`, and `event_id`
- add feature flags or config gates where needed to roll out incrementally

### Track B: Event Log

- create `src/event_log.rs`
- define `AgentEvent` enum and event payload structs
- implement append-only JSONL writer
- implement event reader and replay iterator
- add event emission for:
- `TaskStarted`
- `ToolExecutionStarted`
- `ToolExecutionFinished`
- `ArtifactCreated`
- `TaskFinished`
- add unit tests for event serialization and replay ordering

### Track C: Task State Materialization

- create `src/task_state.rs`
- define `TaskStateSnapshot` and reducer logic
- implement replay-to-state materialization
- implement write/read of `task_state.json`
- replace direct prompt-driven task plan usage with derived task state summary
- add tests proving replay produces deterministic task state

### Track D: Artifact Store

- create `src/artifact_store.rs`
- define artifact metadata model
- implement artifact ID generation
- implement file layout under `rusty_claw/artifacts/...`
- integrate artifact creation into tool execution flow for large outputs
- add tests for artifact pathing, metadata generation, and large-output routing

### Track E: Evidence Model And Freshness

- create `src/evidence.rs`
- define evidence object schema and freshness fields
- implement file hash and mtime capture
- implement freshness validation
- implement invalidation and tombstone generation
- update retrieval interfaces to produce evidence objects
- add tests for stale-file detection and evidence invalidation

### Track F: Context Assembler

- create `src/context_assembler.rs`
- define prompt candidate model with `priority_score`, `token_cost`, `required`, and `layer`
- implement stable-to-volatile block ordering
- implement deterministic eviction
- implement prompt tombstone insertion
- extend `PromptReport` with eviction and cache-oriented fields
- add deterministic tests for prompt assembly under fixed inputs

### Track G: Core Integration

- wire event emission into `src/core.rs`
- route large tool results through artifact store
- load task state and evidence through the new assembler path
- preserve existing cancel/recovery behavior during the migration
- add integration tests around full turn execution

### Track H: Session Management

- extend `src/session_manager.rs` to initialize event log, task state, transcript, and artifact roots
- ensure session restore loads materialized state consistently
- add tests for restoring sessions from event log plus transcript

### Track I: Telemetry

- create `src/telemetry.rs`
- wrap prompt assembly, retrieval, tool execution, artifact writes, and memory promotion in spans
- add counters and histograms
- add correlation IDs into logs
- add tests or golden assertions for emitted span fields where feasible

### Track J: Cleanup And Migration

- keep legacy transcript behavior readable during migration
- add migration notes for old `.rusty_claw_task_plan.json` usage
- remove or deprecate prompt-path code once task state is authoritative
- update docs after code lands

## Quality Assurance Measures

Quality gates should be explicit before implementation starts.

### Testing strategy

We should use four layers of testing:

- unit tests for event schemas, reducers, evidence freshness, eviction, and artifact metadata
- integration tests for full turn execution across `core + context + tools + state`
- regression tests for session restore, compaction, and prompt assembly stability
- golden tests for prompt assembly output and tombstone behavior

### Determinism checks

The following parts must be deterministic under fixed input:

- event emission ordering
- task-state materialization
- prompt candidate sorting
- eviction decisions
- tombstone generation

Recommended checks:

- same input fixtures produce byte-identical `task_state.json`
- same prompt candidates produce identical eviction outputs
- hash-map iteration is never used as a visible ordering source

### Failure-mode tests

We should explicitly test:

- partial event-log corruption
- artifact write failures
- stale evidence after file mutation
- over-budget prompt assembly
- session restore with missing derived state but intact event log
- cancellation during tool execution

### Backward-compatibility checks

During migration, the system should still tolerate:

- existing transcripts
- missing new state files
- sessions created before event sourcing exists

Recommended checks:

- if `task_state.json` is missing, rebuild from `events.jsonl`
- if `events.jsonl` is missing for legacy sessions, fall back safely without crashing

### Review gates before merge

Each implementation phase should not merge without:

- passing tests for the new module
- passing at least one integration path through the agent loop
- updated schema examples in documentation if persisted formats change
- explicit sign-off that no silent context drops were introduced

## Observability And Analysis Rollout Plan

The telemetry plan above defines what to emit. This section defines how it should be landed and used.

### Stage 1: Local structured traces

Goal:

- make agent execution analyzable locally before adopting a remote backend

Implementation:

- emit structured spans and events to local logs
- include `session_id`, `task_id`, `turn_id`, `event_id`
- add machine-readable JSON log mode if not already present

Outputs:

- easy local grep/debug
- artifact and state correlation during development

### Stage 2: Context observability

Goal:

- make context decisions explainable

Implementation:

- emit span data for:
- prompt assembly start/end
- evidence reconciliation outcomes
- eviction decisions
- compaction decisions
- include token counts by layer
- include evicted item labels

Outputs:

- reviewers can answer "why was this evidence included or dropped?"
- easier debugging of hallucinations caused by stale or evicted context

### Stage 3: Tool and artifact observability

Goal:

- make tool-side evidence generation traceable

Implementation:

- emit tool execution spans with duration, status, and artifact IDs
- emit artifact creation events with `content_type` and `semantic_type`
- link tool execution to downstream evidence objects

Outputs:

- easier debugging of cases where tool output was summarized incorrectly
- traceability from prompt evidence back to raw artifact

### Stage 4: Replay and offline analysis

Goal:

- support postmortem and dataset generation

Implementation:

- provide a small replay utility or internal API that consumes `events.jsonl`
- reconstruct task state timeline and major decisions
- export prompt assembly decisions for offline inspection

Outputs:

- root-cause analysis for failed runs
- training/evaluation dataset extraction

### Stage 5: External backend integration

Goal:

- integrate with standard APM tooling when needed

Implementation:

- wire OTel exporter configuration behind environment flags
- keep the local structured logs as the default fallback
- document the minimum supported attributes and metrics

Outputs:

- compatibility with Datadog, Grafana, Jaeger, or similar systems

### Minimum analyzability requirements

The implementation should not be considered complete unless an engineer can answer these questions from state plus telemetry:

- what did the agent know at turn N?
- what evidence was active, stale, refreshed, or evicted?
- which tool outputs became artifacts?
- which events caused the task state to change?
- why did the final prompt include one item but exclude another?

## Migration Plan

### Phase 0: Preparation

Goal:

- introduce schemas and storage paths without changing runtime behavior

Changes:

- add schema constants
- add storage directory helpers
- add empty event log writer

Acceptance:

- new modules compile
- no behavior changes

### Phase 1: Event Log Foundation

Goal:

- start recording append-only task events

Changes:

- emit `TaskStarted`
- emit plan and tool execution events
- create `events.jsonl` per session

Acceptance:

- every turn produces replayable events
- no task state yet depends on overwrite-only mutation

### Phase 2: Materialized Task State

Goal:

- derive task state from events

Changes:

- add reducers
- build `task_state.json`
- stop treating `.rusty_claw_task_plan.json` as the primary state container

Acceptance:

- deleting `task_state.json` and replaying events recreates the same state

### Phase 3: Artifact Store

Goal:

- externalize large tool outputs

Changes:

- add artifact writer
- store metadata next to artifacts
- update tool execution flow in `src/core.rs`

Acceptance:

- large tool outputs generate artifacts
- transcript and task state only keep summaries and references

### Phase 4: Evidence Objects And Freshness

Goal:

- stop relying on stale snippets

Changes:

- replace text-only retrieval outputs with evidence objects
- add freshness metadata
- add pre-assembly reconciliation pass

Acceptance:

- file mutations invalidate or refresh dependent evidence before prompt build

### Phase 5: Context Assembler

Goal:

- centralize prompt assembly and deterministic eviction

Changes:

- move assembly logic out of `AgentContext`
- add cache-aware ordering
- add candidate scoring and tombstones

Acceptance:

- prompt reports include evictions and cache-oriented stats
- assembly becomes deterministic under fixed inputs

### Phase 6: Telemetry

Goal:

- make execution externally analyzable

Changes:

- add OTel spans
- add counters and histograms
- add correlation IDs

Acceptance:

- execution lifecycle is visible in trace data

## File-Level Work Plan

### `src/context.rs`

Planned changes:

- reduce ownership of assembly details
- keep thin APIs for status, snapshot, and transcript support
- remove direct responsibility for most prompt block selection

### `src/core.rs`

Planned changes:

- emit events at key execution boundaries
- route large tool outputs into artifact store
- update materialized task state through reducers or state service

### `src/session_manager.rs`

Planned changes:

- initialize transcript, event log, task state, and artifact roots per session
- pass session-scoped stores into `AgentLoop`

### `src/rag.rs`

Planned changes:

- return structured evidence objects instead of text-only tuples at API boundary

### `src/tools.rs`

Planned changes:

- annotate outputs with artifactization hints where useful
- ensure tools return enough metadata for event emission and artifact typing

## Review Questions

These are the key questions reviewers should answer before coding begins:

1. Is `events.jsonl` the approved source of truth for task progression?
2. Is JSON Schema sufficient for v1, or should protobuf be introduced now?
3. Is file hash + mtime enough for v1 evidence freshness, or is git blob hashing required immediately?
4. Is the proposed cache-aware prompt ordering acceptable across all supported providers?
5. Is the deterministic eviction model acceptable, or do reviewers want a stricter algorithm now?
6. Are artifact storage paths and metadata fields sufficient for downstream tooling?

## Definition Of Done

The implementation is complete when:

- task progression is reconstructable from append-only events
- `task_state.json` is derived from events
- large tool outputs are artifactized
- evidence freshness checks prevent stale-file usage
- prompt assembly uses stable-to-volatile ordering
- context eviction is deterministic and visible
- persisted state has versioned schemas
- execution emits machine-readable telemetry

## Recommendation

This plan is intentionally staged so that reviewers can approve the direction without committing to a rewrite. The first coding milestone should be Phase 1 plus Phase 2, because event-sourced task state is the foundation for every later improvement.

If reviewers want the smallest acceptable starting point, implement:

- event log
- task-state materialization
- artifact store for large tool outputs

That gives the system replayability, recoverability, and cleaner prompt pressure immediately, while leaving retrieval and telemetry upgrades for follow-on phases.
