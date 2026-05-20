//! Persistent storage layer.
//!
//! Two backends collaborate:
//! * **redb** is the source-of-truth K/V store for chats, messages, and
//!   settings. ACID, single writer, multi-reader.
//! * **tantivy** is a Lucene-style full-text + fuzzy search index over message
//!   bodies, kept eventually consistent with redb.
//!
//! The public façade is `Store`, which is safe to clone and pass across
//! threads (`Arc<Inner>` internally). Writes are serialised through a single
//! redb write transaction at a time as the engine enforces.

use crate::error::StorageError;
use crate::models::{Chat, Message, Settings};
use parking_lot::Mutex;
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

pub mod search;

use search::SearchIndex;

// Centralised bincode 2 helpers. We use the standard config with a fixed-int
// encoding and little-endian byte order so on-disk records are stable across
// architectures. Anything that hits redb goes through these two functions.
fn bincode_serialize<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, StorageError> {
    bincode::serde::encode_to_vec(value, bincode::config::standard())
        .map_err(|e| StorageError::Encode(e.to_string()))
}

fn bincode_deserialize<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StorageError> {
    let (value, _consumed) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())
        .map_err(|e| StorageError::Decode(e.to_string()))?;
    Ok(value)
}

// --- table layout -----------------------------------------------------------

/// Chats keyed by their `Uuid` bytes -> bincoded `Chat`.
const TBL_CHATS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chats");

/// Messages keyed by `chat_id (16) || created_at_micros_be (8) || msg_id (16)`
/// -> bincoded `Message`. The composite key gives us efficient range scans per
/// chat ordered by timestamp.
const TBL_MESSAGES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("messages");

/// Simple key/value table for app metadata (settings, version markers, etc.).
const TBL_META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

const META_SETTINGS: &str = "settings";
const META_SCHEMA_VERSION: &str = "schema_version";
const CURRENT_SCHEMA_VERSION: u32 = 1;

// --- public store -----------------------------------------------------------

#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    db: Database,
    /// Tantivy writes go through a single writer protected by a mutex; redb
    /// already serialises writes itself.
    index: Mutex<SearchIndex>,
}

impl Store {
    pub fn open(db_path: &Path, index_dir: &Path) -> Result<Self, StorageError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::create_dir_all(index_dir)?;

