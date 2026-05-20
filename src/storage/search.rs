//! Tantivy-backed full-text search over chat history.
//!
//! Schema: `msg_id` (string, stored), `chat_id` (string, stored), `body`
//! (text, indexed+stored), `created_at` (i64, stored). Matches return hits
//! containing the redb keys needed to resolve the underlying `Message`.

use crate::error::StorageError;
use crate::models::Message;
use std::path::Path;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Field, Schema, INDEXED, STORED, STRING, TEXT};
use tantivy::{doc, Index, IndexReader, IndexWriter, ReloadPolicy, TantivyDocument, Term};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Hit {
    pub msg_id: Uuid,
    pub chat_id: Uuid,
    pub score: f32,
    pub snippet: String,
}

pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    writer: IndexWriter,
    fields: Fields,
}

#[derive(Clone, Copy)]
struct Fields {
    msg_id: Field,
    chat_id: Field,
    body: Field,
    created_at: Field,
}

const WRITER_HEAP: usize = 64_000_000;

impl SearchIndex {
    pub fn open(dir: &Path) -> Result<Self, StorageError> {
        let mut builder = Schema::builder();
        let msg_id = builder.add_text_field("msg_id", STRING | STORED);
        let chat_id = builder.add_text_field("chat_id", STRING | STORED);
        let body = builder.add_text_field("body", TEXT | STORED);
        let created_at = builder.add_i64_field("created_at", INDEXED | STORED);
        let schema = builder.build();

        let dir = tantivy::directory::MmapDirectory::open(dir)?;
        let index = Index::open_or_create(dir, schema.clone())?;
        let writer: IndexWriter = index.writer(WRITER_HEAP)?;
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            writer,
            fields: Fields {
                msg_id,
                chat_id,
                body,
                created_at,
            },
        })
    }

    pub fn add_message(&mut self, message: &Message) -> Result<(), StorageError> {
        // Delete any prior document for this id before adding the new one so
        // edits don't produce duplicates.
        let term = Term::from_field_text(self.fields.msg_id, &message.id.to_string());
        self.writer.delete_term(term);
        self.writer.add_document(doc!(
            self.fields.msg_id => message.id.to_string(),
            self.fields.chat_id => message.chat_id.to_string(),
            self.fields.body => message.content.clone(),
            self.fields.created_at => message.created_at.timestamp_micros(),
        ))?;
        // Commit lazily; callers can force-flush via `commit`.
        self.writer.commit()?;
        Ok(())
    }

    pub fn delete_message(&mut self, msg_id: Uuid) -> Result<(), StorageError> {
        let term = Term::from_field_text(self.fields.msg_id, &msg_id.to_string());
        self.writer.delete_term(term);
        self.writer.commit()?;
        Ok(())
    }

    pub fn delete_chat(&mut self, chat_id: Uuid) -> Result<(), StorageError> {
        let term = Term::from_field_text(self.fields.chat_id, &chat_id.to_string());
        self.writer.delete_term(term);
        self.writer.commit()?;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<(), StorageError> {
        self.writer.commit()?;
        Ok(())
    }

    pub fn reset(&mut self) -> Result<(), StorageError> {
        self.writer.delete_all_documents()?;
        self.writer.commit()?;
        Ok(())
    }

    pub fn search(&self, q: &str, limit: usize) -> Result<Vec<Hit>, StorageError> {
        if q.trim().is_empty() {
            return Ok(Vec::new());
        }
        // Make sure the reader sees the latest committed segments. With the
        // default `OnCommitWithDelay` policy, recent writes can be invisible
        // to a reader created moments ago.
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        let parser = QueryParser::for_index(&self.index, vec![self.fields.body]);
        let query = parser
            .parse_query(q)
            .map_err(|e| StorageError::Index(e.to_string()))?;
        let top = searcher.search(&query, &TopDocs::with_limit(limit))?;

        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let msg_id = first_text(&doc, self.fields.msg_id)
                .and_then(|s| Uuid::parse_str(&s).ok())
                .unwrap_or_default();
            let chat_id = first_text(&doc, self.fields.chat_id)
                .and_then(|s| Uuid::parse_str(&s).ok())
                .unwrap_or_default();
            let body = first_text(&doc, self.fields.body).unwrap_or_default();
            hits.push(Hit {
                msg_id,
                chat_id,
                score,
                snippet: snippet(&body, q, 160),
            });
        }
        Ok(hits)
    }
}

fn first_text(doc: &TantivyDocument, field: Field) -> Option<String> {
    use tantivy::schema::OwnedValue;
    for value in doc.get_all(field) {
        if let OwnedValue::Str(s) = value {
            return Some(s.to_string());
        }
    }
    None
}

/// Build a short snippet around the first match, falling back to a prefix.
fn snippet(body: &str, q: &str, max: usize) -> String {
    let lc_body = body.to_lowercase();
    let lc_q = q.to_lowercase();
    let trimmed = q.trim();
    let needle = trimmed.split_whitespace().next().unwrap_or(trimmed);
    let needle_lc = needle.to_lowercase();
    if !needle_lc.is_empty() {
        if let Some(pos_in_lc) = lc_body.find(&needle_lc).or_else(|| lc_body.find(&lc_q)) {
            // `pos_in_lc` is a byte offset into the *lowercased* body. For
            // some locales (e.g. Turkish "İ" → "i̇", German "ß" → "ss")
            // lowercasing changes byte length, so this offset may land on a
            // non-char-boundary in the original `body` — or worse, point
            // past a different character than the match. The original
            // implementation sliced `body[..pos]` directly, which would
            // panic on those inputs.
            //
            // Map back by char-count: the matched byte offset in `lc_body`
            // corresponds to some char index there; we can't always recover
            // the exact byte position in `body`, but for the snippet
            // rendering an approximate position is fine. Use ceil/floor to
            // a real `body` char boundary so the slice never panics.
            let char_count = lc_body[..pos_in_lc].chars().count();
            let approx = body
                .char_indices()
                .nth(char_count)
                .map(|(i, _)| i)
                .unwrap_or(body.len());
            let pos = floor_char_boundary(body, approx);
            let start = body[..pos]
                .char_indices()
                .rev()
                .nth(40)
                .map(|(i, _)| i)
                .unwrap_or(0);
            let end = body[pos..]
                .char_indices()
                .nth(max)
                .map(|(i, _)| pos + i)
                .unwrap_or(body.len());
            let mut s = String::new();
            if start > 0 {
                s.push_str("… ");
            }
            s.push_str(&body[start..end]);
            if end < body.len() {
                s.push_str(" …");
            }
            return s;
        }
    }
    body.chars().take(max).collect()
}

/// Find the largest `i <= idx` such that `s.is_char_boundary(i)`. Slicing
/// `s[..i]` is then guaranteed to be panic-free regardless of how `idx`
/// was derived. `std::str::floor_char_boundary` exists as a nightly API;
/// this is the stable in-house equivalent.
fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}
