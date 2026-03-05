---
name: understand_image
description: Uses Google Gemini API to analyze and understand an image file.
parameters:
  image_path:
    type: string
    description: The local path to the image file to analyze.
    required: true
  prompt:
    type: string
    description: Specific question or instruction about the image. Example "What is in this image?"
    required: true
---

python3 skills/scripts/vision.py "{{image_path}}" "{{prompt}}"
