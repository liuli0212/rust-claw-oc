pub fn execution_notice() -> String {
    [
        "Code Mode is enabled for this provider.",
        "For multi-step work, prefer the `exec` tool so you can orchestrate several nested tool calls inside one JavaScript cell.",
        "`exec` input must be raw JavaScript source stored in the `code` field. Do not wrap it in markdown fences.",
        "If an `exec` result says the cell is still running, call `wait` to resume that same cell.",
        "Use direct tools for trivial one-shot actions.",
        "",
        "Inside the `exec` JavaScript environment, you have access to the following globals:",
        "- `tools`: An object to call any available host tool asynchronously (e.g., `let res = await tools.read_file({ path: '...' });`). Let the host handle filesystem, shell, and network access through these tools.",
        "- `text(value)`: Append a string to the execution output buffer. This buffer is sent to the LLM only when `flush`, `wait`, or cell completion occurs.",
        "- `flush(value)`: Immediately send the accumulated output buffer back to the LLM along with an optional state value. The JavaScript code will smoothly continue running in the background. You must use the `wait` tool to sync up and receive its subsequent outputs.",
        "- `store(key, value)`: Save a JSON-serializable value in the session state to persist data across multiple separate `exec` tool calls (regular JS variables are destroyed when a cell finishes).",
        "- `load(key)`: Retrieve a value previously saved with `store`.",
        "- `notify(value)`: Append a message to the structured Notifications list in the execution result. Useful to separate important milestones from standard `text()` logs.",
        "- `exit(value)`: Terminate the current code cell early with a specific return value.",
        "- `setTimeout` / `clearTimeout`: Standard asynchronous timer functions.",
    ]
    .join("\n")
}
