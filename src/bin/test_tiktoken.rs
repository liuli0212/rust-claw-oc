use std::fs;
use std::io::{BufRead, BufReader};

fn main() {
    println!("Loading BPE...");
    let bpe = tiktoken_rs::cl100k_base().unwrap();

    let path = "rusty_claw/sessions/telegram_8578308394.jsonl";
    println!("Reading {}...", path);
    let file = fs::File::open(path).expect("failed to open transcript");
    let reader = BufReader::new(file);

    let mut num_lines = 0;
    for (i, line) in reader.lines().enumerate() {
        let text = line.unwrap();
        if text.trim().is_empty() {
            continue;
        }

        // Print progress
        println!("Line {}: encoding {} bytes...", i, text.len());

        let start = std::time::Instant::now();
        let tokens = bpe.encode_with_special_tokens(&text);
        let elapsed = start.elapsed();

        println!("Line {}: {} tokens, took {:?}", i, tokens.len(), elapsed);
        num_lines += 1;

        if elapsed.as_millis() > 100 {
            println!("WARNING: encoding took {} ms!", elapsed.as_millis());
        }
    }

    println!("Successfully processed {} lines.", num_lines);
}
