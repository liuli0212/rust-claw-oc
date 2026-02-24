use fastembed::{TextEmbedding, InitOptions, EmbeddingModel};
use serde::{Deserialize, Serialize};
use rusqlite::{Connection, params};
use std::sync::Mutex;

#[derive(Serialize, Deserialize, Clone)]
pub struct RagChunk {
    pub content: String,
    pub source: String,
    pub embedding: Vec<f32>,
}

pub struct VectorStore {
    model: Mutex<TextEmbedding>,
    conn: Mutex<Connection>,
}

impl VectorStore {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let mut opts = InitOptions::new(EmbeddingModel::AllMiniLML6V2);
        opts.show_download_progress = true;
        let model = TextEmbedding::try_new(opts)?;

        // Open a SQLite connection for hybrid search
        let conn = Connection::open(".rusty_claw_memory.db")?;
        
        // Initialize the FTS5 virtual table
        conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                content,
                source UNINDEXED,
                embedding UNINDEXED
            )",
            [],
        )?;

        Ok(Self {
            model: Mutex::new(model),
            conn: Mutex::new(conn),
        })
    }

    pub fn insert_chunk(&self, content: String, source: String) -> Result<(), Box<dyn std::error::Error>> {
        let documents = vec![content.clone()];
        let mut model = self.model.lock().unwrap();
        let embeddings = model.embed(documents, None)?;
        let embedding = embeddings[0].clone();

        // Serialize the embedding vector into JSON so we can store it in the UNINDEXED column
        let embedding_json = serde_json::to_string(&embedding)?;

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO chunks_fts (content, source, embedding) VALUES (?1, ?2, ?3)",
            params![content, source, embedding_json],
        )?;

        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<(String, String, f32)>, Box<dyn std::error::Error>> {
        let query_embedding = {
            let mut model = self.model.lock().unwrap();
            let embeddings = model.embed(vec![query.to_string()], None)?;
            embeddings[0].clone()
        };

        let conn = self.conn.lock().unwrap();
        
        // Let's retrieve all chunks and perform hybrid scoring.
        // In a real production system, we'd use SQLite vec extensions or only fetch top K from FTS,
        // but for now, we combine an FTS5 full-text score with vector cosine similarity.
        
        // First, let's get BM25 scores for matching chunks, and get all other chunks to calculate vector similarity.
        let mut stmt = conn.prepare("SELECT content, source, embedding, bm25(chunks_fts) as bm25_score FROM chunks_fts")?;
        
        let mut rows = stmt.query([])?;
        let mut results = Vec::new();
        
        let mut max_bm25 = 0.0_f64;
        let mut max_vector = 0.0_f32;
        
        struct ScoredChunk {
            content: String,
            source: String,
            bm25_score: f64,
            vector_score: f32,
        }
        
        let mut scored_chunks = Vec::new();

        while let Some(row) = rows.next()? {
            let content: String = row.get(0)?;
            let source: String = row.get(1)?;
            let embedding_json: String = row.get(2)?;
            // Since we queried without MATCH, bm25_score might just be 0, wait, `bm25` requires a MATCH clause.
            // Oh, we can't use bm25() without MATCH.
            
            // So we need TWO passes or just use Rust for everything.
            // Let's do two passes to get hybrid scores.
            let chunk_embedding: Vec<f32> = serde_json::from_str(&embedding_json)?;
            let vector_score = cosine_similarity(&chunk_embedding, &query_embedding);
            
            scored_chunks.push(ScoredChunk {
                content,
                source,
                bm25_score: 0.0, // Default to 0
                vector_score,
            });
        }
        
        // Pass 2: get BM25 scores
        // We will tokenize the query manually and use MATCH.
        let fts_query = query.replace("'", "''"); // escape quotes
        let match_query = format!("'{}'", fts_query); // naive match query
        
        let mut fts_stmt = conn.prepare(&format!("SELECT content, bm25(chunks_fts) FROM chunks_fts WHERE chunks_fts MATCH ?1"))?;
        // Ignore error if match syntax is invalid (e.g. empty query)
        if let Ok(mut fts_rows) = fts_stmt.query([match_query]) {
            while let Ok(Some(row)) = fts_rows.next() {
                let content: String = row.get(0)?;
                let bm25_score: f64 = row.get(1)?;
                // the bm25() function returns a *negative* value where more negative means better match in SQLite
                // So we negate it to make it positive.
                let positive_bm25 = -bm25_score;
                
                if let Some(chunk) = scored_chunks.iter_mut().find(|c| c.content == content) {
                    chunk.bm25_score = positive_bm25;
                }
            }
        }

        // Normalize scores
        for chunk in &scored_chunks {
            if chunk.bm25_score > max_bm25 { max_bm25 = chunk.bm25_score; }
            if chunk.vector_score > max_vector { max_vector = chunk.vector_score; }
        }

        for chunk in scored_chunks {
            let mut normalized_bm25 = 0.0;
            if max_bm25 > 0.0 {
                normalized_bm25 = (chunk.bm25_score / max_bm25) as f32;
            }
            
            let mut normalized_vector = 0.0;
            if max_vector > 0.0 {
                // Vector scores are between -1 and 1
                normalized_vector = chunk.vector_score.max(0.0) / max_vector.max(0.0001);
            }

            // Alpha weights: 0.3 for full text, 0.7 for semantic vector
            let combined_score = (0.3 * normalized_bm25) + (0.7 * normalized_vector);
            results.push((chunk.content, chunk.source, combined_score));
        }

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
        // Clean up previous db
        let _ = std::fs::remove_file(".rusty_claw_memory.db");
        
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
