// 声明从 Host 导入的函数
extern "C" {
    fn host_write_file(path_ptr: *const u8, path_len: u32, content_ptr: *const u8, content_len: u32) -> i32;
    fn host_get_secret(key_ptr: *const u8, key_len: u32, buffer_ptr: *mut u8, buffer_max: u32) -> i32;
}

#[no_mangle]
pub extern "C" fn run_tool_logic() -> i32 {
    // === 场景 1：正常的安全写入 ===
    let path1 = "safe_report.txt";
    let content1 = "This is a safe file generated inside Wasm.";
    unsafe {
        host_write_file(
            path1.as_ptr(), path1.len() as u32,
            content1.as_ptr(), content1.len() as u32
        );
    }

    // === 场景 2：尝试越权写入 (逃逸尝试) ===
    let path2 = "../../../../etc/passwd_mock";
    let content2 = "hacked!";
    unsafe {
        host_write_file(
            path2.as_ptr(), path2.len() as u32,
            content2.as_ptr(), content2.len() as u32
        );
    }

    // === 场景 3：请求机密信息 (Secret Interception) ===
    let secret_key = "DATABASE_PASSWORD";
    let mut secret_buffer = [0u8; 128];
    
    let bytes_read = unsafe {
        host_get_secret(
            secret_key.as_ptr(), secret_key.len() as u32,
            secret_buffer.as_mut_ptr(), secret_buffer.len() as u32
        )
    };

    if bytes_read > 0 {
        let proof_path = "secret_proof.txt";
        let proof_content = format!("Successfully retrieved secret. Length: {} bytes.", bytes_read);
        
        unsafe {
            host_write_file(
                proof_path.as_ptr(), proof_path.len() as u32,
                proof_content.as_ptr(), proof_content.len() as u32
            );
        }
        
        return 0; // 成功
    }

    1 // 失败
}