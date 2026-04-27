use async_trait::async_trait;
use std::sync::Arc;

use crate::tools::protocol::ToolExecutionEnvelope;
use crate::tools::Tool;
use crate::tools::ToolContext;

/// Lifecycle hooks that allow extensions (such as SkillRuntime) to intercept
/// and modify AgentLoop behaviour without the main loop knowing about
/// skill-specific internals.
#[async_trait]
pub trait ExecutionExtension: Send + Sync {
    /// Called at the very start of each turn, before prompt assembly.
    /// An extension may intercept or directly handle the turn entirely.
    async fn before_turn_start(&self, input: &str) -> ExtensionDecision;

    /// Called before the system prompt is finalised.
    /// Extensions may inject skill-level prompt sections into the draft.
    async fn before_prompt_build(&self, draft: PromptDraft) -> PromptDraft;

    /// Called before the tool list is sent to the model.
    /// Extensions may filter, reorder or augment the visible tool set.
    async fn before_tool_resolution(&self, tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>>;

    async fn enrich_tool_context(&self, ctx: ToolContext) -> ToolContext {
        ctx
    }

    /// Called after every successful tool execution.
    /// Extensions may update internal state based on the result.
    async fn after_tool_result(&self, result: &ToolExecutionEnvelope);

    /// Called before a final text response is committed.
    /// Extensions may deny completion if artifact contracts are unmet.
    async fn before_finish(&self) -> FinishDecision;

    /// Called after the final text response has been accepted by all extensions and the
    /// run is about to commit its finished state.
    async fn on_finish_committed(&self, _summary: &str) {}
}

// ---------------------------------------------------------------------------
// Decision / Data types returned by extension hooks
// ---------------------------------------------------------------------------

/// Carries optional skill-level prompt sections that should be injected
/// into the system prompt during assembly.
#[derive(Debug, Default, Clone)]
pub struct PromptDraft {
    /// A compact, auto-generated contract describing the active skill's
    /// name, version, allowed tools, hard gates and current phase.
    pub skill_contract: Option<String>,
    /// The skill's full instruction body (may be truncated by the assembler).
    pub skill_instructions: Option<String>,
    /// A human-readable summary of the skill's current execution state.
    pub skill_state_summary: Option<String>,
    /// General execution notices surfaced by runtime-level extensions.
    pub execution_notices: Option<String>,
}

/// Returned by `before_turn_start`.
#[derive(Debug)]
pub enum ExtensionDecision {
    /// Let the turn proceed normally.
    Continue,
    /// The extension wants to intercept this turn.
    Intercept { prompt_overlay: Option<String> },
    /// The extension handled this input directly and the loop should yield.
    Halt { message: String },
}

/// Returned by `before_finish`.
#[derive(Debug)]
pub enum FinishDecision {
    /// Completion is allowed.
    Allow,
    /// Completion is denied; the agent should continue working.
    Deny { reason: String },
}
