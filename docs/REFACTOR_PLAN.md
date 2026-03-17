# Rusty-Claw Refactor Plan

## Goal

This document captures the structural problems found in the current codebase and defines a staged refactor plan that preserves behavior while improving maintainability.

The intent is not to rewrite everything at once. We will refactor incrementally, with tests protecting the critical execution paths before and during each step.

## Current Assessment

The codebase is functional, but several core modules have accumulated too many responsibilities:

- `src/core.rs` is both the agent runtime and the workflow coordinator.
- `src/context.rs` mixes message models, prompt building, token accounting, transcript persistence, compaction, and evidence management.
- `src/tools.rs` combines tool protocol definitions with many unrelated concrete tool implementations.
- `src/llm_client.rs` mixes provider selection, model heuristics, request shaping, and provider-specific implementations.
- `src/main.rs` and `src/session_manager.rs` contain too much application wiring and session-specific business logic.

This creates a system where behavior is spread across a few oversized files. The result is slow iteration, fragile changes, weak boundaries, and high regression risk.

## Key Problems

### 1. `AgentLoop` has too many responsibilities

`src/core.rs` currently handles:

- task lifecycle initialization and reset
- cancellation and loop termination
- prompt assembly orchestration
- LLM streaming
- streamed text parsing
- tool dispatch
- tool timeout and interruption handling
- finish-task detection
- evidence updates
- output rendering coordination
- plan update detection

The center of this problem is `AgentLoop::step()`, which has grown into a large flow function with many unrelated branches. It is hard to reason about and hard to test in isolation.

### 2. Tool protocol is weakly typed

Tools return `Result<String, ToolError>`, but much of the runtime expects structured JSON envelopes inside those strings. That means:

- correctness depends on convention rather than types
- `core.rs` has to inspect tool names and parse outputs manually
- adding or changing a tool can silently break runtime assumptions

This is visible in logic that special-cases `finish_task`, `read_file`, `send_file`, and `execute_bash`.

### 3. Context management is overloaded

`src/context.rs` defines message and turn models, but also owns:

- prompt section assembly
- token accounting
- history truncation
- transcript IO
- snapshot/diff reporting
- compaction logic
- evidence-related behavior

This makes `AgentContext` a catch-all object rather than a clear domain boundary.

### 4. Dynamic behavior is embedded in ad hoc branches

Examples:

- skill loading is repeated inside runtime flow
- evidence mutation depends on tool name string matching
- finish-task behavior exists both as text fallback logic and tool-call logic
- cancellation semantics are tied to loop structure rather than a focused execution abstraction

These branches make behavior harder to discover and harder to safely change.

### 5. Application wiring is mixed with business logic

`src/main.rs` currently builds tools, LSP clients, vector store, memory store, platform integrations, and session management directly.

`src/session_manager.rs` is responsible for:

- in-memory session lifecycle
- transcript loading
- registry persistence
- tool injection
- telemetry setup
- LLM replacement

This makes bootstrapping harder to change and makes tests more expensive to write.

### 6. Warning suppression is hiding design debt

Several core files use `#![allow(warnings)]`.

This is a symptom that the code is carrying dead code, deprecated APIs, and ownership/design issues that have been globally suppressed instead of made explicit and fixed. We should treat this as technical debt to remove, not as a permanent state.

## Refactor Principles

All refactor work should follow these rules:

1. Preserve behavior unless a change is explicitly intended.
2. Keep each step small enough to verify with targeted tests.
3. Move logic behind narrower interfaces before changing implementation.
4. Prefer extracting cohesive units over renaming or reformatting large areas.
5. Avoid introducing a second layer of accidental complexity while splitting modules.
6. Remove `#![allow(warnings)]` only after the owning module has been narrowed enough to fix warnings cleanly.

## Target Architecture

The codebase should converge toward these boundaries:

- `agent runtime`: execution state machine for a single task step
- `context domain`: conversation model, prompt inputs, transcript, compaction, token budgeting
- `tool runtime`: tool protocol, execution wrappers, structured tool results
- `provider layer`: LLM/provider-specific transport and request shaping
- `application wiring`: startup, session construction, integration bootstrapping

## Staged Plan

### Phase 0: Stabilize the safety net

Status: in progress

Objective:

- lock down the most fragile runtime behavior with tests before major movement

Completed so far:

- context truncation and transcript path sanitization tests
- task state summary and fallback-load tests
- event log append and malformed-line tolerance tests
- utility truncation tests
- tool protocol and validation tests
- `step()` behavior tests for empty input and cancellation during pending stream startup
- finish-task state transition test
- `core` helper extraction tests for think-block stripping and transient LLM error detection

Before moving deeper into refactor, keep adding tests only when they directly protect an extraction we are about to do.

### Phase 1: Shrink `AgentLoop`

Priority: highest

Objective:

