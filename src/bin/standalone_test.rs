use reqwest::Client;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = std::env::var("DASHSCOPE_API_KEY").expect("Need DASHSCOPE_API_KEY");
    let base_url = "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions";

    let client = Client::new();

    // Exact payload that rusty-claw sends
    let body = json!({
        "model": "qwen-plus",
        "messages": [
            {
                "role": "user",
                "content": "Please use the dummy_read tool to read the file 'test.txt'."
            }
        ],
        "stream": true,
        "parallel_tool_calls": false,
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "dummy_read",
                    "description": "Read a dummy file",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"}
                        },
                        "required": ["path"]
                    }
                }
            }
        ],
        "tool_choice": "required"
    });

    println!("Sending request to Aliyun...");

    let mut response = client
        .post(base_url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;

    println!("Response status: {}", response.status());

    while let Some(chunk) = response.chunk().await? {
        let text = String::from_utf8_lossy(&chunk);
        println!("CHUNK: {}", text);
    }

    Ok(())
}
