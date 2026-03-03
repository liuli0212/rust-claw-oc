import argparse
import json
import sys
import math
from typing import Dict, List, Any, Optional
from dataclasses import dataclass
from datetime import datetime

try:
    import tiktoken
    from rich.console import Console
    from rich.table import Table
    from rich.panel import Panel
    from rich.tree import Tree
    from rich.markup import escape
    from rich.layout import Layout
    from rich.text import Text
except ImportError:
    print("Error: Missing dependencies. Please run 'pip install rich tiktoken'")
    sys.exit(1)

console = Console()
enc = tiktoken.get_encoding("cl100k_base")

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

class ContextAuditor:
    def __init__(self, data: Dict[str, Any]):
        self.data = data
        self.messages = data.get("messages", [])
        self.system_prompt = data.get("system_prompt", {})
        self.report = data.get("report", {})
        self.turns: List[TurnStats] = []
        self._analyze_turns()

    def _analyze_turns(self):
        """Break down messages into logical turns and analyze token usage."""
        for idx, msg in enumerate(self.messages):
            role = msg.get("role", "unknown")
            parts = msg.get("parts", [])
            
            content_preview = ""
            has_tool = False
            has_error = False
            
            full_text = ""
            for part in parts:
                if text := part.get("text"):
                    full_text += text
                    if len(content_preview) < 50:
                        content_preview += text[:50].replace("\n", " ")
                
                if part.get("functionCall"):
                    has_tool = True
                    fname = part["functionCall"].get("name", "")
                    content_preview += f" [Tool: {fname}]"
                
                if resp := part.get("functionResponse"):
                    fname = resp.get("name", "")
                    content_preview += f" [Result: {fname}]"
                    # Simple heuristic for error detection in result
                    res_str = str(resp.get("response", ""))
                    if "error" in res_str.lower() or "failed" in res_str.lower():
                        has_error = True

            # Estimate tokens using tiktoken (approximation of parts)
            # We treat the whole message structure as text for rough counting
            dump_str = json.dumps(msg)
            tokens = len(enc.encode(dump_str))

            self.turns.append(TurnStats(
                index=idx,
                turn_id=f"msg_{idx}",
                role=role,
                content_preview=content_preview[:80] + "..." if len(content_preview) > 80 else content_preview,
                tokens=tokens,
                has_tool_call=has_tool,
                has_error=has_error
            ))

    def check_completeness(self) -> List[str]:
        issues = []
        
        # 1. Latest User Goal
        if not self.turns or self.turns[-1].role != "user":
            # If last message isn't user, maybe we are mid-generation or tool execution.
            # Check if there's *any* user message in the last 3 turns
            recent_user = any(t.role == "user" for t in self.turns[-3:])
            if not recent_user:
                issues.append("[Critical] Active Goal Missing: No user message found in the last 3 turns.")
        
        # 2. System Prompt Integrity
        if not self.system_prompt:
            issues.append("[Critical] System Prompt Missing: The 'system_prompt' field is empty.")
        
        # 3. RAG/Memory Coverage
        # Check if report says we have memory, and if it's actually in system prompt
        reported_mem_count = self.report.get("retrieved_memory_snippets", 0)
        sys_text = ""
        if isinstance(self.system_prompt, dict):
            # It might be a Message object
            parts = self.system_prompt.get("parts", [])
            for p in parts:
                if t := p.get("text"):
                    sys_text += t
        
        if reported_mem_count > 0 and "Retrieved Memory" not in sys_text:
             issues.append(f"[Warning] Memory Disconnect: Report claims {reported_mem_count} snippets, but 'Retrieved Memory' section not found in System Prompt.")

        return issues

    def check_redundancy(self) -> List[TurnStats]:
        redundant_turns = []
        
        # 1. High-Volume Tool Outputs (Naive check: > 2000 tokens and is function)
        for t in self.turns:
            if t.role == "function" or "Result" in t.content_preview:
                if t.tokens > 2000:
                    t.is_redundant = True
                    t.redundancy_reason = f"High Token Volume ({t.tokens}) - Consider Offloading"
                    redundant_turns.append(t)
        
        # 2. Repeated Errors (Oscillation)
        # Check if consecutive tool outputs have errors
        for i in range(len(self.turns) - 1):
            t1 = self.turns[i]
            t2 = self.turns[i+1]
            if t1.has_error and t2.has_error and t1.role == t2.role:
                 t2.is_redundant = True
                 t2.redundancy_reason = "Oscillation: Repeated Errors"
                 redundant_turns.append(t2)

        return redundant_turns

    def print_report(self):
        layout = Layout()
        layout.split_column(
            Layout(name="header", size=3),
            Layout(name="main", ratio=1),
            Layout(name="footer", size=3)
        )
        
        # Header
        total_tokens = self.report.get("total_prompt_tokens", "N/A")
        max_tokens = self.report.get("max_history_tokens", "N/A")
        layout["header"].update(Panel(f"[bold cyan]Claw-Context Audit[/bold cyan] | Total Tokens: [bold yellow]{total_tokens}/{max_tokens}[/bold yellow]", style="white"))

        # Main Content - Split into Turns and Issues
        
        # 1. Completeness Check
        issues = self.check_completeness()
        issue_panel = Panel(
            "\n".join([f"- {i}" for i in issues]) if issues else "[green]No completeness issues found.[/green]",
            title="[bold red]Completeness Audit[/bold red]",
            border_style="red" if issues else "green"
        )

        # 2. Redundancy Check
        redundant = self.check_redundancy()
        
        # 3. Token Timeline (ASCII)
        timeline_text = Text()
        max_t = max((t.tokens for t in self.turns), default=1)
        for t in self.turns:
            bar_len = int((t.tokens / max_t) * 40)
            bar = "█" * bar_len
            color = "red" if t.tokens > 2000 else "green"
            if t.is_redundant:
                color = "yellow"
            timeline_text.append(f"{t.index:02d} [{t.role[:4]}] {bar} ({t.tokens}) {t.redundancy_reason}\n", style=color)
        
        timeline_panel = Panel(timeline_text, title="[bold blue]Token Timeline[/bold blue]")

        # Table of Turns
        table = Table(title="Context Turns")
        table.add_column("Idx", style="cyan", no_wrap=True)
        table.add_column("Role", style="magenta")
        table.add_column("Preview", style="white")
        table.add_column("Tokens", justify="right", style="green")
        table.add_column("Status", justify="center")

        for t in self.turns:
            status = "⚠️" if t.has_error else "✅"
            if t.is_redundant:
                status += " ✂️"
            table.add_row(str(t.index), t.role, t.content_preview, str(t.tokens), status)

        layout["main"].split_row(
            Layout(Panel(table), ratio=2),
            Layout(Layout(name="analysis"), ratio=1)
        )
        layout["analysis"].split_column(issue_panel, timeline_panel)
        
        # Footer
        layout["footer"].update(Panel("[dim]Generated by context-profiler v0.1[/dim]", style="grey50"))

        console.print(layout)

