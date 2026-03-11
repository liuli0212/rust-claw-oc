mod executor;

use anyhow::Result;
use executor::{SandboxState, WasmExecutor};
use std::path::PathBuf;

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("🚀 启动 Wasm 沙盒宿主 (Host) - 基于通用 Executor");

    let wasm_file = "../guest/target/wasm32-unknown-unknown/release/wasm_tool_guest.wasm";

    // 初始化沙盒状态：限制只能写入 ./sandbox_data，并注入两个测试机密
    let state = SandboxState::new(vec![PathBuf::from("./sandbox_data")])
        .with_secret("DATABASE_PASSWORD", "super_secret_db_pass_123!")
        .with_secret("API_KEY", "sk-abc123def456ghi789");

    // 初始化通用的执行器
    let executor = WasmExecutor::new()?;

    // 执行 Guest 中的 `run_tool_logic` 函数
    let result = executor.execute(wasm_file, "run_tool_logic", state)?;

    tracing::info!("🏁 Wasm 沙盒测试运行完成！最终结果: {}", result);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wasm_execution() {
        // 由于测试在 host 目录下执行，相对路径需要能找到 wasm 文件
        // 如果找不到文件，说明 guest 没有提前编译，在测试中我们跳过或期望失败
        let wasm_file = "../guest/target/wasm32-unknown-unknown/release/wasm_tool_guest.wasm";
        if !std::path::Path::new(wasm_file).exists() {
            println!("Wasm file not found, skipping test. Run 'cargo build --target wasm32-unknown-unknown --release' in guest directory first.");
            return;
        }

        let state = SandboxState::new(vec![PathBuf::from("./sandbox_data")])
            .with_secret("DATABASE_PASSWORD", "test_pass");

        let executor = WasmExecutor::new().unwrap();
        let result = executor.execute(wasm_file, "run_tool_logic", state).unwrap();
        assert_eq!(result, 0); // guest 测试成功时返回 0
    }
}