---
name: check_git_status
description: Runs git status and returns the result.
trigger: manual_only
allowed_tools: [execute_bash]
---
# Check Git Status

Run `git status --short --branch` with `execute_bash` in the current working directory and summarize the result for the user.

Return the important branch and file-state information instead of pasting excessive raw output unless the user explicitly asks for it.
