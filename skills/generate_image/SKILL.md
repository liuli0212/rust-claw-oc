---
name: generate_image
description: Generates an image from a text prompt using Google Gemini 2.5 Flash Image API.
trigger: manual_only
allowed_tools: [execute_bash]
parameters:
  prompt:
    type: string
    description: The text description of the image to generate.
    required: true
  output_path:
    type: string
    description: The local path (e.g., image.png) where the resulting image will be saved.
    required: true
preamble:
  shell: 'python3 skills/scripts/generate_image.py "{{prompt}}" "{{output_path}}"'
---
# Generate Image

This skill generates images from text descriptions using the Google Gemini 2.5 Flash Image API.

## Usage

Provide a text prompt describing the desired image and an output file path.

## Parameters

- `prompt` (string, required): The text description of the image to generate.
- `output_path` (string, required): The local path (e.g., image.png) where the resulting image will be saved.