- reduce `src/core.rs` from a monolithic orchestrator to a thin runtime coordinator

Status:

- completed

Sub-steps:

1. [x] Extract task lifecycle helpers.
   - initialization
   - completion
   - cancellation exit paths
   - task-state persistence hooks

2. [x] Extract streaming response processing.
   - receive stream events
   - buffer visible text
   - route thinking output
   - collect tool calls
   - normalize stream termination and stream interruption

3. [x] Extract tool dispatch.
   - tool lookup
   - timeout wrapper
   - interruption wrapper
   - result normalization
   - per-tool post-processing hooks

4. [x] Extract evidence updates.
   - read-file evidence insertion
   - bash diagnostic/directory evidence insertion
   - write-induced invalidation

5. [x] Extract post-tool state reconciliation.
   - reload task plan state
   - detect plan updates
   - determine finish conditions

Expected result:

- `AgentLoop::step()` becomes a high-level sequence of well-named method calls
- the flow is understandable without scrolling through all branches
- new behavior can be tested by component instead of only by full-step integration tests

Suggested intermediate modules:

- `core/task_lifecycle.rs`
- `core/stream_processor.rs`
- `core/tool_dispatch.rs`
- `core/evidence_updates.rs`

Completed notes:

- `AgentLoop::step()` now runs as a short orchestration loop over helper stages instead of one monolithic control-flow block
- extracted the step/runtime helper group into `src/core/step_helpers.rs`
- moved `core`-specific tests into `src/core/tests.rs` so production runtime code no longer shares the same file with the test harness

### Phase 2: Introduce a structured tool runtime

Priority: high

Objective:

- remove the dependency on stringly typed tool outputs inside runtime logic

Status:

- completed

Sub-steps:

1. Define a structured internal tool result type.
   - success/failure
   - display output
   - exit code
   - truncation flag
   - optional file path
   - optional structured payload

2. Keep compatibility at the boundary.
   - tools may still serialize envelopes externally at first
   - runtime should stop depending on hand-parsed string conventions

3. Move tool-specific schema helpers and execution helpers into protocol-focused modules.

4. Split `src/tools.rs` by domain.
   - completed via `src/tools/mod.rs`, `protocol.rs`, `bash.rs`, `files.rs`, `web.rs`, `memory.rs`, `integrations.rs`, and `lsp.rs`

Suggested structure:

- `src/tools/mod.rs`
- `src/tools/protocol.rs`
- `src/tools/bash.rs`
- `src/tools/files.rs`
- `src/tools/web.rs`
- `src/tools/memory.rs`
- `src/tools/integrations.rs`
- `src/tools/lsp.rs`

Expected result:

- runtime no longer branches on tool names just to understand tool output shape
- adding new tools becomes cheaper and less risky
- tests become more local

Completed notes:

- extracted tool protocol types and helpers into `src/tools/protocol.rs`
- split concrete tools across `src/tools/bash.rs`, `files.rs`, `web.rs`, `memory.rs`, `integrations.rs`, and `lsp.rs`
- tightened the public facade in `src/tools/mod.rs` so it exports the actually-consumed runtime entry points instead of broad wildcard re-exports
- tool results now consistently use envelope helpers, and runtime effect handling in `core` now consumes structured envelope metadata instead of hard-coded post-processing by tool name
- tool outputs now carry structured metadata for file evidence, bash evidence, finish-task summaries, plan payloads, and web payloads so context sanitization can keep moving off string matching

### Phase 3: Decompose `AgentContext`

Priority: high

Objective:

- split data model, prompt construction, history management, and persistence into separate concepts

Status:

- completed

Sub-steps:

1. Extract conversation model types.
   - `Message`
   - `Part`
   - `Turn`
   - `FunctionCall`
   - `FunctionResponse`

2. Extract prompt assembly support.
   - system prompt sections
   - token accounting
   - prompt reports

3. Extract history and compaction.
   - build-with-budget
   - recency rules
   - rule-based compaction
   - current-turn compression

4. Extract transcript persistence.
   - loading
   - appending archived turns
   - session transcript path handling

5. Extract context snapshots and diffs.

Suggested structure:

- `src/context/mod.rs`
- `src/context/model.rs`
- `src/context/prompt.rs`
- `src/context/history.rs`
- `src/context/transcript.rs`
- `src/context/snapshot.rs`

Expected result:

- `AgentContext` becomes a facade over smaller focused units
- compaction and prompt-building logic become independently testable
- transcript and context policies stop leaking into unrelated code

Completed notes:

- extracted `AgentContext` into `src/context/agent_context.rs`
- split prompt, history, transcript, report, sanitize, token, state, and turn-management helpers into focused modules under `src/context/`
- removed the temporary `src/context/legacy.rs` facade after internal callers were migrated to the new module layout

### Phase 4: Clean up provider/client boundaries

