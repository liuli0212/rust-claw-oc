#!/bin/bash
SESSION_FILES=$(ls -1t rusty_claw/events/*.jsonl 2>/dev/null | head -n 3)
if [ -z "$SESSION_FILES" ]; then
    echo "No event logs found."
    exit 1
fi

for f in $SESSION_FILES; do
    echo "=== Analysis for $f ==="
    jq -r '"\(.event_type): \((.payload // {}) | tojson)"' "$f" | sort | uniq -c
    echo ""
done
