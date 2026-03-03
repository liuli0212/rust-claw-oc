import tiktoken
from typing import List, Type
from .models import ContextMessage, AnalysisReport, OptimizationSuggestion
from .rules.base import BaseRule
from .rules.grep_bloat import GrepBloatRule

class ContextAnalyzer:
    def __init__(self, model_name: str = "gemini-2.0-flash"):
        # For non-OpenAI models, we might need a different tokenizer library or a mapping.
        # tiktoken is specific to OpenAI models.
        # For simplicity in this prototype, we'll keep tiktoken but acknowledge the approximation,
        # or switch to a generic length estimation if precise tokenization isn't available.
        try:
            self.encoder = tiktoken.encoding_for_model(model_name)
        except KeyError:
            # Fallback for non-OpenAI models if not found in tiktoken
            # Ideally, we would integrate google-generativeai or a generic tokenizer here.
            # Using cl100k_base (gpt-4) as a reasonable proxy for estimation.
            self.encoder = tiktoken.get_encoding("cl100k_base") 
        
        self.rules: List[BaseRule] = [
            GrepBloatRule(),
            # Add more rules here
        ]

    def analyze(self, messages: List[ContextMessage]) -> AnalysisReport:
        all_suggestions = []
        total_tokens = 0
        
        # Calculate tokens and run rules
        for msg in messages:
            if not msg.token_count:
                msg.token_count = len(self.encoder.encode(msg.content))
            total_tokens += msg.token_count
            
        for rule in self.rules:
            suggestions = rule.evaluate(messages)
            all_suggestions.extend(suggestions)

        # Simple health score algorithm
        health_score = 100
        for sug in all_suggestions:
            if sug.severity == "CRITICAL":
                health_score -= 20
            elif sug.severity == "WARNING":
                health_score -= 5
        
        health_score = max(0, health_score)

        return AnalysisReport(
            total_tokens=total_tokens,
            total_messages=len(messages),
            suggestions=all_suggestions,
            health_score=health_score
        )
