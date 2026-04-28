#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodeModeFormat {
    #[default]
    FunctionTool,
    TextCommand,
}

impl CodeModeFormat {
    pub fn accepts_text_command(self) -> bool {
        matches!(self, Self::TextCommand)
    }
}

impl std::str::FromStr for CodeModeFormat {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "function" | "function-tool" | "json" => Ok(Self::FunctionTool),
            "text" | "text-command" | "command" => Ok(Self::TextCommand),
            other => Err(format!(
                "Unknown code mode format `{other}`. Expected function or text."
            )),
        }
    }
}

pub fn execution_notice(available_tools: &[String], format: CodeModeFormat) -> String {
    let tools_list = available_tools.join(", ");
    let tools_notice = format!("- `tools`: An object to call any available host tool asynchronously (e.g., `let res = await tools.read_file({{ path: '...' }});`). Let the host handle filesystem, shell, and network access through these tools. Available tools: [{}]", tools_list);
    let wait_instruction = match format {
        CodeModeFormat::FunctionTool => {
            "If an `exec` result says the cell is still running, call `wait` to poll or sync that same cell. `wait` does not resume timers; it only syncs current state. Without `wait_timeout_ms`, `wait` blocks until the next update, completion, cancellation, or the cell runtime deadline."
        }
        CodeModeFormat::TextCommand => {
            "In text command mode, the visible assistant text is the JavaScript source, but execution starts only when the same model turn also makes a real `exec` tool call with `code` set exactly to `__RUSTY_CLAW_TEXT_COMMAND__`. If the `exec` result says the cell is still running, call the `wait` tool to poll or sync that same cell. `wait` does not resume timers; it only syncs current state. Without `wait_timeout_ms`, `wait` blocks until the next update, completion, cancellation, or the cell runtime deadline."
        }
    };

    let mut lines = vec![
        "Code Mode is enabled for this provider.",
        "For multi-step work, prefer code mode so you can orchestrate several nested tool calls inside one JavaScript cell.",
    ];

    match format {
        CodeModeFormat::FunctionTool => {
            lines.extend([
                "For multi-step work in function-tool mode, prefer the `exec` tool.",
                "Use function-tool form: call `exec` with raw JavaScript source stored in the `code` field.",
                "`exec` input must be raw JavaScript source stored in the `code` field. Do not wrap it in markdown fences.",
                "`exec` also accepts optional `auto_flush_ms` (milliseconds) to publish accumulated progress while the cell keeps running in the background.",
                "`exec` also accepts optional `cell_timeout_ms` (milliseconds) for the cell's expected maximum runtime. It defaults to 120000ms and the system hard-caps it at 300000ms; when the deadline is reached, the system cancels the cell and wakes any `wait` call.",
            ]);
        }
        CodeModeFormat::TextCommand => {
            lines.extend([
                "Code Mode Text Format:",
                "When you choose Code Mode, your visible assistant message MUST be raw JavaScript and the same model turn MUST also include a real `exec` tool call.",
                "In that same model turn, call the real `exec` tool with this exact sentinel argument:",
                "  {\"code\":\"__RUSTY_CLAW_TEXT_COMMAND__\"}",
                "Do not put the JavaScript source in the `exec` arguments. The host reads the source from the visible assistant text.",
                "Put optional execution settings such as `auto_flush_ms` and `cell_timeout_ms` in the sentinel `exec` tool arguments.",
                "Valid example:",
                "  const res = await tools.read_file({ path: \"src/main.rs\" });",
                "  text(res);",
                "  [and in the same model response, call exec({\"code\":\"__RUSTY_CLAW_TEXT_COMMAND__\",\"auto_flush_ms\":1000})]",
                "Rules:",
                "The real `exec` tool call with the sentinel authorizes execution.",
                "Never call `exec` with the JavaScript source in text mode.",
                "Do not write explanation text before or after the JavaScript.",
                "Do not wrap the JavaScript in markdown fences.",
                "Use direct tools or normal text for trivial one-shot work.",
                "Invalid examples:",
                "  Here is the code:",
                "  ```js",
                "  ...",
                "  ```",
                "`cell_timeout_ms` defaults to 120000ms and the system hard-caps it at 300000ms; when the deadline is reached, the system cancels the cell and wakes any `wait` call.",
            ]);
        }
    }

    lines.extend([
        "When a cell uses `setTimeout`, polling, retries, long tool chains, or other background work that may outlive the first response, usually set `auto_flush_ms` so progress can surface without pausing JavaScript.",
        "Prefer `flush(value)` for meaningful milestones you want to publish immediately. Prefer `auto_flush_ms` for heartbeat-style progress during long-running work. You may use both together.",
        "Timer boundaries are internal runtime details, not user-visible progress events. Do not rely on timers alone to report progress.",
        wait_instruction,
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
    ]);

    lines.join("\n")
}
