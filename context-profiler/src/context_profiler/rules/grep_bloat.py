import re
from typing import List
from .base import BaseRule
from ..models import ContextMessage, OptimizationSuggestion, MessageRole

class GrepBloatRule(BaseRule):
    @property
    def rule_id(self) -> str:
        return "GREP_BLOAT"

    def evaluate(self, messages: List[ContextMessage]) -> List[OptimizationSuggestion]:
        suggestions = []
        for idx, msg in enumerate(messages):
            if msg.role != MessageRole.TOOL:
                continue

            # Detect grep in tool name or args
            is_grep = False
            command_str = ""
            
            if msg.tool_name and "grep" in msg.tool_name.lower():
                is_grep = True
            
            if msg.tool_args and "command" in msg.tool_args:
                command_str = str(msg.tool_args["command"])
                if re.search(r'\b(grep|rg|find)\b', command_str):
                    is_grep = True

            if is_grep:
                line_count = msg.content.count('\n')
                if line_count > 50:
                    suggestions.append(OptimizationSuggestion(
                        rule_id=self.rule_id,
                        severity="WARNING" if line_count < 200 else "CRITICAL",
                        target_message_index=idx,
                        description=f"Grep command produced {line_count} lines of output.",
                        actionable_advice=(
                            "If you only need file locations, use '-l'. "
                            "If you need to see results, use '| head -n 20' or 'grep -c' to count."
                        ),
                        estimated_savings_tokens=int(len(msg.content) * 0.8 / 4) # Estimate 80% reduction
                    ))
        return suggestions
