---
name: generate_tts_audio
description: Generates high-quality TTS audio (WAV) from text using Gemini 2.5 Flash Streaming API. Best for long-form content like podcasts.
trigger: manual_only
allowed_tools: [execute_bash]
parameters:
  text:
    type: string
    description: The text content to be converted to speech.
    required: true
  output_path:
    type: string
    description: The local path (e.g., audio.mp3) where the resulting file will be saved, will only generate MP3.
    required: true
  voice:
    type: string
    description: "The name of the voice to use. Options: Aoede (Female, Prof), Fenrir (Male, Energetic), Kore (Soft), etc."
    required: false
---
# Generate TTS Audio

This skill generates high-quality TTS audio from text using the Gemini 2.5 Flash Streaming API.

## Parameters

- `text` (string, required): The text content to be converted to speech.
- `output_path` (string, required): The local path for the output file (MP3 format).
- `voice` (string, optional): Voice name. Options include Aoede (Female, Prof), Fenrir (Male, Energetic), Kore (Soft), etc.

## Execution

Use `execute_bash` to run:

```bash
python3 skills/scripts/generate_tts.py "{{text}}" "{{output_path}}" "{{voice}}"
```

If `voice` is omitted, let the script use its default behavior.
