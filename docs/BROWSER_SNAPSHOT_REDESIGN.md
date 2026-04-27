# Browser Snapshot Architecture: Accessibility Tree + Smart Filtering

Status: implemented. The current browser snapshot path uses CDP
`Accessibility.getFullAXTree`, stores `BackendNodeId` mappings in
`BrowserState`, and marks snapshot output with
`payload_kind: browser_snapshot` for history sanitization.

## Problem

The previous `extractor.js` DOM walker had three issues:

1. **No size limit** — A page with 300+ links returns all of them, potentially 20-40k tokens per snapshot.
2. **DOM-based extraction** — Traverses the raw DOM, missing semantic information that the accessibility tree provides for free.
3. **No context sanitization** — Browser results have no `payload_kind`, so old snapshots linger in history without meaningful compression.

## Design

### Phase 1: CDP Accessibility Tree Snapshot

The implementation replaced the JS-based DOM walker with a Rust-side CDP call to `Accessibility.getFullAXTree`.

**Why**: The accessibility tree already filters decorative elements, provides semantic roles (`button`, `link`, `textbox`), and gives meaningful names. It's exactly what screen readers use — and what LLMs need.

**Implementation**:

```rust
// In browser.rs, the snapshot action:
use chromiumoxide::cdp::browser_protocol::accessibility::{
    GetFullAxTreeParams, GetFullAxTreeReturns, AxNode,
};

let result: GetFullAxTreeReturns = page
    .execute(GetFullAxTreeParams::builder().build())
    .await?;

// Filter to interactive roles only
let interactive = filter_interactive_nodes(&result.nodes);

// Format as compact text
let output = format_snapshot(interactive, max_elements);
```

**Interactive roles to keep**:
- `button`, `link`, `menuitem`, `tab`
- `textbox`, `searchbox`, `combobox`, `spinbutton`
- `checkbox`, `radio`, `switch`, `slider`
- `select`, `listbox`, `option` (within selects)
- `menu`, `menubar`, `toolbar` (structural, only if they contain interactive children)

**Roles to skip** (decorative/structural):
- `generic`, `group`, `region`, `main`, `navigation`, `banner`, `contentinfo`
- `heading`, `paragraph`, `list`, `listitem`, `text`, `img`, `separator`
- `none`, `presentation`

**Output format** (compact, ~30-50 chars per element):
```
# Page: Example Login
[1] textbox "Email"
[2] textbox "Password" 
[3] button "Sign In"
[4] link "Forgot password?"
[5] link "Create account"
```

Each line is `[stable_id] role "name"`. When an element has a current value (e.g., filled form field), append it:
```
[1] textbox "Email" = "user@example.com"
```

**Element ID mapping**: Store a `HashMap<u32, BackendNodeId>` in `BrowserState` so that `act` can resolve `[1]` → CDP `BackendNodeId` → click/type via CDP. This replaces the current `__OC_NODE_MAP__` JS global.

### Phase 2: Smart Filtering with `hint` Parameter

The snapshot action supports an optional `hint` string. When provided, Rust-side keyword overlap scoring prioritizes relevant elements.

**Example parameter**:
```json
{"action": "snapshot", "hint": "login form submit"}
```

**Algorithm** (runs in Rust, <1ms):
1. Tokenize hint into keyword set (Unicode-aware: `\p{L}\p{N}`)
2. Score each element by keyword overlap with `role + name + value`
3. Always keep: all form fields (textbox/checkbox/select/radio) regardless of score
4. Sort by score descending, take top `max_elements` (default 80)

**Without hint**: Return all interactive elements up to `max_elements`.

### Phase 3: Output Budget + History Sanitization

1. **Hard output cap**: `max_elements` parameter (default 80, configurable). Output also capped at 8,000 chars.
2. **StructuredToolOutput envelope**: Wrap snapshot results with `payload_kind: "browser_snapshot"` so the sanitizer can strip old snapshots.
3. **Sanitize rule**: Old browser snapshots → `[browser snapshot stripped - N elements]` (same as web_content treatment).

### Phase 4: `act` Command

With `BackendNodeId` stored from the accessibility tree:
- **Click**: Use `DOM.resolveNode(backendNodeId)` → get objectId → call `Runtime.callFunctionOn` to click, or use `DOM.getBoxModel` → get coordinates → CDP `Input.dispatchMouseEvent`.
- **Type**: Use `DOM.focus(backendNodeId)` → `Input.insertText`.

This removes the need for `data-oc-id` attributes and the `__OC_NODE_MAP__` JS injection.

## Current State

- `snapshot` reads from the CDP accessibility tree.
- `act` reads from `BrowserState.node_map` instead of evaluating a JS node map.
- The external browser tool interface remains stable.

## Token Impact Estimate

| Page type | Previous DOM walk | Current AXTree snapshot |
|---|---|---|
| Login page (5 fields) | ~500 tokens | ~150 tokens |
| Google search results | ~5,000 tokens | ~800 tokens |
| Amazon product page | ~15,000 tokens | ~2,000 tokens |
| Complex SPA (Gmail) | ~30,000+ tokens | ~3,000 tokens |
