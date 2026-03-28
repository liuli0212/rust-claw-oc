use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::core::extensions::{
    ExecutionExtension, ExtensionDecision, FinishDecision, PromptDraft, ResumeDecision,
};
use crate::subagent_runtime::SubagentRuntime;
use crate::tools::protocol::ToolExecutionEnvelope;
use crate::tools::Tool;

pub struct SubagentNotificationExtension {
    session_id: String,
    runtime: SubagentRuntime,
    pending_notice: RwLock<Option<String>>,
}

impl SubagentNotificationExtension {
    pub fn new(session_id: impl Into<String>, runtime: SubagentRuntime) -> Self {
        Self {
            session_id: session_id.into(),
            runtime,
            pending_notice: RwLock::new(None),
        }
    }
}

#[async_trait]
impl ExecutionExtension for SubagentNotificationExtension {
    async fn before_turn_start(&self, _input: &str) -> ExtensionDecision {
        let notifications = self.runtime.take_notifications(&self.session_id).await;
        if notifications.is_empty() {
            return ExtensionDecision::Continue;
        }

        let mut lines = Vec::with_capacity(notifications.len() + 1);
        lines.push(
            "Background subagent updates are available from earlier work in this session:"
                .to_string(),
        );
        for notification in notifications {
            lines.push(format!(
                "- job `{}` ({}) is `{}`: {}",
                notification.job_id,
                notification.sub_session_id,
                notification.status,
                notification.summary
            ));
        }

        *self.pending_notice.write().await = Some(lines.join("\n"));
        ExtensionDecision::Continue
    }

    async fn before_prompt_build(&self, mut draft: PromptDraft) -> PromptDraft {
        draft.execution_notices = self.pending_notice.write().await.take();
        draft
    }

    async fn before_tool_resolution(
        &self,
        tools: Vec<std::sync::Arc<dyn Tool>>,
    ) -> Vec<std::sync::Arc<dyn Tool>> {
        tools
    }

    async fn after_tool_result(&self, _result: &ToolExecutionEnvelope) {}

    async fn on_user_resume(&self, _input: &str) -> ResumeDecision {
        ResumeDecision::PassThrough
    }

    async fn before_finish(&self) -> FinishDecision {
        FinishDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::llm_client::{LlmClient, LlmError, StreamEvent};
    use crate::subagent_runtime::SubagentNotification;
    use crate::tools::Tool;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    struct NoopLlm;

    #[async_trait]
    impl LlmClient for NoopLlm {
        fn model_name(&self) -> &str {
            "noop"
        }

        fn provider_name(&self) -> &str {
            "test"
        }

        async fn stream(
            &self,
            _messages: Vec<crate::context::Message>,
            _system_instruction: Option<crate::context::Message>,
            _tools: Vec<Arc<dyn Tool>>,
        ) -> Result<mpsc::Receiver<StreamEvent>, LlmError> {
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }
    }

    #[tokio::test]
    async fn test_notification_extension_injects_runtime_notice_once() {
        let runtime = SubagentRuntime::new(Arc::new(NoopLlm), Vec::new(), 1);
        runtime
            .record_notification_for_test(
                "parent-session",
                SubagentNotification {
                    job_id: "job-1".to_string(),
                    sub_session_id: "sub-1".to_string(),
                    status: "finished".to_string(),
                    summary: "Completed parser analysis".to_string(),
                },
            )
            .await;

        let extension = SubagentNotificationExtension::new("parent-session", runtime);
        let decision = extension.before_turn_start("continue").await;
        assert!(matches!(decision, ExtensionDecision::Continue));

        let draft = extension.before_prompt_build(PromptDraft::default()).await;
        let notices = draft.execution_notices.unwrap();
        assert!(notices.contains("job `job-1`"));
        assert!(notices.contains("Completed parser analysis"));

        let next_draft = extension.before_prompt_build(PromptDraft::default()).await;
        assert!(next_draft.execution_notices.is_none());
    }
}
