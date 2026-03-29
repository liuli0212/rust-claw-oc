use rusty_claw::telegram::parse_telegram_reply_to;

#[tokio::test]
async fn test_telegram_output_routing() {
    assert_eq!(parse_telegram_reply_to("tg_123456789"), Some(123456789));
    assert_eq!(
        parse_telegram_reply_to("telegram:987654321"),
        Some(987654321)
    );
    assert_eq!(parse_telegram_reply_to("cli"), None);
    assert_eq!(parse_telegram_reply_to("tg_invalid"), None);
}
