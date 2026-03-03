# Context Profiler - Technical Design Document

## 1. Overview
**Context Profiler** is a production-grade static analysis tool for LLM context windows. It identifies inefficiencies, security risks, and "token bloat" in the interaction history between Agents and LLMs.

**Goal:** Reduce token usage and improve model attention by detecting low-value information patterns.

## 2. Architecture Principles
*   **Modularity:** Rules are plugins. New optimization strategies can be added without touching the core engine.
*   **Agnosticism:** The core analyzer works on a normalized `ContextNode` format, making it adaptable to different LLM providers (OpenAI, Anthropic, OpenClaw internal).
*   **Zero-Destruction:** The tool suggests changes or generates a "Shadow Context" (optimized view) but never overwrites original logs destructively by default.
*   **Configurability:** All thresholds (e.g., "Max Lines for Grep") must be configurable via YAML/TOML.

## 3. System Components

### 3.1. Ingestor Layer (`parsers/`)
Responsible for converting raw logs into a Normalized Context Graph.
*   **Input:** JSON files, `.log` files, or API response objects.
*   **Output:** `List[ContextMessage]` (Pydantic Models).
*   **Supported Formats:**
    *   `OpenAIFormat` (Standard `messages` array)
    *   `OpenClawFormat` (Internal session dumps)

### 3.2. Analysis Engine (`engine.py`)
Iterates through the Normalized Context and applies active Rules.
*   **Passes:**
    1.  **Fast Pass:** Regex/Heuristic-based (e.g., Large JSON detection).
    2.  **Semantic Pass (Optional):** Uses a small LLM to judge relevance (e.g., "Is this grep output actually answering the user's question?").

### 3.3. Rule System (`rules/`)
Each rule inherits from `BaseRule`.
*   **`GrepBloatRule`**: Detects extensive file listings or content dumps from search tools.
*   **`JsonDumpRule`**: Detects raw JSON blobs that exceed token density thresholds.
*   **`HtmlNoiseRule`**: Identifies uncleaned HTML (scripts, styles) from web fetches.
*   **`StacktraceRule`**: Compresses repetitive error stack traces.
*   **`ConversationDriftRule`**: Suggests summarizing old history if topic has shifted.

### 3.4. Reporting Layer (`reporters/`)
*   **CLI Reporter:** Rich terminal output (Tables, Trees).
*   **JSON Reporter:** Machine-readable output for CI/CD integration.
*   **Diff Reporter:** Shows `Original` vs `Optimized` context preview.

## 4. Data Structures (Pydantic)

```python
class MessageRole(str, Enum):
    SYSTEM = "system"
    USER = "user"
    ASSISTANT = "assistant"
    TOOL = "tool"

class ContextMessage(BaseModel):
    id: str
    role: MessageRole
    content: str
    metadata: Dict[str, Any]
    token_count: int

class OptimizationSuggestion(BaseModel):
    rule_id: str
    severity: Literal["INFO", "WARNING", "CRITICAL"]
    target_message_id: str
    description: str
    actionable_advice: str
    savings_estimate: int
```

## 5. Configuration (`config.yaml`)

```yaml
rules:
  grep_bloat:
    enabled: true
    max_lines: 50
    suggest_flags: ["-l", "| head"]
  
  json_dump:
    enabled: true
    max_chars: 5000
```

## 6. Development Workflow
1.  **Environment:** Python 3.10+ managed via Poetry.
2.  **Linting:** `ruff` for linting/formatting, `mypy` for static typing.
3.  **Testing:** `pytest` with >80% coverage requirement.
4.  **Distribution:** Build as a PyPI package / Docker container.

## 7. Roadmap to Production
1.  **Skeleton:** Set up Poetry, Pydantic models, CLI entry point.
2.  **Core:** Implement the `Analyzer` and `BaseRule`.
3.  **Rule Implementation:** Port the "Grep" logic and add "JSON" logic.
4.  **CLI:** Implement `rich` based UI.
5.  **Test:** Unit tests for rules.
6.  **Release:** Push to GitHub `main`.
