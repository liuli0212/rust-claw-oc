use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceActor {
    User,
    MainAgent,
    Subagent,
    Skill,
    Llm,
    Tool,
    Context,
    Scheduler,
    System,
}

impl TraceActor {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::MainAgent => "main_agent",
            Self::Subagent => "subagent",
            Self::Skill => "skill",
            Self::Llm => "llm",
            Self::Tool => "tool",
            Self::Context => "context",
            Self::Scheduler => "scheduler",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceKind {
    SpanStart,
    SpanEnd,
    Event,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceStatus {
    Ok,
    Error,
    Cancelled,
    TimedOut,
    Yielded,
    Retrying,
    Skipped,
    Running,
}

impl TraceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
            Self::Yielded => "yielded",
            Self::Retrying => "retrying",
            Self::Skipped => "skipped",
            Self::Running => "running",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceLevel {
    Normal,
    Debug,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceRecord {
    pub schema_version: u32,
    pub record_id: String,
    pub trace_id: String,
    pub run_id: String,
    pub span_id: Option<String>,
    pub parent_span_id: Option<String>,
    pub session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub iteration: Option<u32>,
    pub actor: TraceActor,
    pub kind: TraceKind,
    pub name: String,
    pub status: TraceStatus,
    pub ts_unix_ms: u64,
    pub duration_ms: Option<u64>,
    pub level: TraceLevel,
    pub summary: Option<String>,
    pub attrs: Value,
}

#[derive(Debug, Clone)]
pub struct TraceContext {
    pub trace_id: String,
    pub run_id: String,
    pub session_id: String,
    pub root_session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub iteration: Option<u32>,
    pub parent_span_id: Option<String>,
}

impl TraceContext {
    pub fn with_parent_span_id(&self, parent_span_id: Option<String>) -> Self {
        let mut ctx = self.clone();
        ctx.parent_span_id = parent_span_id;
        ctx
    }

    pub fn with_turn_id(&self, turn_id: Option<String>) -> Self {
        let mut ctx = self.clone();
        ctx.turn_id = turn_id;
        ctx
    }

    pub fn with_iteration(&self, iteration: Option<u32>) -> Self {
        let mut ctx = self.clone();
        ctx.iteration = iteration;
        ctx
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub trace_id: String,
    pub session_id: String,
    pub root_session_id: String,
    pub task_id: Option<String>,
    pub root_goal: Option<String>,
    pub status: String,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub total_events: u64,
    pub total_spans: u64,
    pub total_tool_calls: u64,
    pub total_llm_calls: u64,
    pub total_subagents: u64,
    pub peak_prompt_tokens: Option<u64>,
    pub peak_history_tokens: Option<u64>,
    pub last_error_summary: Option<String>,
    pub tool_names: Vec<String>,
    pub artifact_paths: Vec<String>,
    pub updated_at_unix_ms: u64,
}

impl RunSummary {
    pub fn new(run_id: &str, trace_id: &str, session_id: &str, root_session_id: &str) -> Self {
        Self {
            run_id: run_id.to_string(),
            trace_id: trace_id.to_string(),
            session_id: session_id.to_string(),
            root_session_id: root_session_id.to_string(),
            task_id: None,
            root_goal: None,
            status: "running".to_string(),
            started_at_unix_ms: 0,
            finished_at_unix_ms: None,
            duration_ms: None,
            provider: None,
            model: None,
            total_events: 0,
            total_spans: 0,
            total_tool_calls: 0,
            total_llm_calls: 0,
            total_subagents: 0,
            peak_prompt_tokens: None,
            peak_history_tokens: None,
            last_error_summary: None,
            tool_names: Vec::new(),
            artifact_paths: Vec::new(),
            updated_at_unix_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSeed {
    pub trace_id: String,
    pub run_id: String,
    pub root_session_id: String,
    pub task_id: Option<String>,
    pub parent_span_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceArtifacts {
    pub files: Vec<String>,
    pub evidence: Vec<Value>,
    pub subagent_sessions: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceTreeNode {
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub actor: TraceActor,
    pub name: String,
    pub status: String,
    pub started_at_unix_ms: u64,
    pub duration_ms: Option<u64>,
    pub summary: Option<String>,
    pub attrs: Value,
    pub children: Vec<TraceTreeNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSpanSummary {
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub actor: TraceActor,
    pub name: String,
    pub status: String,
    pub started_at_unix_ms: u64,
    pub duration_ms: u64,
    pub turn_id: Option<String>,
    pub iteration: Option<u32>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunContextOverview {
    pub total_events: u64,
    pub plan_updates: u64,
    pub task_state_updates: u64,
    pub compactions: u64,
    pub truncations: u64,
    pub yields: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceHotspot {
    pub actor: TraceActor,
    pub name: String,
    pub count: u64,
    pub total_duration_ms: u64,
    pub max_duration_ms: u64,
    pub error_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOverview {
    pub summary: RunSummary,
    pub turn_count: u64,
    pub iteration_count: u64,
    pub context: RunContextOverview,
    pub slowest_spans: Vec<TraceSpanSummary>,
    pub hotspots: Vec<TraceHotspot>,
}
