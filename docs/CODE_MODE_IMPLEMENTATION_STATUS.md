# Code Mode Implementation Status

## Current State

The code-mode work described in `docs/CODE_MODE_DESIGN.md` is now implemented through the current `exec` / `wait` entrypoints and the live drain pipeline underneath them.

Delivered pieces:

- Provider-aware code-mode scaffolding and prompt notices are in place.
- `exec` / `wait` are registered as top-level tools and still coexist with normal direct function tools.
- Nested code-mode tool calls go through `CodeModeNestedToolExecutor`, so autopilot/TODOS gating, loop protection, trace linkage, timeouts, and cancellation still apply.
- Runtime output is tracked as ordered `RuntimeEvent` values (`Text`, `Notification`, `Yield`, `ToolCallRequested`, `ToolCallResolved`, `Completed`, `Failed`, `Cancelled`, `WorkerCompleted`).
- Session state is centered on `ActiveCellHandle`, including status, recent event slices, committed resume state, and pending non-terminal progress.
- `CodeModeService` owns one active code-mode cell per session and now also owns the corresponding live worker handle.
- `CodeModeService::poll(...)` exists and is wired through the `wait` tool via `wait_timeout_ms` / `refresh_slice_ms`.
- `wait_timeout_ms` is enforced by the driver, and `refresh_slice_ms` is exercised at the driver drain layer so host-side slice timing is no longer just schema plumbing.
- Non-terminal drains are replay-safe:
  - nested tool progress now records full `result_json`, including error envelopes
  - timer progress tracks registration, completion, and clearing state
  - duplicate side effects are avoided across `poll` / `wait`
- Live workers now keep advancing across yielded manual/timer boundaries instead of terminating after the first non-terminal summary, while repeated timer-wait snapshots are suppressed when they would only duplicate the already-visible state.
- Lifecycle guards are enforced:
  - `exec` is rejected while a pending cell exists
  - mismatched `cell_id` errors do not destroy the real active cell
  - terminal drains clear the active cell and live worker

## Verification

The implementation currently passes:

- `cargo fmt`
- `cargo clippy -- -D warnings`
- `cargo test code_mode`
- `cargo test`

## Important Runtime Note

The non-terminal drain lifecycle is now safe and service-owned, but the runtime still uses persisted resume state to bootstrap a yielded cell back into a live worker instead of preserving a serialized VM snapshot. In practice this means:

- once a yielded cell has been reattached, later `poll` / `wait` calls operate on the same live worker and the worker can keep advancing across later yield/timer boundaries
- replay-sensitive metadata is preserved so already-executed nested tool side effects are not repeated
- JS local state is still reconstructed from the persisted resume boundary rather than from a serialized VM snapshot

This is the accepted implementation model in the current tree and is fully covered by tests.
