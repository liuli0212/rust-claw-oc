import os
import sys
import json
import base64
import requests

def analyze_image(image_path, prompt="What is in this image?"):
    api_key = os.environ.get("GEMINI_API_KEY")
    if not api_key:
        return "Error: GEMINI_API_KEY environment variable not set."

    if not os.path.exists(image_path):
        return f"Error: Image file not found at {image_path}"

    ext = os.path.splitext(image_path)[1].lower()
    mime_type = "image/jpeg"
    if ext == ".png":
        mime_type = "image/png"
    elif ext == ".webp":
        mime_type = "image/webp"
    elif ext == ".gif":
        mime_type = "image/gif"

    try:
        with open(image_path, "rb") as f:
            image_data = base64.b64encode(f.read()).decode("utf-8")
    except Exception as e:
        return f"Error reading image: {str(e)}"

    url = f"https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent?key={api_key}"

    payload = {
        "contents": [{
            "parts": [
                {"text": prompt},
                {
                    "inline_data": {
                        "mime_type": mime_type,
                        "data": image_data
                    }
                }
            ]
        }]
    }

    try:
        # Configuration for proxy if needed (often required for local dev environments)
        proxies = {
            "http": os.environ.get("HTTP_PROXY") or os.environ.get("http_proxy"),
            "https": os.environ.get("HTTPS_PROXY") or os.environ.get("https_proxy")
        }
        proxies = {k: v for k, v in proxies.items() if v}

        response = requests.post(url, json=payload, proxies=proxies, timeout=60)
        response.raise_for_status()
        data = response.json()

        if "candidates" in data and len(data["candidates"]) > 0:
            candidate = data["candidates"][0]
            if "content" in candidate and "parts" in candidate["content"]:
                return candidate["content"]["parts"][0].get("text", "No text generated.")

        return f"Unexpected API response format: {json.dumps(data)}"
    except Exception as e:
        return f"Error calling Gemini API: {str(e)}"

if __name__ == "__main__":
    # Ensure sys.argv[2] (prompt) is handled even if empty or quoted weirdly
    img_path = sys.argv[1] if len(sys.argv) > 1 else ""
    prompt_text = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2].strip() else "Please describe this image in detail."

    if not img_path:
        print("Usage: python3 vision.py <image_path> [prompt]")
        sys.exit(1)

    print(analyze_image(img_path, prompt_text))
