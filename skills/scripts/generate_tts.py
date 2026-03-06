import os
import sys
import json
import base64
import requests
import subprocess
import time
from datetime import datetime

def log(message):
    timestamp = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    print(f"[{timestamp}] {message}")

def convert_pcm_to_formats(raw_path, base_path):
    wav_path = base_path + ".wav"
    mp3_path = base_path + ".mp3"
    log(f"Converting raw PCM (Little Endian, 24k) to MP3...")
    try:
        # Gemini L16 is 24000Hz, Mono, Little Endian (Standard)
        # Convert to WAV
        # This is only for DEBUGGING purpose.
        #subprocess.run([
        #    'ffmpeg', '-y',
        #    '-f', 's16le', '-ar', '24000', '-ac', '1',
        #    '-i', raw_path,
        #    wav_path
        #], check=True, capture_output=True)
        # Convert to MP3
        subprocess.run([
            'ffmpeg', '-y',
            '-f', 's16le', '-ar', '24000', '-ac', '1',
            '-i', raw_path,
            '-codec:a', 'libmp3lame', '-b:a', '64k',
            mp3_path
        ], check=True, capture_output=True)
        return True
    except Exception as e:
        log(f"FFmpeg error: {e}")
        return False

def fetch_audio_stream(text, voice_name="Aoede"):
    api_key = os.environ.get("GEMINI_API_KEY")
    model = "gemini-2.5-pro-preview-tts"
    url = f"https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?key={api_key}"

    payload = {
        "contents": [{"parts": [{"text": text}]}],
        "generationConfig": {
            "response_modalities": ["AUDIO"],
            "speechConfig": {"voiceConfig": {"prebuiltVoiceConfig": {"voiceName": voice_name}}}
        }
    }
    proxies = {'http': 'http://127.0.0.1:7890', 'https': 'http://127.0.0.1:7890'}

    raw_pcm = b""
    try:
        response = requests.post(url, json=payload, proxies=proxies, stream=True, timeout=180)
        if response.status_code != 200:
            log(f"API Error {response.status_code}: {response.text}")
            return None

        buffer = ""
        for chunk in response.iter_content(chunk_size=None, decode_unicode=True):
            if not chunk: continue
            buffer += chunk
            while True:
                buffer = buffer.strip()
                if buffer.startswith('['): buffer = buffer[1:].strip()
                if buffer.startswith(','): buffer = buffer[1:].strip()
                if not buffer.startswith('{'): break

                count = 0
                end_pos = -1
                for i, char in enumerate(buffer):
                    if char == '{': count += 1
                    elif char == '}':
                        count -= 1
                        if count == 0:
                            end_pos = i + 1
                            break
                if end_pos == -1: break

                json_str = buffer[:end_pos]
                buffer = buffer[end_pos:]

                try:
                    data = json.loads(json_str)
                    if "candidates" in data:
                        parts = data["candidates"][0].get("content", {}).get("parts", [])
                        for p in parts:
                            if "inlineData" in p:
                                raw_pcm += base64.b64decode(p["inlineData"]["data"])
                            elif "text" in p:
                                log(f"Warning: Model generated text: {p['text']}")
                except: pass
        return raw_pcm
    except Exception as e:
        log(f"Network error: {e}")
        return None

def generate_dual(text, output_base, voice_name="Aoede"):
    text = text.replace('#', '').replace('*', '').replace('-', '').strip()
    CHUNK_LIMIT = 400
    chunks = [text[i:i+CHUNK_LIMIT] for i in range(0, len(text), CHUNK_LIMIT)]

    log(f"Generating Dual Formats. Model: {voice_name}. Parts: {len(chunks)}")

    all_pcm = b""
    for i, chunk in enumerate(chunks):
        log(f"Part {i+1}/{len(chunks)}...")
        pcm = fetch_audio_stream(chunk, voice_name)
        if pcm:
            all_pcm += pcm
        else:
            log(f"Failed at part {i+1}")
            return "Failed"

    if all_pcm:
        raw_file = output_base + ".raw"
        with open(raw_file, "wb") as f:
            f.write(all_pcm)
        if convert_pcm_to_formats(raw_file, output_base):
            os.remove(raw_file)
            return f"Success: Created {output_base}.mp3"
    return "No audio data."

if __name__ == "__main__":
    if len(sys.argv) < 3: sys.exit(1)
    t = sys.argv[1]
    if t.startswith("file:"):
        with open(t[5:], 'r') as f: t = f.read()
    print(generate_dual(t, sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else "Aoede"))
