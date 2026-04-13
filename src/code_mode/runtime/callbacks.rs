#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeToolCall {
    pub tool_name: String,
    pub args_json: String,
}
