pub fn execution_notice() -> String {
    [
        "Code Mode is enabled for this provider.",
        "For multi-step work, prefer the `exec` tool so you can orchestrate several nested tool calls inside one JavaScript cell.",
        "`exec` input must be raw JavaScript source stored in the `code` field. Do not wrap it in markdown fences.",
        "If an `exec` result says the cell is still running, call `wait` to resume that same cell.",
        "Use direct tools for trivial one-shot actions.",
        "Only call tools exposed through `ALL_TOOLS`, and let the host handle filesystem, shell, and network access through those tools.",
    ]
    .join("\n")
}
