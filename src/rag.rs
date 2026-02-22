use fastembed::{TextEmbedding, InitOptions, EmbeddingModel};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct RagChunk {
    pub content: String,
    pub source: String,
    pub embedding: Vec<f32>,
}

pub struct VectorStore {
    model: std::sync::Mutex<TextEmbedding>,
    chunks: std::sync::RwLock<Vec<RagChunk>>,
}

impl VectorStore {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Initialize the embedding model
        // This will download the model automatically the first time it runs
        let mut opts = InitOptions::new(EmbeddingModel::AllMiniLML6V2);
        opts.show_download_progress = true;
        let model = TextEmbedding::try_new(opts)?;

        Ok(Self {
            model: std::sync::Mutex::new(model),
            chunks: std::sync::RwLock::new(Vec::new()),
        })
    }

    pub fn insert_chunk(&self, content: String, source: String) -> Result<(), Box<dyn std::error::Error>> {
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

        let mut chunks = self.chunks.write().unwrap();
        chunks.push(chunk);

        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<(String, String, f32)>, Box<dyn std::error::Error>> {
        let query_embedding = {
            let mut model = self.model.lock().unwrap();
            let embeddings = model.embed(vec![query.to_string()], None)?;
            embeddings[0].clone()
        };

        let chunks = self.chunks.read().unwrap();
        
        let mut results: Vec<(String, String, f32)> = chunks.iter().map(|chunk| {
            let distance = cosine_similarity(&chunk.embedding, &query_embedding);
            (chunk.content.clone(), chunk.source.clone(), distance)
        }).collect();

        // Sort by distance ascending (assuming 1.0 is identical, so we want largest, meaning we sort descending)
        // Wait, standard cosine similarity is 1.0 for same, -1.0 for opposite.
        // We want to return the largest similarity.
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
        let store = VectorStore::new().expect("Failed to init fastembed");
        
        store.insert_chunk(
            "Rust is a blazing fast systems programming language.".to_string(), 
            "doc1.md".to_string()
        ).unwrap();
        
        store.insert_chunk(
            "Python is a great language for data science and AI.".to_string(), 
            "doc2.md".to_string()
        ).unwrap();
        
        let results = store.search("Which language is fast?", 1).unwrap();
        
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1, "doc1.md");
    }
}