        let db = Database::create(db_path).map_err(redb::Error::from)?;
        let index = SearchIndex::open(index_dir)?;
        let store = Self {
            inner: Arc::new(Inner {
                db,
                index: Mutex::new(index),
            }),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StorageError> {
        let write = self.inner.db.begin_write()?;
        {
            let mut meta = write.open_table(TBL_META)?;
            // Touch the chats/messages tables so they exist before any read txn.
            write.open_table(TBL_CHATS)?;
            write.open_table(TBL_MESSAGES)?;

            let current = meta
                .get(META_SCHEMA_VERSION)?
                .and_then(|v| {
                    let bytes = v.value();
                    if bytes.len() == 4 {
                        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            if current != CURRENT_SCHEMA_VERSION {
                meta.insert(
                    META_SCHEMA_VERSION,
                    &CURRENT_SCHEMA_VERSION.to_be_bytes()[..],
                )?;
            }
        }
        write.commit()?;
        Ok(())
    }

    // ---- settings -----------------------------------------------------------

    pub fn load_settings(&self) -> Result<Settings, StorageError> {
        let read = self.inner.db.begin_read()?;
        let meta = read.open_table(TBL_META)?;
        match meta.get(META_SETTINGS)? {
            Some(v) => bincode_deserialize::<Settings>(v.value()),
            None => Ok(Settings::default()),
        }
    }

    pub fn save_settings(&self, settings: &Settings) -> Result<(), StorageError> {
        let bytes = bincode_serialize(settings)?;
        let write = self.inner.db.begin_write()?;
        {
            let mut meta = write.open_table(TBL_META)?;
            meta.insert(META_SETTINGS, bytes.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    // ---- chats --------------------------------------------------------------

    pub fn upsert_chat(&self, chat: &Chat) -> Result<(), StorageError> {
        let bytes = bincode_serialize(chat)?;
        let write = self.inner.db.begin_write()?;
        {
            let mut chats = write.open_table(TBL_CHATS)?;
            chats.insert(chat.id.as_bytes().as_slice(), bytes.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn delete_chat(&self, id: Uuid) -> Result<(), StorageError> {
        let write = self.inner.db.begin_write()?;
        {
            let mut chats = write.open_table(TBL_CHATS)?;
            chats.remove(id.as_bytes().as_slice())?;
            let mut messages = write.open_table(TBL_MESSAGES)?;
            let (lo, hi) = message_range(id);
            // redb requires the bounds outlive the call, so collect first.
            let to_delete: Vec<Vec<u8>> = messages
                .range(lo.as_slice()..=hi.as_slice())?
                .filter_map(|r| r.ok().map(|(k, _)| k.value().to_vec()))
                .collect();
            for key in to_delete {
                messages.remove(key.as_slice())?;
            }
        }
        write.commit()?;
        self.inner.index.lock().delete_chat(id)?;
        Ok(())
    }

    pub fn list_chats(&self) -> Result<Vec<Chat>, StorageError> {
        let read = self.inner.db.begin_read()?;
        let chats = read.open_table(TBL_CHATS)?;
        let mut out = Vec::new();
        for item in chats.iter()? {
            let (_, v) = item?;
            let chat: Chat = bincode_deserialize(v.value())?;
            out.push(chat);
        }
        // Newest first, pinned bubble to top.
        out.sort_by(|a, b| {
            b.pinned
                .cmp(&a.pinned)
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        Ok(out)
    }

    // ---- messages -----------------------------------------------------------

    pub fn insert_message(&self, message: &Message) -> Result<(), StorageError> {
        let bytes = bincode_serialize(message)?;
        let key = message_key(message);
        let write = self.inner.db.begin_write()?;
        {
            let mut messages = write.open_table(TBL_MESSAGES)?;
            messages.insert(key.as_slice(), bytes.as_slice())?;
        }
        write.commit()?;
        // Best-effort index. Failure to index must not block message writes.
        if let Err(e) = self.inner.index.lock().add_message(message) {
            tracing::warn!(error = %e, "failed to index message");
        }
        Ok(())
    }

    pub fn update_message(&self, message: &Message) -> Result<(), StorageError> {
        // Same key derivation since the id+chat+ts are stable.
        self.insert_message(message)
    }

    pub fn delete_message(&self, message: &Message) -> Result<(), StorageError> {
        let key = message_key(message);
        let write = self.inner.db.begin_write()?;
        {
            let mut messages = write.open_table(TBL_MESSAGES)?;
            messages.remove(key.as_slice())?;
        }
        write.commit()?;
        self.inner.index.lock().delete_message(message.id)?;
        Ok(())
    }

    pub fn list_messages(&self, chat_id: Uuid) -> Result<Vec<Message>, StorageError> {
        let read = self.inner.db.begin_read()?;
        let messages = read.open_table(TBL_MESSAGES)?;
        let (lo, hi) = message_range(chat_id);
        let mut out = Vec::new();
        for item in messages.range(lo.as_slice()..=hi.as_slice())? {
            let (_, v) = item?;
            out.push(bincode_deserialize::<Message>(v.value())?);
        }
        Ok(out)
    }

    pub fn count_messages(&self) -> Result<u64, StorageError> {
        let read = self.inner.db.begin_read()?;
        let messages = read.open_table(TBL_MESSAGES)?;
        Ok(messages.len()?)
    }

    // ---- search -------------------------------------------------------------

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<search::Hit>, StorageError> {
        self.inner.index.lock().search(query, limit)
    }

    pub fn rebuild_index(&self) -> Result<u64, StorageError> {
        let read = self.inner.db.begin_read()?;
        let table = read.open_table(TBL_MESSAGES)?;
        let mut index = self.inner.index.lock();
        index.reset()?;
        let mut count = 0u64;
        for item in table.iter()? {
            let (_, v) = item?;
            let msg: Message = bincode_deserialize(v.value())?;
            index.add_message(&msg)?;
            count += 1;
        }
        index.commit()?;
        Ok(count)
    }
}

fn message_key(msg: &Message) -> Vec<u8> {
    // 16 (chat) + 8 (ts micros big-endian) + 16 (msg) = 40 bytes.
    let mut k = Vec::with_capacity(40);
    k.extend_from_slice(msg.chat_id.as_bytes());
    let ts: i64 = msg.created_at.timestamp_micros();
    // Map signed -> unsigned so big-endian sort matches chronological order.
    let unsigned = (ts as i128 - i64::MIN as i128) as u64;
    k.extend_from_slice(&unsigned.to_be_bytes());
    k.extend_from_slice(msg.id.as_bytes());
    k
}

fn message_range(chat_id: Uuid) -> (Vec<u8>, Vec<u8>) {
    // Build a 40-byte lo/hi spanning every possible key with prefix
    // `chat_id`. Keys are `chat_id (16) || ts_be (8) || msg_id (16)`, all
    // 40 bytes total, so:
    //   lo = chat_id || 0x00 * 24    (smallest key with this prefix)
    //   hi = chat_id || 0xFF * 24    (largest key with this prefix)
    // Callers use the inclusive range `lo..=hi`, which closes the previous
    // bug where chat_id-of-all-0xFF overflowed the carry algorithm and
    // produced a backwards exclusive range that scanned nothing.
    let mut lo = Vec::with_capacity(40);
    lo.extend_from_slice(chat_id.as_bytes());
    lo.resize(40, 0x00);
    let mut hi = Vec::with_capacity(40);
    hi.extend_from_slice(chat_id.as_bytes());
    hi.resize(40, 0xff);
    (lo, hi)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::models::{Chat, Message, Role, Settings};
    use tempfile::tempdir;

    fn open_store(dir: &std::path::Path) -> Store {
        Store::open(&dir.join("db.redb"), &dir.join("index")).expect("open")
    }

    #[test]
    fn settings_roundtrip() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let mut s = Settings::default();
        s.temperature = 1.25;
        s.font_size = 17.5;
        store.save_settings(&s).unwrap();
        let loaded = store.load_settings().unwrap();
        assert!((loaded.temperature - 1.25).abs() < 1e-6);
        assert!((loaded.font_size - 17.5).abs() < 1e-6);
    }

    #[test]
    fn chat_and_messages_persist_and_order() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let chat = Chat::new("xai", "grok-beta");
        store.upsert_chat(&chat).unwrap();

        let mut m1 = Message::new(chat.id, Role::User, "first");
        m1.created_at = chrono::Utc::now() - chrono::Duration::seconds(2);
        let m2 = Message::new(chat.id, Role::Assistant, "second");
        store.insert_message(&m1).unwrap();
        store.insert_message(&m2).unwrap();

        let listed = store.list_messages(chat.id).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed[0].created_at < listed[1].created_at);
        assert_eq!(listed[0].content, "first");
        assert_eq!(listed[1].content, "second");
    }

    #[test]
    fn delete_chat_removes_messages() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let chat = Chat::new("xai", "grok-beta");
        store.upsert_chat(&chat).unwrap();
        for i in 0..5 {
            let m = Message::new(chat.id, Role::User, format!("msg {i}"));
            store.insert_message(&m).unwrap();
        }
        assert_eq!(store.list_messages(chat.id).unwrap().len(), 5);
        store.delete_chat(chat.id).unwrap();
        assert!(store.list_messages(chat.id).unwrap().is_empty());
    }

    #[test]
    fn delete_message_drops_it_from_history_and_index() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let chat = Chat::new("xai", "grok-beta");
        store.upsert_chat(&chat).unwrap();
        let m = Message::new(chat.id, Role::User, "ephemeral note");
        store.insert_message(&m).unwrap();
        assert_eq!(store.list_messages(chat.id).unwrap().len(), 1);
        assert!(!store.search("ephemeral", 5).unwrap().is_empty());

        store.delete_message(&m).unwrap();
        assert!(store.list_messages(chat.id).unwrap().is_empty());
        assert!(
            store.search("ephemeral", 5).unwrap().is_empty(),
            "deleted message must vanish from the search index"
        );
    }

    #[test]
    fn full_text_search_finds_keyword() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let chat = Chat::new("xai", "grok-beta");
        store.upsert_chat(&chat).unwrap();
        let m = Message::new(
            chat.id,
            Role::User,
            "the rusted bicycle squeaks beautifully",
        );
        store.insert_message(&m).unwrap();
        let hits = store.search("bicycle", 5).unwrap();
        assert!(!hits.is_empty(), "expected at least one hit for 'bicycle'");
        assert_eq!(hits[0].chat_id, chat.id);
    }
}
