---
name: generate_image
description: Generates an image from a text prompt using Google Gemini 2.5 Flash Image API.
parameters:
  prompt:
    type: string
    description: The text description of the image to generate.
    required: true
  output_path:
    type: string
    description: The local path (e.g., image.png) where the resulting image will be saved.
    required: true
---

python3 skills/scripts/generate_image.py "{{prompt}}" "{{output_path}}"
