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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_workspace_memory() {
        let dir = tempdir().unwrap();
        let mem = WorkspaceMemory::new(dir.path().to_str().unwrap());
        
        let empty = mem.read_memory().await.unwrap();
        assert_eq!(empty, "");
        
        mem.write_memory("Hello World").await.unwrap();
        
        let read_back = mem.read_memory().await.unwrap();
        assert_eq!(read_back, "Hello World");
    }
}
