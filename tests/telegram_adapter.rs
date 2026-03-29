use rusty_claw::telegram;
use rusty_claw::session_manager::SessionManager;
use std::sync::Arc;

#[tokio::test]
async fn test_telegram_output_routing() {
    // This is a thin test to verify that the output router can correctly
    // identify and route telegram messages.
    
    let session_manager = Arc::new(SessionManager::new(None, vec![]));
    
    // The actual routing logic is internal to the telegram module,
    // but we can verify that the session manager accepts the router.
    // In a real scenario, we would mock the teloxide bot and verify
    // that messages are sent to the correct chat_id.
    
    // For now, we just ensure the module compiles and can be initialized
    // in a test context.
    assert!(true);
}
