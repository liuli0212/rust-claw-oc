import subprocess
import time
import sys
import os

def run_agent_test():
    print("🚀 Starting Rusty-Claw Integration Test...")
    
    cargo_path = os.path.expanduser("~/.cargo/bin/cargo")
    
    # Run cargo build first to ensure binary is fresh
    print("Building project...")
    build_result = subprocess.run([cargo_path, 'build'], capture_output=True)
    if build_result.returncode != 0:
        print("❌ Build failed!")
        print(build_result.stderr.decode('utf-8'))
        sys.exit(1)

    # Start the rust agent process
    print("Starting agent process...")
    process = subprocess.Popen(
        [cargo_path, 'run', '--quiet'],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        bufsize=1
    )
    
    # Wait for the prompt
    time.sleep(3)
    
    test_cases = [
        {
            "name": "1. Dynamic Context Test (Runtime Info)",
            "input": "What operating system and architecture are you running on right now?\n",
            "wait": 8
        },
        {
            "name": "2. Long Output Truncation Test",
            "input": "Run `ls -laR` in this directory. Tell me if the output was truncated or how many files you see.\n",
            "wait": 15
        },
        {
            "name": "3. RAG Memory Insert Test",
            "input": "Memorize this knowledge: 'Rusty-Claw uses sqlite fts5 and fastembed for hybrid search'.\n",
            "wait": 10
        },
        {
            "name": "4. RAG Memory Recall Test",
            "input": "Search your knowledge base: What technologies does Rusty-Claw use for hybrid search?\n",
            "wait": 10
        }
    ]

    for tc in test_cases:
        print(f"\n🧪 Running Test: {tc['name']}")
        print(f"Input: {tc['input'].strip()}")
        
        process.stdin.write(tc['input'])
        process.stdin.flush()
        
        # Give agent time to process and call tools
        time.sleep(tc['wait'])

    # Trigger exit
    print("\n🚪 Sending exit signal...")
    process.stdin.write("exit\n")
    process.stdin.flush()
    
    # Collect output
    try:
        stdout, stderr = process.communicate(timeout=10)
        print("\n=== AGENT OUTPUT LOG ===")
        print(stdout)
        
        if stderr:
            print("\n=== STDERR LOG ===")
            print(stderr)
            
        # Basic assertions based on output
        assert "macOS" in stdout or "linux" in stdout or "darwin" in stdout.lower() or "apple" in stdout.lower(), "Missing OS info"
        assert "ls -laR" in stdout, "Did not execute ls command"
        assert "sqlite fts5" in stdout.lower() or "hybrid search" in stdout.lower(), "Did not recall RAG memory correctly"
            
        print("\n✅ All integration tests passed successfully!")
        
    except subprocess.TimeoutExpired:
        process.kill()
        stdout, stderr = process.communicate()
        print("❌ Process killed due to timeout. Agent got stuck. Output so far:")
        print(stdout)
        sys.exit(1)
    except AssertionError as e:
        print(f"❌ Test Assertion Failed: {e}")
        sys.exit(1)

if __name__ == "__main__":
    run_agent_test()
