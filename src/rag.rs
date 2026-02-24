use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, RwLock};
use std::time::Instant;

#[derive(Serialize, Deserialize, Clone)]
pub struct RagChunk {
    pub id: i64,
    pub content: String,
    pub source: String,
    pub embedding: Vec<f32>,
}

pub struct VectorStore {
    // We use a Mutex for the embedding model because it's not thread-safe or we want to serialize calls
    model: Mutex<TextEmbedding>,
    // SQLite connection for persistence and FTS5 search
    conn: Mutex<Connection>,
    // In-memory cache of all chunks for fast vector scanning
    // RwLock allows multiple readers (searches) at once
    chunks: RwLock<Vec<RagChunk>>,
}

impl VectorStore {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let start = Instant::now();
        println!("[RAG] Initializing VectorStore...");

        let mut opts = InitOptions::new(EmbeddingModel::AllMiniLML6V2);
        opts.show_download_progress = true;
        let model = TextEmbedding::try_new(opts)?;

        // Open a SQLite connection for hybrid search
        let conn = Connection::open(".rusty_claw_memory.db")?;

        // Initialize the tables
        // 1. chunks: The source of truth
        conn.execute(
            "CREATE TABLE IF NOT EXISTS chunks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                content TEXT NOT NULL,
                source TEXT NOT NULL,
                embedding_json TEXT NOT NULL
            )",
            [],
        )?;

        // 2. chunks_fts: The keyword search index (external content table)
        conn.execute(
            "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
                content,
                content_rowid UNINDEXED
            )",
            [],
        )?;

        // 3. Triggers to keep FTS in sync
        conn.execute(
            "CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
              INSERT INTO chunks_fts(content, content_rowid) VALUES (new.content, new.id);
            END;",
            [],
        )?;
        conn.execute(
            "CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
              DELETE FROM chunks_fts WHERE content_rowid = old.id;
            END;",
            [],
        )?;

        // Load all chunks into memory
        let mut loaded_chunks = Vec::new();
        let mut stmt = conn.prepare("SELECT id, content, source, embedding_json FROM chunks")?;
        let rows = stmt.query_map([], |row| {
            let id: i64 = row.get(0)?;
            let content: String = row.get(1)?;
            let source: String = row.get(2)?;
            let embedding_json: String = row.get(3)?;
            let embedding: Vec<f32> = serde_json::from_str(&embedding_json).unwrap_or_default();
            Ok(RagChunk {
                id,
                content,
                source,
                embedding,
            })
        })?;

        for chunk in rows {
            loaded_chunks.push(chunk?);
        }

        // Drop the statement to release the borrow on conn
        drop(stmt);

        println!(
            "[RAG] Loaded {} chunks into memory in {}ms",
            loaded_chunks.len(),
            start.elapsed().as_millis()
        );

        Ok(Self {
            model: Mutex::new(model),
            conn: Mutex::new(conn),
            chunks: RwLock::new(loaded_chunks),
        })
    }

    pub fn insert_chunk(
        &self,
        content: String,
        source: String,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let documents = vec![content.clone()];

        // Generate embedding
        let embedding = {
            let mut model = self.model.lock().unwrap();
            let embeddings = model.embed(documents, None)?;
            embeddings[0].clone()
        };

        let embedding_json = serde_json::to_string(&embedding)?;

        // Write to DB
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO chunks (content, source, embedding_json) VALUES (?1, ?2, ?3)",
            params![content, source, embedding_json],
        )?;
        let id = conn.last_insert_rowid();

        // Update in-memory cache
        let mut chunks = self.chunks.write().unwrap();
        chunks.push(RagChunk {
            id,
            content,
            source,
            embedding,
        });

        Ok(())
    }

    pub fn search(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, f32)>, Box<dyn std::error::Error>> {
        let _start = Instant::now();

        // 1. Generate query embedding
        let query_embedding = {
            let mut model = self.model.lock().unwrap();
            let embeddings = model.embed(vec![query.to_string()], None)?;
            embeddings[0].clone()
        };

        // 2. Keyword Search (FTS5)
        // Lock ordering rule: always conn -> chunks to avoid deadlocks with insert_chunk.
        let fts_scores: std::collections::HashMap<i64, f64> = {
            let conn = self.conn.lock().unwrap();
            // Naive query sanitization
            let safe_query = query.replace("\"", "").replace("'", "");
            let fts_query = format!("\"{}\"", safe_query);
            let mut scores = std::collections::HashMap::new();
            let mut stmt = conn.prepare(
                "SELECT content_rowid, bm25(chunks_fts) FROM chunks_fts WHERE chunks_fts MATCH ?1 ORDER BY bm25(chunks_fts) LIMIT 50"
            )?;
            let fts_iter = stmt.query_map(params![fts_query], |row| {
                let id: i64 = row.get(0)?;
                let score: f64 = row.get(1)?;
                Ok((id, score))
            });

            if let Ok(iter) = fts_iter {
                for item in iter {
                    if let Ok((id, score)) = item {
                        // SQLite BM25 is negative (more negative = better). We invert it.
                        scores.insert(id, score.abs());
                    }
                }
            }
            scores
        };

        // 3. Vector Search (In-Memory Scan)
        // Since we likely have < 50k chunks, a brute-force cosine similarity scan is very fast.
        let chunks = self.chunks.read().unwrap();
        let mut scored_chunks: Vec<(usize, f32)> = chunks
            .iter()
            .enumerate()
            .map(|(idx, chunk)| {
                let score = cosine_similarity(&chunk.embedding, &query_embedding);
                (idx, score)
            })
            .collect();

        // Sort by vector score descending (preliminary top K for vector)
        scored_chunks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 4. Hybrid Scoring & Re-ranking
        let mut results = Vec::new();
        let vector_weight = 0.7;
        let keyword_weight = 0.3;

        // Find max scores for normalization
        let max_vector_score = scored_chunks.first().map(|s| s.1).unwrap_or(1.0).max(0.001);
        let max_keyword_score = fts_scores
            .values()
            .fold(0.0f64, |a, &b| a.max(b))
            .max(0.001);

        // Consider candidates: Top 50 from vector + Top 50 from keywords (union)
        // Optimization: Just re-rank the top 100 vector results + any FTS hits
        for (idx, v_score) in scored_chunks.iter().take(100).copied() {
            let chunk = &chunks[idx];
            let normalized_v = v_score.max(0.0) / max_vector_score;
            let k_score = fts_scores.get(&chunk.id).copied().unwrap_or(0.0);
            let normalized_k = k_score / max_keyword_score;

            let final_score =
                (normalized_v * vector_weight) + (normalized_k as f32 * keyword_weight);
            results.push((chunk.content.clone(), chunk.source.clone(), final_score));
        }

        // Sort by final score
        results.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);

        // if results.len() > 0 {
        //     println!("[RAG] Search finished in {}ms. Top score: {}", start.elapsed().as_millis(), results[0].2);
        // }

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
    fn test_rag_hybrid_search() {
        let _ = std::fs::remove_file(".rusty_claw_memory.db");
        let store = VectorStore::new().expect("Failed to init");

        store
            .insert_chunk("Rust is fast.".to_string(), "doc1".to_string())
            .unwrap();
        store
            .insert_chunk("Python is easy.".to_string(), "doc2".to_string())
            .unwrap();
        store
            .insert_chunk("The rusty claw grabs.".to_string(), "doc3".to_string())
            .unwrap();

        // 1. Vector match (semantic)
        let res = store.search("speed performance", 1).unwrap();
        assert_eq!(res[0].1, "doc1");

        // 2. Keyword match (exact)
        let res2 = store.search("rusty claw", 1).unwrap();
        assert_eq!(res2[0].1, "doc3");
    }
}
