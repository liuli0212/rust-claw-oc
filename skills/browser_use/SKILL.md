---
name: browser_use
description: 使用 browser-use 库和 Gemini 模型自主操作浏览器完成复杂任务。
trigger: manual_only
allowed_tools: [execute_bash]
parameters:
  task:
    type: string
    description: 要在浏览器中执行的具体任务（例如：“在 GitHub 上搜索 browser-use 项目并告诉我它的 Star 数量”）。
    required: true
---
# Browser Use Skill

这个技能允许 Agent 使用 `browser-use` 库和 Gemini 模型来操作浏览器。它可以处理复杂的网页交互、数据抓取和自动化任务。

## Parameters

- `task` (string, required): 要执行的任务描述。

## Execution

使用 `execute_bash` 运行：

```bash
source skills/browser_use/venv/bin/activate && python skills/browser_use/browser_agent.py "<task>"
```

注意：确保 `GEMINI_API_KEY` 已在环境变量中设置。
