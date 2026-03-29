use rusty_claw::telegram::TelegramOutputRouter;
use rusty_claw::core::OutputRouter;
use teloxide::Bot;

#[tokio::test]
async fn test_telegram_output_routing() {
    // Create a dummy bot (it won't make network calls just by being created)
    let bot = Bot::new("dummy_token");
    let router = TelegramOutputRouter { bot };

    // Test valid telegram reply_to formats
    let output1 = router.try_route("tg_123456789");
    assert!(output1.is_some(), "Should route tg_ prefix");

    let output2 = router.try_route("telegram:987654321");
    assert!(output2.is_some(), "Should route telegram: prefix");

    // Test invalid formats
    let output3 = router.try_route("cli");
    assert!(output3.is_none(), "Should not route cli");

    let output4 = router.try_route("tg_invalid");
    assert!(output4.is_none(), "Should not route invalid chat id");
}
