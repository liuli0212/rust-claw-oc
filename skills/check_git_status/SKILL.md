---
name: check_git_status
description: Runs git status and returns the result.
trigger: manual_only
allowed_tools: [execute_bash]
preamble:
  shell: "git status"
---
# Check Git Status

This skill runs `git status` in the current working directory and returns the result.
