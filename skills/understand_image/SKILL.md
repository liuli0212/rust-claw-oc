---
name: understand_image
description: Uses Google Gemini API to analyze and understand an image file.
trigger: manual_only
allowed_tools: [execute_bash]
parameters:
  image_path:
    type: string
    description: The local path to the image file to analyze.
    required: true
  prompt:
    type: string
    description: 'Specific question or instruction about the image. Example: "What is in this image?"'
    required: true
---
# Understand Image

This skill uses the Google Gemini API to analyze and understand an image file.

## Parameters

- `image_path` (string, required): The local path to the image file to analyze.
- `prompt` (string, required): Specific question or instruction about the image.

## Execution

Use `execute_bash` to run:

```bash
python3 skills/scripts/vision.py "{{image_path}}" "{{prompt}}"
```

Summarize the result in plain language after the command completes.
