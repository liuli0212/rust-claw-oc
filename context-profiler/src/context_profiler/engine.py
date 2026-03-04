import json
import math
from typing import Dict, Any, List
try:
    import tiktoken
except ImportError:
    pass # Handled in main

from .models import TurnStats, AuditReport, TokenStats, ResourceUsage, ContextMessage, MessageRole, OptimizationSuggestion
from .rules.base import BaseRule
from .rules.grep_bloat import GrepBloatRule
from .rules.causal_chain import CausalChainRule
from .rules.system_bloat import SystemPromptBloatRule

class ContextEngine:
    def __init__(self, data: Dict[str, Any]):
        self.data = data
        self.messages = data.get("messages", [])
        self.system_prompt = data.get("system_prompt", {})
        self.report = data.get("report", {})
        self.enc = tiktoken.get_encoding("cl100k_base")
        self.turns: List[TurnStats] = []
        self.resource_usage_map: Dict[str, ResourceUsage] = {}

    def run_audit(self) -> AuditReport:
        self._analyze_turns()
        completeness = self._check_completeness()
        redundancy = self._check_redundancy()
        suggestions = self._run_rules()
        
        return AuditReport(
            turns=self.turns,
            completeness_issues=completeness,
            redundant_turns=redundancy,
            resource_usage_summary=self.resource_usage_map,
            total_tokens=self.report.get("total_prompt_tokens", 0),
            max_tokens=self.report.get("max_history_tokens", 0),
            suggestions=suggestions
        )

    def _build_context_messages(self) -> List[ContextMessage]:
        ctx_msgs = []
        for msg in self.messages:
            role_str = msg.get("role", "user")
            # map string to MessageRole
            try:
                role = MessageRole(role_str.lower())
            except ValueError:
                role = MessageRole.USER
                
            parts = msg.get("parts", [])
            content = ""
            tool_name = None
            tool_args = None
            
            for part in parts:
                if "text" in part:
                    content += part["text"]
                if "functionCall" in part:
                    fc = part["functionCall"]
                    tool_name = fc.get("name")
                    tool_args = fc.get("args")
                    # Model tool calls role is typically Model but rules might want to know it's a tool intent
                if "functionResponse" in part:
                    fr = part["functionResponse"]
                    tool_name = fr.get("name")
                    content += str(fr.get("response", ""))
                    role = MessageRole.TOOL # Usually responses come with role=user in gemini, we override to TOOL for rules
            
            ctx_msgs.append(ContextMessage(
                role=role,
                content=content,
                tool_name=tool_name,
                tool_args=tool_args
            ))
        return ctx_msgs

    def _run_rules(self) -> List[OptimizationSuggestion]:
        ctx_messages = self._build_context_messages()
        rules: List[BaseRule] = [
            GrepBloatRule(),
            CausalChainRule(),
            SystemPromptBloatRule(),
        ]
        
        all_suggestions = []
        detailed_stats = self.report.get("detailed_stats", {})
        
        for rule in rules:
            try:
                suggestions = rule.evaluate(ctx_messages, detailed_stats=detailed_stats)
                all_suggestions.extend(suggestions)
            except Exception as e:
                import traceback
                traceback.print_exc()
                print(f"Error running rule {rule.rule_id}: {e}")
                
        return all_suggestions

    def _analyze_turns(self):
        for idx, msg in enumerate(self.messages):
            role = msg.get("role", "unknown")
            parts = msg.get("parts", [])
            
            content_preview = ""
            full_text_content = ""
            has_tool = False
            has_error = False
            turn_resources = []

            for part in parts:
                if text := part.get("text"):
                    full_text_content += text
                    if len(content_preview) < 50:
                        content_preview += text[:50].replace("\n", " ")
                
                if part.get("functionCall"):
                    has_tool = True
                    fname = part["functionCall"].get("name", "")
                    content_preview += f" [Tool: {fname}]"
                
                if resp := part.get("functionResponse"):
                    fname = resp.get("name", "")
                    content_preview += f" [Result: {fname}]"
                    res_val = resp.get("response", {})
                    res_str = str(res_val)
                    full_text_content += res_str
                    
                    if "error" in res_str.lower() or "failed" in res_str.lower():
                        has_error = True
                    
                    # Track Resource Usage
                    result_tokens = len(self.enc.encode(res_str))
                    resource_type, identifier = self._identify_resource(idx, fname)
                    
                    if identifier:
                        key = f"{resource_type}:{identifier}"
                        ru = ResourceUsage(resource_type, identifier, result_tokens, 1)
                        turn_resources.append(ru)
                        
                        if key in self.resource_usage_map:
                            self.resource_usage_map[key].tokens += result_tokens
                            self.resource_usage_map[key].count += 1
                        else:
                            self.resource_usage_map[key] = ru

            # Token Analysis
            tokens = len(self.enc.encode(json.dumps(msg)))
            unique_words = len(set(full_text_content.split()))
            density = unique_words / tokens if tokens > 0 else 0

            self.turns.append(TurnStats(
                index=idx,
                turn_id=f"msg_{idx}",
                role=role,
                content_preview=content_preview[:80] + "..." if len(content_preview) > 80 else content_preview,
                tokens=tokens,
                has_tool_call=has_tool,
                has_error=has_error,
                token_stats=TokenStats(tokens, unique_words, density),
                resource_usage=turn_resources
            ))

    def _identify_resource(self, current_idx: int, tool_name: str) -> (str, str):
        """Identify the resource type and identifier based on the tool call."""
        args = self._find_tool_args(current_idx, tool_name)
        if not args:
            return ("unknown", "")

        if tool_name == "read_file":
            return ("file", args.get("path") or args.get("file") or args.get("filename") or "")
        elif tool_name == "web_fetch":
            return ("url", args.get("url") or "")
        elif tool_name == "web_search":
            return ("search", args.get("query") or "")
        elif tool_name == "execute_bash":
            cmd = args.get("command") or args.get("cmd") or ""
            # Extract the first word as the command (e.g., "git", "ls")
            return ("cmd", cmd.split()[0] if cmd else "bash")
        elif tool_name == "browser":
            action = args.get("action") or "unknown"
            target = args.get("url") or args.get("selector") or ""
            return ("browser", f"{action} {target}".strip())
        
        return ("tool", tool_name)

    def _find_tool_args(self, current_idx: int, tool_name: str) -> Dict[str, Any]:
        # Look backwards for the corresponding tool call
        for i in range(current_idx - 1, -1, -1):
            msg = self.messages[i]
            parts = msg.get("parts", [])
            for p in parts:
                if fc := p.get("functionCall"):
                    if fc.get("name") == tool_name:
                        args = fc.get("args", {})
                        if isinstance(args, str):
                            try:
                                return json.loads(args)
                            except:
                                # Fallback: if it's not JSON, treat the whole string as the 'path' or 'query'
                                # This covers simple tool calls where args is just the payload
                                return {"path": args, "file": args, "filename": args, "query": args, "url": args, "command": args}
                        return args
        return {}

    def _check_completeness(self) -> List[str]:
        issues = []
        if not self.turns or self.turns[-1].role != "user":
            recent_user = any(t.role == "user" for t in self.turns[-3:])
            if not recent_user:
                issues.append("[Critical] Active Goal Missing: No user message found in the last 3 turns.")
        if not self.system_prompt:
            issues.append("[Critical] System Prompt Missing.")
        return issues

    def _check_redundancy(self) -> List[TurnStats]:
        redundant = []
        # 1. High Volume
        for t in self.turns:
            if t.tokens > 2000 and (t.role == "function" or "Result" in t.content_preview):
                t.is_redundant = True
                t.redundancy_reason = f"High Volume ({t.tokens})"
                redundant.append(t)
            # 2. Low Information Density
            elif t.tokens > 500 and t.token_stats.density_score < 0.1:
                t.is_redundant = True
                t.redundancy_reason = f"Low Info Density ({t.token_stats.density_score:.2f})"
                redundant.append(t)

        # 3. Oscillation
        for i in range(len(self.turns) - 2):
            t1 = self.turns[i]
            t3 = self.turns[i+2]
            if t1.has_error and t3.has_error and t1.role == "function" and t3.role == "function":
                 t3.is_redundant = True
                 t3.redundancy_reason = "Oscillation: Repeated Errors"
                 redundant.append(t3)
        return redundant
