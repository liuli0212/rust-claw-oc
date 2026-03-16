# WASM Demo Compilation Guide

This demo consists of a **Guest** (WASM module) and a **Host** (Rust executor using Wasmtime).

## 1. Prerequisites

You need the Rust WASM target installed:
```bash
rustup target add wasm32-unknown-unknown
```

## 2. Compile the Guest

Navigate to the `guest` directory and compile it to Wasm:
```bash
cd wasm_demo/guest
cargo build --target wasm32-unknown-unknown --release
```
The output will be at `wasm_demo/guest/target/wasm32-unknown-unknown/release/wasm_tool_guest.wasm`.

## 3. Run the Host

The host is configured to look for the Guest's wasm binary at the path above.
Navigate to the `host` directory and run it:
```bash
cd wasm_demo/host
cargo run
```

## What this demo does:
- The **Guest** tries to perform three operations:
  1. Write a safe file.
  2. Attempt a directory traversal attack (`../../etc/passwd_mock`).
  3. Retrieve a secret key from the host.
- The **Host** uses a secure Sandbox executor to:
  1. Allow safe writes.
  2. Block the directory traversal attack.
  3. Provide controlled access to secrets.
