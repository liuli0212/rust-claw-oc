use std::process::Command;

#[test]
fn test_binary_headless_smoke() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rusty-claw"));
    
    cmd.arg("--command").arg("echo hello");
    cmd.arg("--provider").arg("invalid_provider");
    
    // Unset API keys so it doesn't accidentally fall back to a valid provider
    // if the developer has them set in their local environment.
    cmd.env_remove("GEMINI_API_KEY");
    cmd.env_remove("DASHSCOPE_API_KEY");
    
    let output = cmd.output().expect("Failed to execute binary");
    
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    
    // The binary should exit with an error because of the invalid provider
    assert!(!output.status.success(), "Expected failure due to invalid provider");
    
    // It should print an error message about the provider
    assert!(
        stderr.contains("GEMINI_API_KEY must be set") || 
        stderr.contains("Failed to initialize default LLM") || 
        stderr.contains("No LLM provider configured") ||
        stdout.contains("GEMINI_API_KEY must be set") ||
        stdout.contains("Failed to initialize default LLM") ||
        stdout.contains("No LLM provider configured"),
        "Expected error message about LLM initialization, got stdout: {}, stderr: {}",
        stdout, stderr
    );
}
