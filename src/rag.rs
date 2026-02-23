use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::BufReader;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone)]
pub struct RagChunk {
    pub content: String,
    pub source: String,
    pub embedding: Vec<f32>,
}

pub struct VectorStore {
    model: std::sync::Mutex<TextEmbedding>,
    chunks: std::sync::RwLock<Vec<RagChunk>>,
    path: PathBuf,
}

impl VectorStore {
    pub fn new(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        // Initialize the embedding model
        // This will download the model automatically the first time it runs
        let mut opts = InitOptions::new(EmbeddingModel::AllMiniLML6V2);
        opts.show_download_progress = true;
        let model = TextEmbedding::try_new(opts)?;

        let store = Self {
            model: std::sync::Mutex::new(model),
            chunks: std::sync::RwLock::new(Vec::new()),
            path: PathBuf::from(path),
        };

        // Try to load existing data
        if let Err(e) = store.load() {
            // If file doesn't exist, that's fine, start empty.
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "Warning: Failed to load vector store from {:?}: {}",
                    store.path, e
                );
            }
        }

        Ok(store)
    }

    fn load(&self) -> std::io::Result<()> {
        if !self.path.exists() {
            return Ok(());
        }
        let file = fs::File::open(&self.path)?;
        let reader = BufReader::new(file);
        let chunks: Vec<RagChunk> = serde_json::from_reader(reader)?;

        let mut store_chunks = self.chunks.write().unwrap();
        *store_chunks = chunks;

        Ok(())
    }

    fn save(&self) -> std::io::Result<()> {
        // Ensure directory exists
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = fs::File::create(&self.path)?;
        let writer = std::io::BufWriter::new(file);
        let chunks = self.chunks.read().unwrap();
        serde_json::to_writer(writer, &*chunks)?;
        Ok(())
    }

    pub fn insert_chunk(
        &self,
        content: String,
        source: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let documents = vec![content.clone()];
        // Generate embedding
        let mut model = self.model.lock().unwrap();
        let embeddings = model.embed(documents, None)?;
        let embedding = embeddings[0].clone();

        let chunk = RagChunk {
            content,
            source,
            embedding,
        };

        {
            let mut chunks = self.chunks.write().unwrap();
            chunks.push(chunk);
        } // Drop lock before saving

        if let Err(e) = self.save() {
            eprintln!("Warning: Failed to save vector store: {}", e);
        }

        Ok(())
    }

    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, f32)>, Box<dyn std::error::Error>> {
        let query_embedding = {
            let mut model = self.model.lock().unwrap();
            let embeddings = model.embed(vec![query.to_string()], None)?;
            embeddings[0].clone()
        };

        let chunks = self.chunks.read().unwrap();

        let mut results: Vec<(String, String, f32)> = chunks
            .iter()
            .map(|chunk| {
                let distance = cosine_similarity(&chunk.embedding, &query_embedding);
                (chunk.content.clone(), chunk.source.clone(), distance)
            })
            .collect();

        // Sort by distance ascending (assuming 1.0 is identical, so we want largest, meaning we sort descending)
        results.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        results.truncate(limit);

        Ok(results)
    }
}

// Helper to calculate cosine similarity
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let mut dot_product = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;

    for i in 0..a.len() {
        dot_product += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot_product / (norm_a.sqrt() * norm_b.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rag_insert_and_search() {
        let path = "test_rag_store.json";
        if std::path::Path::new(path).exists() {
            std::fs::remove_file(path).unwrap();
        }

        let store = VectorStore::new(path).expect("Failed to init fastembed");

        store
            .insert_chunk(
                "Rust is a blazing fast systems programming language.".to_string(),
                "doc1.md".to_string(),
            )
            .unwrap();

        store
            .insert_chunk(
                "Python is a great language for data science and AI.".to_string(),
                "doc2.md".to_string(),
            )
            .unwrap();

        let results = store.search("Which language is fast?", 1).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "doc1.md");

        // Verify persistence
        drop(store);
        let store2 = VectorStore::new(path).expect("Failed to reload store");
        let results2 = store2.search("Which language is fast?", 1).unwrap();
        assert_eq!(results2.len(), 1);
        assert_eq!(results2[0].1, "doc1.md");

        // Cleanup
        std::fs::remove_file(path).unwrap();
    }
}
