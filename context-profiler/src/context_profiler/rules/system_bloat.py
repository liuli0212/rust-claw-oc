from typing import List
from .base import BaseRule
from ..models import ContextMessage, OptimizationSuggestion

class SystemPromptBloatRule(BaseRule):
    """
    Analyzes detailed_stats from the payload and flags overgrown sections of the System Prompt.
    """
    @property
    def rule_id(self) -> str:
        return "SYSTEM_PROMPT_BLOAT"

    def evaluate(self, messages: List[ContextMessage], **kwargs) -> List[OptimizationSuggestion]:
        suggestions = []
        detailed_stats = kwargs.get("detailed_stats", {})
        
        if not detailed_stats:
            return suggestions

        total_sys = sum([
            detailed_stats.get("system_static", 0),
            detailed_stats.get("system_runtime", 0),
            detailed_stats.get("system_custom", 0),
            detailed_stats.get("system_project", 0),
            detailed_stats.get("system_task_plan", 0),
            detailed_stats.get("memory", 0)
        ])
        
        # Check overall bloat
        if total_sys > 20000:
            suggestions.append(OptimizationSuggestion(
                rule_id=self.rule_id,
                severity="WARNING" if total_sys < 40000 else "CRITICAL",
                target_message_index=-1, # Target System Prompt (not a user msg)
                description=f"System Prompt is extremely large ({total_sys} tokens).",
                actionable_advice=("Review project context files like AGENTS.md, README.md, and MEMORY.md. "
                                   "Consider deleting outdated knowledge to leave room for RAG and dialogue history.")
            ))

        # Check individual sections bloat
        sys_project = detailed_stats.get("system_project", 0)
        if sys_project > 10000:
            suggestions.append(OptimizationSuggestion(
                rule_id=self.rule_id,
                severity="WARNING" if sys_project < 15000 else "CRITICAL",
                target_message_index=-1,
                description=f"Project Context injected into System Prompt is bulky ({sys_project} tokens).",
                actionable_advice=("This includes AGENTS.md or MEMORY.md. Please truncate or summarize them directly on disk "
                                   "to improve LLM focus.")
            ))

        sys_custom = detailed_stats.get("system_custom", 0)
        if sys_custom > 3000:
            suggestions.append(OptimizationSuggestion(
                rule_id=self.rule_id,
                severity="WARNING",
                target_message_index=-1,
                description=f"Custom Instructions (.claw_prompt.md) size is quite large ({sys_custom} tokens).",
                actionable_advice="Ensure that custom instructions don't contain stale task descriptions."
            ))

        return suggestions
