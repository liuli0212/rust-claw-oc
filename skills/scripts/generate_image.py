import os
import sys
import json
import base64
import requests

def generate_image(prompt, output_path):
    api_key = os.environ.get("GEMINI_API_KEY")
    if not api_key:
        return "Error: GEMINI_API_KEY environment variable not set."

    url = f"https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-image:generateContent?key={api_key}"

    payload = {
        "contents": [{
            "parts": [{"text": prompt}]
        }]
    }

    try:
        # Configuration for proxy if needed
        proxies = {
            "http": os.environ.get("HTTP_PROXY") or os.environ.get("http_proxy"),
            "https": os.environ.get("HTTPS_PROXY") or os.environ.get("https_proxy")
        }
        proxies = {k: v for k, v in proxies.items() if v}

        response = requests.post(url, json=payload, proxies=proxies, timeout=120)
        response.raise_for_status()
        data = response.json()

        if "candidates" in data and len(data["candidates"]) > 0:
            candidate = data["candidates"][0]
            if "content" in candidate and "parts" in candidate["content"]:
                for part in candidate["content"]["parts"]:
                    if "inlineData" in part:
                        image_data_base64 = part["inlineData"]["data"]
                        image_bytes = base64.b64decode(image_data_base64)
                        with open(output_path, 'wb') as f:
                            f.write(image_bytes)
                        return f"Successfully generated image and saved to {output_path}"
        
        return f"Unexpected API response format or no image generated: {json.dumps(data)}"
    except Exception as e:
        return f"Error calling Gemini API: {str(e)}"

if __name__ == "__main__":
    # Handle arguments
    prompt_text = sys.argv[1] if len(sys.argv) > 1 else ""
    out_path = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2].strip() else "generated_image.png"

    if not prompt_text:
        print("Usage: python3 generate_image.py <prompt> [output_path]")
        sys.exit(1)

    print(generate_image(prompt_text, out_path))
