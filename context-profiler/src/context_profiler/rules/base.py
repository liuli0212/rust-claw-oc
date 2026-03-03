from abc import ABC, abstractmethod
from typing import List
from ..models import ContextMessage, OptimizationSuggestion

class BaseRule(ABC):
    @property
    @abstractmethod
    def rule_id(self) -> str:
        pass

    @abstractmethod
    def evaluate(self, messages: List[ContextMessage]) -> List[OptimizationSuggestion]:
        pass
