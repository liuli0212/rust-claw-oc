pub(crate) fn is_code_mode_nested_tool(tool_name: &str) -> bool {
    !matches!(
        tool_name,
        "exec"
            | "wait"
            | "finish_task"
            | "ask_user_question"
            | "subagent"
            | "task_plan"
            | "manage_schedule"
            | "send_telegram_message"
    )
}
