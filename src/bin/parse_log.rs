use serde_json::Value;
use std::fs::File;
use std::io::{BufRead, BufReader};

fn main() {
    if let Ok(file) = File::open("logs/claw.log.2026-03-11") {
        let reader = BufReader::new(file);
        let mut last_body = String::new();
        for line in reader.lines().map_while(Result::ok) {
            if let Some(idx) = line.find("OpenAI stream body: ") {
                last_body = line[idx + 20..].to_string();
            }
        }

        if !last_body.is_empty() {
            if let Ok(v) = serde_json::from_str::<Value>(&last_body) {
                if let Some(msgs) = v.get("messages").and_then(|v| v.as_array()) {
                    for m in msgs
                        .iter()
                        .filter(|x| x.get("role").and_then(|r| r.as_str()) != Some("system"))
                    {
                        println!("{}", serde_json::to_string_pretty(m).unwrap_or_default());
                        println!("------");
                    }
                }
            } else {
                println!("Could not parse exact last body. Needs truncation check.");
            }
        }
    }
}
