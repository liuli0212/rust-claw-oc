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
---
# Summarize Info

使用 OpenRouter 免费模型总结网页或文本信息。

## Parameters

- `input` (string, required): 要总结的 URL、本地文件路径或文本内容。
- `language` (string, optional): 输出语言（例如：zh, en）。默认为 zh。

## Execution

先阅读注入到 prompt 里的 `Skill Arguments (JSON)`，从中获取 `input` 和可选的 `language`。

然后使用 `execute_bash` 运行：

```bash
summarize "<input>" --model free --language "<language>"
```

如果 `language` 为空，则省略 `--language` 参数并使用工具默认语言。不要自己做模板替换，直接根据参数块里的值构造命令。
