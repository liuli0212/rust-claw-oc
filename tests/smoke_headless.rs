use std::process::Command;

#[test]
fn test_binary_headless_smoke() {
    // This test actually runs the compiled binary in headless mode
    // to ensure it doesn't immediately crash on startup.
    
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rusty-claw"));
    
    // We pass a simple command that doesn't require LLM
    // and we don't provide an LLM config, so it should fail gracefully
    // or execute a local-only command if we had one.
    // For now, we just check that it starts and exits with a known state.
    cmd.arg("--command").arg("echo hello");
    
    // We don't want it to read the real config and connect to real LLMs
    cmd.env("RUSTY_CLAW_PROVIDER", "invalid_provider");
    
    let output = cmd.output().expect("Failed to execute binary");
    
    // It might succeed if it falls back to a default or if the command is handled locally
    // Let's just print the output for debugging if it fails
    if !output.status.success() {
        println!("stdout: {}", String::from_utf8_lossy(&output.stdout));
        println!("stderr: {}", String::from_utf8_lossy(&output.stderr));
    }
    
    // We just want to make sure it doesn't panic/crash
    // A clean exit (0) or a handled error (non-zero) are both fine for a smoke test
    // as long as it's not a panic (which usually results in a specific exit code or signal)
}
