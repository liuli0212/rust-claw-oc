from dataclasses import dataclass, field
from typing import List, Dict, Optional, Any
from enum import Enum

class MessageRole(str, Enum):
    USER = "user"
    MODEL = "model"
    TOOL = "tool"
    SYSTEM = "system"

@dataclass
class ContextMessage:
    role: MessageRole
    content: str
    tool_name: Optional[str] = None
    tool_args: Optional[Dict[str, Any]] = None

@dataclass
class OptimizationSuggestion:
    rule_id: str
    severity: str  # e.g., "INFO", "WARNING", "CRITICAL"
    target_message_index: int
    description: str
    actionable_advice: str
    estimated_savings_tokens: int = 0

@dataclass
class TokenStats:
    total: int
    unique_words: int
    density_score: float = 0.0

@dataclass
class ResourceUsage:
    resource_type: str  # "file", "url", "command", "browser"
    identifier: str     # filename, url, or command string
    tokens: int
    count: int

@dataclass
class TurnStats:
    index: int
    turn_id: str
    role: str
    content_preview: str
    tokens: int
    has_tool_call: bool
    has_error: bool
    is_redundant: bool = False
    redundancy_reason: str = ""
    token_stats: TokenStats = field(default_factory=lambda: TokenStats(0, 0))
    resource_usage: List[ResourceUsage] = field(default_factory=list)

@dataclass
class AuditReport:
    turns: List[TurnStats]
    completeness_issues: List[str]
    redundant_turns: List[TurnStats]
    resource_usage_summary: Dict[str, ResourceUsage]
    total_tokens: int
    max_tokens: int
    suggestions: List[OptimizationSuggestion] = field(default_factory=list)
