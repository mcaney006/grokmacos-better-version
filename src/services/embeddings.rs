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

        // Load-or-keep the model under the existing mutex guard, then
        // borrow it once for the `embed` call. The previous form
        // unwrapped `guard.as_mut()` after the load via
        // `.expect("model loaded above")` — proven safe but the
        // expect line lands inside a `cfg(feature = "rag")` branch
        // and a future contributor adding an early-return between the
        // load and the borrow would silently turn it into a panic.
        // Match the Option directly so the type system enforces what
        // the comment was promising.
        let mut guard = self.model.lock();
        let model = match guard.as_mut() {
            Some(m) => m,
            None => {
                let m = TextEmbedding::try_new(
                    InitOptions::new(EmbeddingModel::BGESmallENV15)
                        .with_show_download_progress(false),
                )
                .map_err(|e| StorageError::Index(e.to_string()))?;
                *guard = Some(m);
                // `guard.as_mut()` cannot return None on the line after
                // we wrote Some(_); if it does, that's a real bug in
                // parking_lot we'd want to surface as a panic. Keeping
                // it in a single Option-typed `match` so any future
                // refactor that adds a fallible operation between the
                // store and the read fails the compiler instead of
                // panicking at runtime.
                guard.as_mut().ok_or_else(|| {
                    StorageError::Index("embedding model dropped immediately after load".to_owned())
                })?
            }
        };

        let mut docs: Vec<String> = Vec::with_capacity(hits.len() + 1);
        docs.push(query.to_string());
        docs.extend(hits.iter().map(|h| h.snippet.clone()));

        let embeddings = model
            .embed(docs, None)
            .map_err(|e| StorageError::Index(e.to_string()))?;
        // `embed` is contractually 1-output-per-input. We just pushed
        // the query plus N hits, so `embeddings.first()` ALWAYS exists.
        // Pattern-matched defensively so a future fastembed bump that
        // changes the contract surfaces as Err, not as panic from
        // `embeddings[0]` indexing.
        let (q, hit_embeddings) = embeddings.split_first().ok_or_else(|| {
            StorageError::Index("embedding backend returned zero vectors".to_owned())
        })?;
        let mut scored: Vec<(f32, &crate::storage::search::Hit)> = hit_embeddings
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
