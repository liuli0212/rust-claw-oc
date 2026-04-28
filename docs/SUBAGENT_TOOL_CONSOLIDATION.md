# Subagent Tool Consolidation

## Overview
The legacy subagent lifecycle used several separate tools:

- `dispatch_subagent`
- `spawn_subagent`
- `get_subagent_result`
- `cancel_subagent`
- `list_subagent_jobs`

That split made the tool surface noisy and hard for the model to reason about. The current design consolidates all subagent lifecycle work behind a single `subagent` tool.

## Current Interface

### `action="run"`
Minimal run arguments:

```json
{
  "action": "run",
  "goal": "Inspect the parser flow",
  "context": "We are debugging the parser refactor",
  "background": false
}
```

Supported aliases for compatibility:

- `input_summary` -> `context`
- `run_in_background` -> `background`

No other execution knobs are part of the public contract anymore. Deprecated fields such as `allowed_tools`, `claimed_paths`, `allow_writes`, `timeout_sec`, and `max_steps` are rejected.

### `action="status"`

```json
{
  "action": "status",
  "job_id": "sub_parent_123",
  "wait_sec": 20,
  "consume": true
}
```

### `action="cancel"`

```json
{
  "action": "cancel",
  "job_id": "sub_parent_123"
}
```

### `action="list"`

```json
{
  "action": "list"
}
```

## Delegation Rules

- Ordinary `subagent` sessions can use the normal toolset, including `call_skill`.
- Ordinary `subagent` sessions cannot spawn another ordinary `subagent`.
- `skill -> subagent` is allowed.
- `skill -> subagent -> skill` is allowed.
- Child skills may use `subagent` only if their frontmatter `allowed_tools` includes it.
- Skill-originated delegation inherits the same call tree and shared delegation budget.

## Tool Visibility Rules

- Skills still treat frontmatter `allowed_tools` as a hard limit.
- Runtime-essential tools such as `task_plan` are always added.
- Ordinary subagents default to the full non-recursive toolset.
- Skill-owned subagent sessions may opt into the `subagent` tool only when the skill contract explicitly allows it.

## Notifications and Status Retrieval

Background jobs can be observed in two ways:

- Poll with `subagent(action="status", job_id="...")`
- Wait for the next-turn notification injected by `SubagentNotificationExtension`

The notification contains the `job_id`, terminal status, and a short summary, and prompts the parent agent to call `status` if it needs the full result.

## Implementation Notes

- `src/tools/subagent.rs` is the single public entry point.
- `src/tools/subagent_async.rs` has been removed.
- Internal session construction is now funneled through `SubagentSessionConfig`.
- Skill and subagent delegation share a single call-tree budget via `MAX_DELEGATION_CALLS_PER_ROOT_REQUEST`.

## Verification Checklist

- [x] One unified `subagent` tool handles run, status, cancel, and list
- [x] Background subagents return `job_id` and can be polled or consumed later
- [x] Sync and background execution both inherit skill budget when launched from a skill tree
- [x] Ordinary subagents can call skills directly
- [x] Child skills can use `subagent` only when their contract explicitly allows it
- [x] Recursive `subagent -> subagent` spawning remains blocked by default
