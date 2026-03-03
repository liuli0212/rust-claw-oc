import pytest
from context_profiler.models import ContextMessage, MessageRole
from context_profiler.rules.grep_bloat import GrepBloatRule

def test_grep_bloat_detection():
    rule = GrepBloatRule()
    
    # Simulate a bad grep output (many lines)
    bad_output = "\n".join([f"match line {i}" for i in range(100)])
    
    messages = [
        ContextMessage(
            role=MessageRole.TOOL,
            content=bad_output,
            tool_name="exec",
            tool_args={"command": "grep -r 'todo' ."}
        )
    ]
    
    suggestions = rule.evaluate(messages)
    assert len(suggestions) == 1
    assert suggestions[0].rule_id == "GREP_BLOAT"
    assert suggestions[0].severity == "WARNING" # 100 lines < 200

def test_grep_bloat_critical():
    rule = GrepBloatRule()
    bad_output = "\n".join([f"match line {i}" for i in range(500)])
    
    messages = [
        ContextMessage(
            role=MessageRole.TOOL,
            content=bad_output,
            tool_name="exec",
            tool_args={"command": "grep 'test'"}
        )
    ]
    
    suggestions = rule.evaluate(messages)
    assert len(suggestions) == 1
    assert suggestions[0].severity == "CRITICAL"

def test_grep_safe_output():
    rule = GrepBloatRule()
    good_output = "just one line"
    
    messages = [
        ContextMessage(
            role=MessageRole.TOOL,
            content=good_output,
            tool_name="exec",
            tool_args={"command": "grep 'test'"}
        )
    ]
    
    suggestions = rule.evaluate(messages)
    assert len(suggestions) == 0
