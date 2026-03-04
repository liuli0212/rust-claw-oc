import re
from typing import List
from .base import BaseRule
from ..models import ContextMessage, OptimizationSuggestion, MessageRole

class CausalChainRule(BaseRule):
    """
    Detects if the context drops causal links, e.g. patching code without errors/context.
    """
    @property
    def rule_id(self) -> str:
        return "MISSING_CAUSAL_ERROR"

    def evaluate(self, messages: List[ContextMessage], **kwargs) -> List[OptimizationSuggestion]:
        suggestions = []
        has_error_in_history = False
        
        # Keywords to detect if an error was observed
        error_keywords = re.compile(r'\b(error|failed|panic|exception|traceback)\b', re.IGNORECASE)

        for idx, msg in enumerate(messages):
            if msg.content and error_keywords.search(msg.content):
                has_error_in_history = True

            # If we see a patch_file tool, it usually indicates we are fixing a bug or adding a feature.
            # We flag this only if there is a discussion about fixing an error, but NO error log was found.
            if msg.role == MessageRole.TOOL and msg.tool_name in ["patch_file", "replace_symbol_body"]:
                # To reduce false positives, we check if the goal or user asks to "fix".
                if not has_error_in_history:
                    # Look at recent user or model content to see if they mentioned fixing something
                    recent_mentions_fix = False
                    start_idx = max(0, idx - 5)
                    for i in range(start_idx, idx):
                        if messages[i].content and re.search(r'\b(fix|resolve|bug|issue)\b', messages[i].content, re.IGNORECASE):
                            recent_mentions_fix = True
                            
                    if recent_mentions_fix:
                        suggestions.append(OptimizationSuggestion(
                            rule_id=self.rule_id,
                            severity="CRITICAL",
                            target_message_index=idx,
                            description=("Agent is trying to fix a bug, but no error logs (error, failed, panic) "
                                         "exist in the current context window. Historical context may have been dropped."),
                            actionable_advice="User should re-run the failing command or re-paste the error to refresh the context."
                        ))
                        # We append one suggestion at a time and reset or break to avoid spamming
                        has_error_in_history = True # Treat as warned
                
        return suggestions
