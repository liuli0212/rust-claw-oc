use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use wasmtime::*;

pub struct SandboxState {
#[allow(dead_code)]
    pub allowed_dirs: Vec<PathBuf>,
    pub secrets: HashMap<String, String>,
}

impl SandboxState {
    pub fn new(allowed_dirs: Vec<PathBuf>) -> Self {
        Self {
            allowed_dirs,
            secrets: HashMap::new(),
        }
    }

    pub fn with_secret(mut self, key: &str, value: &str) -> Self {
        self.secrets.insert(key.to_string(), value.to_string());
        self
    }

    pub fn is_path_allowed(&self, path: &str) -> bool {
        let path = Path::new(path);
        if path.is_absolute() {
            tracing::warn!("🛡️ 沙盒拦截：尝试写入绝对路径 {:?}", path);
            return false;
        }
        for component in path.components() {
            if component == std::path::Component::ParentDir {
                tracing::warn!("🛡️ 沙盒拦截：尝试使用 '..' 逃逸目录 {:?}", path);
                return false;
            }
        }
        true
    }
}

// 辅助函数：从 Wasm 内存中读取字符串
fn read_string_from_memory(memory: &Memory, caller: &mut Caller<'_, SandboxState>, ptr: u32, len: u32) -> Result<String> {
    // 获取内存数据切片
    let data = memory.data(&*caller);
    let ptr = ptr as usize;
    let len = len as usize;
    if ptr + len > data.len() {
        anyhow::bail!("Memory out of bounds");
    }
    let string_slice = &data[ptr..ptr + len];
    Ok(String::from_utf8_lossy(string_slice).to_string())
}

/// 通用的 Wasm 执行器
pub struct WasmExecutor {
    engine: Engine,
}

impl WasmExecutor {
    pub fn new() -> Result<Self> {
        Ok(Self {
            engine: Engine::default(),
        })
    }

    /// 执行指定的 Wasm 模块和函数
    pub fn execute(&self, wasm_file_path: &str, function_name: &str, state: SandboxState) -> Result<i32> {
        let module = Module::from_file(&self.engine, wasm_file_path)
            .context(format!("Failed to load wasm module at {}", wasm_file_path))?;

        let mut store = Store::new(&self.engine, state);
        let mut linker = Linker::new(&self.engine);

        linker.func_wrap(
            "env",
            "host_write_file",
            |mut caller: Caller<'_, SandboxState>, path_ptr: u32, path_len: u32, content_ptr: u32, content_len: u32| -> i32 {
                let memory = match caller.get_export("memory") {
                    Some(Extern::Memory(m)) => m,
                    _ => return -1,
                };

                let path = match read_string_from_memory(&memory, &mut caller, path_ptr, path_len) {
                    Ok(s) => s,
                    Err(_) => return -2,
                };
                let content = match read_string_from_memory(&memory, &mut caller, content_ptr, content_len) {
                    Ok(s) => s,
                    Err(_) => return -3,
                };

                tracing::info!("📥 [Wasm Request] 请求写入文件: '{}'", path);

                if !caller.data().is_path_allowed(&path) {
                    tracing::error!("❌ [Sandbox Guard] 拒绝写入请求：越权路径 ({})", path);
                    return -4;
                }

                let safe_path = format!("./sandbox_out/{}", path);
                let _ = fs::create_dir_all("./sandbox_out");
                
                match fs::write(&safe_path, content) {
                    Ok(_) => {
                        tracing::info!("✅ [Host Execution] 文件已安全写入到: {}", safe_path);
                        0
                    },
                    Err(e) => {
                        tracing::error!("❌ [Host Execution] 写入失败: {}", e);
                        -5
                    }
                }
            }
        )?;

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

                let secret_val = match caller.data().secrets.get(&key) {
                    Some(v) => v.clone(),
                    None => {
                        tracing::warn!("❌ [Host Execution] 未找到请求的机密: {}", key);
                        return 0;
                    }
                };

                let secret_bytes = secret_val.as_bytes();
                if secret_bytes.len() > buf_max as usize {
                    tracing::error!("❌ [Host Execution] 缓冲区域过小，无法传递机密");
                    return -3;
                }

                let ptr = buf_ptr as usize;
                let data = memory.data_mut(&mut caller);
                data[ptr..ptr + secret_bytes.len()].copy_from_slice(secret_bytes);

                tracing::info!("🔐 [Host Execution] 已向 Wasm 内存安全注入机密 ({} 字节)", secret_bytes.len());
                
                secret_bytes.len() as i32
            }
        )?;

        tracing::info!("📦 正在实例化 Wasm 沙盒...");
        let instance = linker.instantiate(&mut store, &module)?;
        
        let run_tool = instance.get_typed_func::<(), i32>(&mut store, function_name)?;
        
        tracing::info!("▶️ 开始执行 {}...", function_name);
        let result = run_tool.call(&mut store, ())?;
        tracing::info!("⏹️ 执行完毕，返回码: {}", result);

        Ok(result)
    }
}