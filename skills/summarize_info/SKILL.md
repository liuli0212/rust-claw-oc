---
name: summarize_info
description: 使用强大的 summarize 工具总结网页、YouTube 视频、本地文件或文本。支持视频 OCR、语音转文字和多模态提取。
trigger: manual_only
allowed_tools: [execute_bash]
parameters:
  input:
    type: string
    description: 要总结的 URL、YouTube 链接、本地文件路径或文本内容。
    required: true
  language:
    type: string
    description: 输出语言（例如：zh, en）。默认为 zh。
    required: false
  length:
    type: string
    description: 总结长度（short, medium, long, xl）。默认为 medium。
    required: false
  youtube:
    type: string
    description: YouTube 处理模式（auto, off）。默认为 auto。
    required: false
  extract_only:
    type: boolean
    description: 是否仅提取内容而不进行总结。
    required: false
  model:
    type: string
    description: 使用的模型（例如：gemini3, free, google/gemini-3-flash-preview）。默认为 gemini3。
    required: false
---
# Summarize Info

使用强大的 `summarize` 工具（steipete/summarize）总结各类信息。该工具支持：
- **网页总结**：自动提取干净的 Markdown 内容。
- **YouTube/视频**：自动抓取字幕或进行语音转文字（ASR），支持视频 OCR 生成幻灯片。
- **本地文件**：支持 PDF、图像、音频和视频。

## Parameters

- `input` (string, required): 输入源。
- `language` (string, optional): 输出语言，默认为 `zh`。
- `length` (string, optional): 总结长度，可选 `short`, `medium`, `long`, `xl`。
- `youtube` (string, optional): YouTube 处理模式，可选 `auto`, `off`。
- `extract_only` (boolean, optional): 如果为 true，则仅输出提取的文本。
- `model` (string, optional): 使用的模型，默认为 `gemini3`。

## Execution

根据 `Skill Arguments (JSON)` 构造命令并使用 `execute_bash` 运行。

示例命令：
```bash
summarize "<input>" --language "<language>" --length "<length>" --youtube "<youtube>" --model "<model>"
```

如果 `extract_only` 为 true，添加 `--extract-only` 参数。
如果某个可选���数为空，则在命令中省略该参数。
