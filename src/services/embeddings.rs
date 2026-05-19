#![allow(dead_code)]
//! Local embeddings + RAG retrieval over chat history.
//!
//! When the `rag` feature is enabled this module uses `fastembed` to compute
//! sentence embeddings for the BAAI/bge-small-en-v1.5 model bundled at first
//! run. Without the feature the module exposes the same surface area but
//! retrieval falls back to lexical (BM25) hits from the search index — so
//! callers don't need to special-case the feature flag.

use crate::error::StorageError;
use crate::storage::Store;

pub struct Retriever {
    store: Store,
    #[cfg(feature = "rag")]
    model: parking_lot::Mutex<Option<fastembed::TextEmbedding>>,
}

#[derive(Debug, Clone)]
pub struct Retrieved {
    pub msg_id: uuid::Uuid,
    pub chat_id: uuid::Uuid,
    pub snippet: String,
    pub score: f32,
}

impl Retriever {
    pub fn new(store: Store) -> Self {
        Self {
            store,
            #[cfg(feature = "rag")]
            model: parking_lot::Mutex::new(None),
        }
    }

    /// Find up to `top_k` messages most relevant to `query` across all chats.
    pub fn retrieve(&self, query: &str, top_k: usize) -> Result<Vec<Retrieved>, StorageError> {
        // Always run a lexical query first; semantic search re-ranks the top
        // candidates rather than scanning every message.
        let hits = self.store.search(query, top_k.max(20))?;
        #[cfg(not(feature = "rag"))]
        {
            Ok(hits
                .into_iter()
                .take(top_k)
                .map(|h| Retrieved {
                    msg_id: h.msg_id,
                    chat_id: h.chat_id,
                    snippet: h.snippet,
                    score: h.score,
                })
                .collect())
        }
        #[cfg(feature = "rag")]
        {
            self.semantic_rerank(query, hits, top_k)
        }
    }

    #[cfg(feature = "rag")]
    fn semantic_rerank(
        &self,
        query: &str,
        hits: Vec<crate::storage::search::Hit>,
        top_k: usize,
    ) -> Result<Vec<Retrieved>, StorageError> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

        let mut guard = self.model.lock();
        if guard.is_none() {
            let model = TextEmbedding::try_new(
                InitOptions::new(EmbeddingModel::BGESmallENV15).with_show_download_progress(false),
            )
            .map_err(|e| StorageError::Index(e.to_string()))?;
            *guard = Some(model);
        }
        let model = guard.as_mut().expect("model loaded above");

        let mut docs: Vec<String> = Vec::with_capacity(hits.len() + 1);
        docs.push(query.to_string());
        docs.extend(hits.iter().map(|h| h.snippet.clone()));

        let embeddings = model
            .embed(docs, None)
            .map_err(|e| StorageError::Index(e.to_string()))?;
        let q = &embeddings[0];
        let mut scored: Vec<(f32, &crate::storage::search::Hit)> = embeddings[1..]
            .iter()
            .zip(hits.iter())
            .map(|(e, h)| (cosine(q, e), h))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored
            .into_iter()
            .take(top_k)
            .map(|(score, h)| Retrieved {
                msg_id: h.msg_id,
                chat_id: h.chat_id,
                snippet: h.snippet.clone(),
                score,
            })
            .collect())
    }
}

#[cfg(feature = "rag")]
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}
