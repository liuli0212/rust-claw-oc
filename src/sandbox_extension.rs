//! Extension that injects sandbox enforcement into the agent loop.
//!
//! Implements `ExecutionExtension` to inject `SandboxEnforcer` into every
//! `ToolContext` via the `enrich_tool_context` hook and inject sandbox
//! constraints into the system prompt via `before_prompt_build`.

use async_trait::async_trait;
use std::sync::Arc;

use crate::core::extensions::{ExecutionExtension, ExtensionDecision, FinishDecision, PromptDraft};
use crate::tools::protocol::ToolExecutionEnvelope;
use crate::tools::sandbox::SandboxEnforcer;
use crate::tools::{Tool, ToolContext};

/// An `ExecutionExtension` that wires sandbox enforcement into the agent loop.
///
/// - Injects `SandboxEnforcer` into every `ToolContext` (so tools can use it).
/// - Injects sandbox constraint summary into the system prompt (so the LLM
///   knows its boundaries upfront).
pub struct SandboxExtension {
    enforcer: Arc<SandboxEnforcer>,
}

impl SandboxExtension {
    pub fn new(enforcer: Arc<SandboxEnforcer>) -> Self {
        Self { enforcer }
    }
}

#[async_trait]
impl ExecutionExtension for SandboxExtension {
    async fn before_turn_start(&self, _input: &str) -> ExtensionDecision {
        ExtensionDecision::Continue
    }

    async fn before_prompt_build(&self, mut draft: PromptDraft) -> PromptDraft {
        let summary = self.enforcer.prompt_summary();
        if !summary.is_empty() {
            let sandbox_notice = format!(
                "<environment_constraints>\n{}\n</environment_constraints>",
                summary
            );
            // Append to execution_notices so it's included in the system prompt
            if let Some(existing) = &draft.execution_notices {
                draft.execution_notices = Some(format!("{}\n\n{}", existing, sandbox_notice));
            } else {
                draft.execution_notices = Some(sandbox_notice);
            }
        }
        draft
    }

    async fn before_tool_resolution(&self, tools: Vec<Arc<dyn Tool>>) -> Vec<Arc<dyn Tool>> {
        tools
    }

    async fn enrich_tool_context(&self, mut ctx: ToolContext) -> ToolContext {
        ctx.sandbox = Some(self.enforcer.clone());
        ctx
    }

    async fn after_tool_result(&self, _result: &ToolExecutionEnvelope) {}

    async fn before_finish(&self) -> FinishDecision {
        FinishDecision::Allow
    }
}
