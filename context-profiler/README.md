# Claw-Context Profiler

A Python tool for analyzing, auditing, and comparing Rust ClawOC context dumps (`debug_context.json`).

## Features
- **Completeness Audit**: Checks for missing user intents, system prompts, and memory connectivity.
- **Redundancy Detection**: Flags high-volume tool outputs and repetitive errors.
- **Token Timeline**: Visualizes token usage per turn.
- **Context Diff**: Compares two context dumps to identify structural changes.

## Usage

1. Install dependencies:
   ```bash
   pip install -r requirements.txt
   ```

2. Run an audit on a single dump:
   ```bash
   python claw_audit.py audit debug_context.json
   ```

3. Compare two dumps:
   ```bash
   python claw_audit.py diff debug_context_v1.json debug_context_v2.json
   ```

## Output Example
The tool outputs a rich terminal dashboard with color-coded status indicators for each turn.

![Example Output](https://example.com/screenshot.png) 