Priority: medium

Objective:

- separate provider factory logic from provider implementations

Status:

- completed

Problems to address:

- `src/llm_client.rs` is too large
- model heuristics and provider selection are mixed with concrete request/response code
- provider-specific complexity is not isolated enough

Sub-steps:

1. Extract provider factory and configuration mapping.
2. Move shared client abstractions to a small protocol layer.
3. Separate Gemini and OpenAI-compatible implementations into their own files.
4. Move model/context-window heuristics into a provider policy module.

Suggested structure:

- `src/llm_client/mod.rs`
- `src/llm_client/factory.rs`
- `src/llm_client/protocol.rs`
- `src/llm_client/gemini.rs`
- `src/llm_client/openai_compat.rs`
- `src/llm_client/policy.rs`

Expected result:

- provider code becomes easier to reason about
- adding a new provider no longer expands one giant file

Completed notes:

- extracted provider construction into `src/llm_client/factory.rs`
- extracted context-window heuristics into `src/llm_client/policy.rs`
- moved shared transport setup into `src/llm_client/protocol.rs`
- moved `GeminiClient` and its cache metadata into `src/llm_client/gemini.rs`
- removed the temporary `src/llm_client/legacy.rs` facade after `factory`, `openai_compat`, and `gemini_context` were migrated

### Phase 5: Simplify bootstrapping and session construction

Priority: medium

Objective:

- move application assembly out of `main.rs` and reduce `SessionManager` scope

Status:

- completed

Sub-steps:

1. Extract runtime bootstrap.
   - config loading
   - tool registry creation
   - shared dependency initialization

2. Narrow `SessionManager`.
   - session lookup/create
   - registry access
   - cancellation routing

3. Move session construction into a dedicated factory.
   - create context
   - attach transcript path
   - create telemetry/event log/task store
   - attach session-scoped tools

Suggested structure:

- `src/app/bootstrap.rs`
- `src/app/tool_registry.rs`
- `src/session/factory.rs`
- `src/session/repository.rs`

Expected result:

- `main.rs` becomes an entrypoint instead of a wiring dump
- session creation becomes easier to test and modify

Completed notes:

- extracted runtime bootstrap into `src/app/bootstrap.rs`
- added `src/session/factory.rs` for agent/session construction
- added `src/session/repository.rs` for session registry persistence
- narrowed `SessionManager` so it delegates persistence and construction instead of owning both
- moved headless and interactive CLI orchestration out of `src/main.rs` into `src/app/cli.rs`
- split Telegram adapter code so `src/telegram.rs` now primarily owns bot startup/dispatcher wiring while output rendering and update handlers live in `src/telegram/output.rs` and `src/telegram/handlers.rs`
- split ACP server code so `src/acp/mod.rs` now focuses on server wiring while route handlers, SSE output, and the console page asset live under `src/acp/`

### Phase 6: Remove warning suppression and tighten module APIs

Priority: medium

Objective:

- remove global warning suppression after modules are small enough to cleanly fix

Status:

- completed

Sub-steps:

1. Remove `#![allow(warnings)]` one core module at a time.
2. Fix dead code or make it explicit behind feature gates.
3. Reduce unnecessary `pub` surface area.
4. Replace incidental cross-module reach-through with focused public APIs.

Expected result:

- the compiler becomes an active tool again
- dead code and deprecated behavior stop accumulating silently

Completed notes:

- removed `#![allow(warnings)]` from `core`, `context`, and `llm_client`
- cleaned up obsolete `AgentLoop` constructor state introduced during extraction
- removed stale control-flow variables left behind by the `step()` split

## Execution Order

Recommended order:

1. Finish Phase 1 on `core`
2. Do Phase 2 for tool runtime
3. Do Phase 3 for context decomposition
4. Do Phase 5 for bootstrap/session cleanup
5. Do Phase 4 for provider cleanup
6. Finish with Phase 6 warning cleanup

Reasoning:

- `core` is the main pressure point and currently blocks safe change elsewhere
- tool runtime and context decomposition unlock most downstream cleanup
- provider cleanup can wait until runtime boundaries are stable

## Non-Goals

These are not immediate goals of the refactor:

- changing the product behavior of the agent
- redesigning prompts for quality improvements
- replacing providers or tools unless architecture work requires it
- introducing a new framework or major dependency without necessity

## Success Criteria

The refactor will be considered successful when:

- no single core module owns multiple unrelated responsibilities
- `AgentLoop::step()` reads as a high-level orchestration function
- tool results are understood structurally rather than via string conventions
- prompt/history/transcript logic are no longer bundled into one file
- `main.rs` is mostly startup and command routing
- warning suppression can be removed from core modules
- targeted regression tests continue to pass after each step

## Current Next Step

The original structural plan is complete. The best follow-up work now is a second pass on `core` and the remaining oversized adapters:

