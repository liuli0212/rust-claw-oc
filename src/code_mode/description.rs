pub fn execution_notice(available_tools: &[String]) -> String {
    let tools_list = available_tools.join(", ");
    let tools_notice = format!("- `tools`: An object to call any available host tool asynchronously (e.g., `let res = await tools.read_file({{ path: '...' }});`). Let the host handle filesystem, shell, and network access through these tools. Available tools: [{}]", tools_list);

    [
        "Code Mode is enabled for this provider.",
        "For multi-step work, prefer the `exec` tool so you can orchestrate several nested tool calls inside one JavaScript cell.",
        "`exec` input must be raw JavaScript source stored in the `code` field. Do not wrap it in markdown fences.",
        "`exec` also accepts optional `auto_flush_ms` (milliseconds) to publish accumulated progress while the cell keeps running in the background.",
        "When a cell uses `setTimeout`, polling, retries, long tool chains, or other background work that may outlive the first response, usually set `auto_flush_ms` so progress can surface without pausing JavaScript.",
        "Prefer `flush(value)` for meaningful milestones you want to publish immediately. Prefer `auto_flush_ms` for heartbeat-style progress during long-running work. You may use both together.",
        "Timer boundaries are internal runtime details, not user-visible progress events. Do not rely on timers alone to report progress.",
        "If an `exec` result says the cell is still running, call `wait` to poll or sync that same cell. `wait` does not resume timers; it only syncs current state.",
        "Use direct tools for trivial one-shot actions.",
        "",
        "Inside the `exec` JavaScript environment, you have access to the following globals:",
        &tools_notice,
        "- `text(value)`: Append a string to the execution output buffer. This buffer is sent to the LLM only when `flush`, `auto_flush_ms`, `wait`, or cell completion publishes it.",
        "- `flush(value)`: Immediately publish the accumulated output buffer back to the LLM along with an optional state value. The JavaScript code keeps running in the background. Use `wait` to sync later outputs when needed.",
        "- `store(key, value)`: Save a JSON-serializable value in the session state to persist data across multiple separate `exec` tool calls (regular JS variables are destroyed when a cell finishes).",
        "- `load(key)`: Retrieve a value previously saved with `store`.",
        "- `notify(value)`: Append a message to the structured Notifications list in the execution result. Useful to separate important milestones from standard `text()` logs.",
        "- `exit(value)`: Terminate the current code cell early with a specific return value.",
        "- `setTimeout` / `clearTimeout`: Standard asynchronous timer functions.",
    ]
    .join("\n")
}
