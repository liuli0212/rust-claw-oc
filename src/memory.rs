use std::path::PathBuf;
use tokio::fs;

pub struct WorkspaceMemory {
    memory_file_path: PathBuf,
}

impl WorkspaceMemory {
    pub fn new(workspace_dir: &str) -> Self {
        let mut path = PathBuf::from(workspace_dir);
        path.push("MEMORY.md");
        Self {
            memory_file_path: path,
        }
    }

    pub async fn read_memory(&self) -> std::io::Result<String> {
        if self.memory_file_path.exists() {
            fs::read_to_string(&self.memory_file_path).await
        } else {
            Ok(String::new())
        }
    }

    pub async fn write_memory(&self, content: &str) -> std::io::Result<()> {
        fs::write(&self.memory_file_path, content).await
    }

    pub async fn append_memory(&self, content: &str) -> std::io::Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.memory_file_path)
            .await?;

        file.write_all(content.as_bytes()).await?;
        file.write_all(b"\n").await?;
        Ok(())
    }
}
