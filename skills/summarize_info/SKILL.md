---
name: summarize_info
description: 使用 summarize 工具和 OpenRouter 免费模型总结网页或文本信息。
trigger: manual_only
allowed_tools: [execute_bash]
parameters:
  input:
    type: string
    description: 要总结的 URL、本地文件路径或文本内容。
    required: true
  language:
    type: string
    description: 输出语言（例如：zh, en）。默认为 zh。
    required: false
preamble:
  shell: "summarize \"{{input}}\" --model free --language \"{{language}}\""
---
# Summarize Info

使用 OpenRouter 免费模型总结网页或文本信息。

## Parameters

- `input` (string, required): 要总结的 URL、本地文件路径或文本内容。
- `language` (string, optional): 输出语言（例如：zh, en）。默认为 zh。
