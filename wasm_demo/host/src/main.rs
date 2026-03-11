use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use wasmtime::*;

// 我们定义一个沙盒的系统状态，用于在 Host Functions 中共享
struct SandboxState {
    allowed_dirs: Vec<PathBuf>,
    secrets: std::collections::HashMap<String, String>,
}

impl SandboxState {
    pub fn new(allowed_dirs: Vec<PathBuf>) -> Self {
        let mut secrets = std::collections::HashMap::new();
        secrets.insert(
            "DATABASE_PASSWORD".to_string(),
            "super_secret_db_pass_123!".to_string(),
        );
        secrets.insert(
            "API_KEY".to_string(),
            "sk-abc123def456ghi789".to_string(),
        );
        
        Self {
            allowed_dirs,
            secrets,
        }
    }

    // 核心安全逻辑：路径审计器
    pub fn is_path_allowed(&self, path: &str) -> bool {
        let path = Path::new(path);
        
        // 1. 拒绝绝对路径（防逃逸）
        if path.is_absolute() {
            tracing::warn!("🛡️  沙盒拦截：尝试写入绝对路径 {:?}", path);
            return false;
        }

        // 2. 拒绝包含向上层级导航的路径（防跳出目录）
        for component in path.components() {
            if component == std::path::Component::ParentDir {
                tracing::warn!("🛡️  沙盒拦截：尝试使用 '..' 逃逸目录 {:?}", path);
                return false;
            }
        }

        // 3. 我们可以进一步限制只写特定后缀名，或只能在 allowed_dirs 内
        // 在这个 demo 里，只要是合法的相对路径，且不含 '..', 我们认为它会在 cwd 下
        true
    }
}

// 辅助函数：从 Wasm 内存中读取字符串
fn read_string_from_memory(memory: &Memory, store: impl AsContextMut, ptr: u32, len: u32) -> Result<String> {
    let data = memory.data(store);
    let ptr = ptr as usize;
    let len = len as usize;
    
    if ptr + len > data.len() {
        anyhow::bail!("Memory out of bounds");
    }
    
    let string_slice = &data[ptr..ptr+len];
    Ok(String::from_utf8_lossy(string_slice).to_string())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::info!("🚀 启动 Wasm 沙盒宿主 (Host)...");

    // 1. 初始化引擎
    let engine = Engine::default();
    
    // 2. 加载我们要执行的 Wasm 模块（Guest）
    // 注意：我们要先编译 Guest 才能拿到这个 wasm 文件
    let wasm_file = "../guest/target/wasm32-unknown-unknown/release/wasm_tool_guest.wasm";
    let module = Module::from_file(&engine, wasm_file)
        .context("Failed to load wasm module. Did you run 'cargo build --target wasm32-unknown-unknown --release' in the guest folder?")?;

    // 3. 初始化 Store 和状态
    let state = SandboxState::new(vec![PathBuf::from("./sandbox_data")]);
    let mut store = Store::new(&engine, state);

    // 4. 创建 Linker 并链接宿主函数 (Host Functions)
    let mut linker = Linker::new(&engine);

    // 暴露 host_write_file 给 Wasm
    linker.func_wrap(
        "env",
        "host_write_file",
        |mut caller: Caller<'_, SandboxState>, path_ptr: u32, path_len: u32, content_ptr: u32, content_len: u32| -> i32 {
            // a. 获取 Wasm 的线性内存
            let memory = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            // b. 从 Wasm 内存中提取路径和内容
            let path = match read_string_from_memory(&memory, &mut caller, path_ptr, path_len) {
                Ok(s) => s,
                Err(_) => return -2,
            };
            
            let content = match read_string_from_memory(&memory, &mut caller, content_ptr, content_len) {
                Ok(s) => s,
                Err(_) => return -3,
            };

            tracing::info!("📥 [Wasm Request] 请求写入文件: '{}'", path);

            // c. 拦截器与权限审计 (Security Interceptor)
            if !caller.data().is_path_allowed(&path) {
                tracing::error!("❌ [Sandbox Guard] 拒绝写入请求：越权路径 ({})", path);
                return -4; // 权限被拒绝
            }

            // 模拟或实际执行写入
            // 为了安全，强制在特定的前缀目录下执行
            let safe_path = format!("./sandbox_out/{}", path);
            let _ = fs::create_dir_all("./sandbox_out");
            
            match fs::write(&safe_path, content) {
                Ok(_) => {
                    tracing::info!("✅ [Host Execution] 文件已安全写入到: {}", safe_path);
                    0 // 成功
                },
                Err(e) => {
                    tracing::error!("❌ [Host Execution] 写入失败: {}", e);
                    -5
                }
            }
        }
    )?;

    // 暴露 host_get_secret 给 Wasm
    linker.func_wrap(
        "env",
        "host_get_secret",
        |mut caller: Caller<'_, SandboxState>, key_ptr: u32, key_len: u32, buf_ptr: u32, buf_max: u32| -> i32 {
            let memory = match caller.get_export("memory") {
                Some(Extern::Memory(m)) => m,
                _ => return -1,
            };

            let key = match read_string_from_memory(&memory, &mut caller, key_ptr, key_len) {
                Ok(s) => s,
                Err(_) => return -2,
            };

            tracing::info!("📥 [Wasm Request] 请求机密信息: '{}'", key);

            // a. 查找机密
            let secret_val = match caller.data().secrets.get(&key) {
                Some(v) => v.clone(),
                None => {
                    tracing::warn!("❌ [Host Execution] 未找到请求的机密: {}", key);
                    return 0; // 0 字节写入
                }
            };

            // b. 校验缓冲区是否足够大
            let secret_bytes = secret_val.as_bytes();
            if secret_bytes.len() > buf_max as usize {
                tracing::error!("❌ [Host Execution] 缓冲区域过小，无法传递机密");
                return -3;
            }

            // c. 注入机密到 Wasm 内存中
            let ptr = buf_ptr as usize;
            let data = memory.data_mut(&mut caller);
            data[ptr..ptr + secret_bytes.len()].copy_from_slice(secret_bytes);

            tracing::info!("🔐 [Host Execution] 已向 Wasm 内存安全注入机密 ({} 字节)", secret_bytes.len());
            // 我们不在日志中打印密码本身
            
            secret_bytes.len() as i32
        }
    )?;

    // 5. 实例化沙盒并执行
    tracing::info!("📦 正在实例化 Wasm 沙盒...");
    let instance = linker.instantiate(&mut store, &module)?;
    
    // 6. 获取导出函数并调用
    let run_tool = instance.get_typed_func::<(), i32>(&mut store, "run_tool_logic")?;
    
    tracing::info!("▶️ 开始执行 Wasm Tool Logic...");
    let result = run_tool.call(&mut store, ())?;
    tracing::info!("⏹️ Wasm Tool 执行完毕，返回码: {}", result);

    Ok(())
}