1. continue splitting `src/core.rs` into focused submodules now that `step()` helpers already live in `src/core/step_helpers.rs`
2. reduce `src/main.rs`, `src/telegram.rs`, and `src/acp.rs` by separating transport/adapter code from agent-session orchestration
3. do a second decomposition pass on `src/context/history.rs` and provider-heavy files such as `src/llm_client/gemini_context.rs`

That keeps the current momentum, while building on the now-stable runtime and tool boundaries.

## Current Recommendation

The highest-value next refactor is to keep shrinking the runtime and adapter hot spots that remain oversized after the first pass.

Why this is the best next step:

- `src/core.rs` is much smaller than before, but it still owns runtime types, orchestration entry points, and local test hooks in one top-level module
- `src/main.rs`, `src/telegram.rs`, and `src/acp.rs` still mix entrypoint or transport wiring with domain behavior
- `src/context/history.rs` and `src/llm_client/gemini_context.rs` are now the clearest “large concentrated logic” modules left in the system

Concretely, the remaining architectural gap is:

- the runtime/tool/context/provider boundaries are now much cleaner
- but a few top-level modules are still physically large enough that iteration cost and review cost remain higher than they should be

Recommended implementation order:

1. keep decomposing `core` until the top-level file is mostly runtime types plus the public orchestration entry points
2. separate adapter and session wiring concerns in `main`/`telegram`/`acp`
3. revisit `context/history` and provider request-shaping modules for a second size-reduction pass

This path continues the same refactor strategy, but shifts from “remove legacy boundaries and stringly-typed runtime behavior” to “finish physically shrinking the remaining large surfaces.”

## Post-Plan Progress

Follow-up decomposition completed so far:

- [x] moved `BashTool` into `src/tools/bash.rs`
- [x] moved `WebFetchTool` into `src/tools/web.rs`
- [x] moved file-oriented tools into `src/tools/files.rs`
- [x] moved memory, integration, and LSP tools into dedicated modules
- [x] removed `src/tools/legacy.rs`
- [x] extracted context model, prompt, history, and transcript data types from `src/context/legacy.rs`
- [x] extracted context transcript/report helpers from `src/context/legacy.rs`
- [x] removed `core`'s tool-name-based runtime post-processing in favor of structured tool metadata
- [x] removed the text-only `finish_task` completion fallback from `core`
- [x] moved `core` runtime helpers into `src/core/step_helpers.rs`
- [x] moved `core` tests into `src/core/tests.rs`
- [x] moved ACP route and SSE output logic into `src/acp/handlers.rs` and `src/acp/output.rs`
- [x] renamed the root LSP integration module from `src/lsp.rs` to `src/lsp_client.rs` to distinguish it from `src/tools/lsp.rs`
- [x] extracted prompt assembly and detailed context stats from `src/context/legacy.rs` into `src/context/prompt.rs`
- [x] extracted `llm_client` protocol types into `src/llm_client/protocol.rs`
- [x] extracted Gemini request/declaration types into `src/llm_client/gemini.rs`
- [x] extracted OpenAI-compatible client implementation into `src/llm_client/openai_compat.rs`
- [x] extracted Gemini vertex request/schema helpers into `src/llm_client/gemini.rs`
- [x] narrowed `src/context/mod.rs` to selective facade re-exports and removed stale bridge imports from `src/context/legacy.rs`
- [x] extracted context snapshot/diff helpers into `src/context/state.rs` and turn lifecycle helpers into `src/context/turns.rs`
- [x] extracted Gemini tool declaration, upload, dehydration, and context cache helpers into `src/llm_client/gemini_context.rs`
- [x] moved Gemini cached-content resolution and text generation config selection out of `src/llm_client/legacy.rs`
- [x] extracted response sanitization helpers from `src/context/legacy.rs` into `src/context/sanitize.rs`
- [x] extracted token and truncation helpers from `src/context/legacy.rs` into `src/context/token.rs`
- [x] extracted Gemini request URL selection and Vertex request serialization from `src/llm_client/legacy.rs`
- [x] moved transcript context loading/appending and shared BPE access out of `src/context/legacy.rs`
- [x] moved `AgentContext` implementation into `src/context/agent_context.rs` and reduced `src/context/legacy.rs` to a compatibility shim
- [x] extracted Gemini non-stream request sending from `src/llm_client/legacy.rs` into `src/llm_client/gemini_context.rs`
- [x] extracted Gemini structured retry handling from `src/llm_client/legacy.rs` into `src/llm_client/gemini_context.rs`
- [x] extracted Gemini streaming connection retry handling from `src/llm_client/legacy.rs` into `src/llm_client/gemini_context.rs`
- [x] extracted Gemini SSE data-block parsing and event emission from `src/llm_client/legacy.rs` into `src/llm_client/gemini_context.rs`
