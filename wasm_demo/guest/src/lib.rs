// 声明从 Host 导入的函数
extern "C" {
    // 请求宿主写文件
    fn host_write_file(ptr: *const u8, len: usize, content_ptr: *const u8, content_len: usize) -> i32;
    // 请求宿主机密信息
    fn host_get_secret(key_ptr: *const u8, key_len: usize, buffer_ptr: *mut u8, buffer_max: usize) -> i32;
}

#[no_mangle]
pub extern "C" fn run_tool_logic() -> i32 {
    let path = "sandbox_test.txt";
    let content = "Hello from Wasm Sandbox!";

    // 1. 尝试写文件
    unsafe {
        host_write_file(
            path.as_ptr(), path.len(),
            content.as_ptr(), content.len()
        );
    }

    // 2. 尝试获取敏感机密并进行内部逻辑处理
    let secret_key = "DATABASE_PASSWORD";
    let mut buffer = [0u8; 64];
    
    let bytes_read = unsafe {
        host_get_secret(
            secret_key.as_ptr(), secret_key.len(),
            buffer.as_mut_ptr(), buffer.len()
        )
    };

    if bytes_read > 0 {
        // 在沙盒内部处理机密（例如生成哈希或加密请求），机密不会流出沙盒
        return 0; // 成功
    }

    1 // 失败
}