def compare_contexts(file1: str, file2: str):
    """Compare two context dump files."""
    try:
        with open(file1, 'r') as f1, open(file2, 'r') as f2:
            d1 = json.load(f1)
            d2 = json.load(f2)
    except FileNotFoundError:
        console.print("[red]Error: File not found.[/red]")
        return

    auditor1 = ContextAuditor(d1)
    auditor2 = ContextAuditor(d2)
    
    table = Table(title="Context Comparison")
    table.add_column("Metric", style="cyan")
    table.add_column(f"{file1}", style="green")
    table.add_column(f"{file2}", style="yellow")
    table.add_column("Diff", style="bold red")

    # Compare Token Counts
    t1 = auditor1.report.get("total_prompt_tokens", 0)
    t2 = auditor2.report.get("total_prompt_tokens", 0)
    diff = t2 - t1
    diff_str = f"+{diff}" if diff > 0 else str(diff)
    table.add_row("Total Tokens", str(t1), str(t2), diff_str)

    # Compare Turn Counts
    c1 = len(auditor1.turns)
    c2 = len(auditor2.turns)
    diff_c = c2 - c1
    table.add_row("Turn Count", str(c1), str(c2), f"+{diff_c}" if diff_c > 0 else str(diff_c))

    console.print(table)
    
    # Structural Diff (Simple)
    console.print("\n[bold]Structural Changes:[/bold]")
    min_len = min(c1, c2)
    for i in range(min_len):
        turn1 = auditor1.turns[i]
        turn2 = auditor2.turns[i]
        if turn1.tokens != turn2.tokens: # Naive check
             console.print(f"Turn {i}: Token count changed {turn1.tokens} -> {turn2.tokens}")

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Audit Rust ClawOC Context Dumps")
    subparsers = parser.add_subparsers(dest="command", required=True)

    # Audit command
    audit_parser = subparsers.add_parser("audit", help="Audit a single context dump file")
    audit_parser.add_argument("file", help="Path to debug_context.json")

    # Diff command
    diff_parser = subparsers.add_parser("diff", help="Compare two context dump files")
    diff_parser.add_argument("file1", help="Path to first debug_context.json")
    diff_parser.add_argument("file2", help="Path to second debug_context.json")

    args = parser.parse_args()

    if args.command == "audit":
        try:
            with open(args.file, 'r') as f:
                data = json.load(f)
            auditor = ContextAuditor(data)
            auditor.print_report()
        except Exception as e:
            console.print(f"[red]Error loading file: {e}[/red]")
    elif args.command == "diff":
        compare_contexts(args.file1, args.file2)
