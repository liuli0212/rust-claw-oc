import json
import typer
from typing import Optional
from pathlib import Path
from rich.console import Console
from rich.table import Table
from rich.panel import Panel
from .engine import ContextAnalyzer
from .models import ContextMessage, MessageRole

app = typer.Typer(help="Context Profiler - Optimize your LLM context.")
console = Console()

def load_messages(file_path: Path):
    with open(file_path, 'r') as f:
        data = json.load(f)
    
    # Adapt simple list of messages to ContextMessage objects
    messages = []
    for m in data:
        # Basic mapping for common LLM message formats
        role = m.get("role", "unknown")
        content = str(m.get("content", ""))
        
        msg = ContextMessage(
            role=role,
            content=content,
            tool_name=m.get("name") or m.get("tool_name"),
            tool_args=m.get("args") or m.get("tool_args") or m.get("metadata", {}),
            raw_data=m
        )
        messages.append(msg)
    return messages

@app.command()
def analyze(
    file: Path = typer.Argument(..., help="Path to the context JSON file"),
    model: str = typer.Option("gemini-2.0-flash", help="Model name for token counting (default: gemini-2.0-flash)"),
    ci: bool = typer.Option(False, help="Exit with code 1 if CRITICAL issues found")
):
    """Analyze a context dump and show optimization suggestions."""
    if not file.exists():
        console.print(f"[red]Error: File {file} not found.[/red]")
        raise typer.Exit(1)

    try:
        messages = load_messages(file)
    except Exception as e:
        console.print(f"[red]Error parsing JSON: {e}[/red]")
        raise typer.Exit(1)

    analyzer = ContextAnalyzer(model_name=model)
    report = analyzer.analyze(messages)

    console.print(Panel(f"Analysis Report for {file.name}", style="bold blue"))
    console.print(f"Total Messages: {report.total_messages}")
    console.print(f"Total Tokens: {report.total_tokens}")
    
    score_color = "green" if report.health_score > 80 else "yellow" if report.health_score > 50 else "red"
    console.print(f"Health Score: [{score_color}]{report.health_score}/100[/{score_color}]")

    if not report.suggestions:
        console.print("\n[green]✅ No optimization suggestions found! Your context is clean.[/green]")
        return

    table = Table(title="\nOptimization Suggestions", show_header=True, header_style="bold magenta")
    table.add_column("Rule ID", style="dim")
    table.add_column("Severity")
    table.add_column("Description")
    table.add_column("Actionable Advice")
    table.add_column("Est. Savings (Tokens)")

    has_critical = False
    for sug in report.suggestions:
        sev_style = "bold red" if sug.severity == "CRITICAL" else "yellow"
        if sug.severity == "CRITICAL":
            has_critical = True
        
        table.add_row(
            sug.rule_id,
            f"[{sev_style}]{sug.severity}[/{sev_style}]",
            sug.description,
            sug.actionable_advice,
            str(sug.estimated_savings_tokens)
        )

    console.print(table)

    if ci and has_critical:
        console.print("\n[red]CI Failure: Critical context issues detected.[/red]")
        raise typer.Exit(1)

if __name__ == "__main__":
    app()
