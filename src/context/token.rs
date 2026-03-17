use super::model::{Message, Turn};
use tiktoken_rs::CoreBPE;

pub(crate) fn get_bpe() -> CoreBPE {
    use once_cell::sync::Lazy;
    static BPE: Lazy<CoreBPE> = Lazy::new(|| tiktoken_rs::cl100k_base().unwrap());
    BPE.clone()
}

pub(crate) fn estimate_tokens(bpe: &CoreBPE, msg: &Message) -> usize {
    let mut count = 0;
    for part in &msg.parts {
        if let Some(text) = &part.text {
            count += bpe.encode_with_special_tokens(text).len();
        }
        if let Some(fc) = &part.function_call {
            count += bpe.encode_with_special_tokens(&fc.name).len();
            count += bpe.encode_with_special_tokens(&fc.args.to_string()).len();
        }
        if let Some(fr) = &part.function_response {
            count += bpe.encode_with_special_tokens(&fr.name).len();
            count += bpe
                .encode_with_special_tokens(&fr.response.to_string())
                .len();
        }
    }
    count
}

pub(crate) fn truncate_chars(input: &str, max_chars: usize) -> String {
    if input.chars().count() <= max_chars {
        return input.to_string();
    }
    input.chars().take(max_chars).collect()
}

pub(crate) fn turn_token_estimate(turn: &Turn, bpe: &CoreBPE) -> usize {
    turn.messages.iter().map(|m| estimate_tokens(bpe, m)).sum()
}
