#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedToolCall {
    pub tool_name: String,
    pub args_json: String,
    pub result_json: String,
}
