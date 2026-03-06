---
name: generate_tts_audio
description: Generates high-quality TTS audio (WAV) from text using Gemini 2.5 Flash Streaming API. Best for long-form content like podcasts.
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

python3 skills/scripts/generate_tts.py "{{text}}" "{{output_path}}" "{{voice}}"
