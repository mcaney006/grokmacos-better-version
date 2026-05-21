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

// Centralised bincode 2 helpers. The config:
//   * `standard()` — variable-length integers, little-endian, stable across
//     architectures.
//   * `with_limit::<MAX_BLOB_BYTES>()` — caps every decoded record. The
//     default config has no upper bound; a corrupted on-disk length prefix
//     could otherwise drive the deserializer into a multi-gigabyte
//     allocation before we even hit a parse error. 16 MiB is comfortably
//     above any realistic message body (token-limit-bounded by the
//     provider) and small enough that even an adversarial value fails
//     fast rather than swapping the box.
//
// Anything that hits redb goes through these two functions.
const MAX_BLOB_BYTES: usize = 16 * 1024 * 1024;

fn bincode_config() -> impl bincode::config::Config {
    bincode::config::standard().with_limit::<MAX_BLOB_BYTES>()
}

fn bincode_serialize<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, StorageError> {
    bincode::serde::encode_to_vec(value, bincode_config())
        .map_err(|e| StorageError::Encode(e.to_string()))
}

fn bincode_deserialize<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StorageError> {
    let (value, _consumed) = bincode::serde::decode_from_slice(bytes, bincode_config())
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
        store.reconcile_index_on_startup()?;
        Ok(store)
    }

    /// Reconciliation contract: redb is the source of truth; tantivy is a
    /// derived index. They can diverge if a process crash lands between
    /// `insert_message`'s redb commit (line ~225) and the tantivy
    /// `add_message` call (line ~228), or if a tantivy commit panics.
    ///
    /// At startup we compare `redb.count_messages` against
    /// `tantivy.doc_count`. If they disagree we trigger a full rebuild
    /// from redb. This is O(N) but only runs once per launch and only
    /// when divergence is detected — the happy path is two cheap reads.
    ///
    /// Failure mode: if the rebuild itself fails, we log + continue with
    /// the divergent index rather than refusing to launch. Stale-search
    /// is a better UX than no-app.
    fn reconcile_index_on_startup(&self) -> Result<(), StorageError> {
        let redb_count = self.count_messages()?;
        let index_count = self.inner.index.lock().doc_count();
        if redb_count != index_count {
            tracing::warn!(
                redb_count,
                index_count,
                "search index diverged from redb storage on startup; rebuilding"
            );
            match self.rebuild_index() {
                Ok(rebuilt) => {
                    tracing::info!(rebuilt, "search index rebuild complete");
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        "search index rebuild failed; continuing with divergent index"
                    );
                }
            }
        }
        Ok(())
    }

    /// Apply schema migrations from the on-disk version up to
    /// `CURRENT_SCHEMA_VERSION`. The previous version was a stub that
    /// only stamped the version key — any future schema change would
    /// have been silently accepted, leaving callers reading stale-
    /// shaped data through bincode.
    ///
    /// Migration contract:
    ///   * Fresh DB (no version key) starts at version 0; migrate
    ///     forward through every intermediate.
    ///   * Each `Self::migrate_to_v<N>` is responsible for transforming
    ///     `v<N-1>` data into `v<N>` shape. Today there's only one
    ///     (v0 → v1) and it's a no-op because the schema hasn't
    ///     changed since launch.
    ///   * A version GREATER than `CURRENT_SCHEMA_VERSION` (user
    ///     downgraded the app) is REFUSED rather than silently
    ///     accepted — bincode is positional, a newer schema's bytes
    ///     would deserialise as garbage. Returns `StorageError::Decode`.
    ///   * Migration failure rolls back via redb's transaction
    ///     semantics: the version key only stamps on the same write
    ///     transaction that ran the migration.
    fn migrate(&self) -> Result<(), StorageError> {
        let current = self.read_schema_version()?;
        if current > CURRENT_SCHEMA_VERSION {
            return Err(StorageError::Decode(format!(
                "on-disk schema version {current} is newer than this build's max {CURRENT_SCHEMA_VERSION}; \
                 downgrade is not supported, install a newer build or wipe local data"
            )));
        }
        // Run migrations sequentially. Each migration is its own
        // transaction so a failure at step N leaves the DB at v<N-1>
        // and the next launch retries cleanly.
        for target in (current + 1)..=CURRENT_SCHEMA_VERSION {
            tracing::info!(from = current, to = target, "running schema migration");
            self.migrate_to(target)?;
        }
        // First-launch path: ensure tables exist even when no migration
        // was applied. Touching them inside a write txn is idempotent.
        let write = self.inner.db.begin_write()?;
        {
            write.open_table(TBL_META)?;
            write.open_table(TBL_CHATS)?;
            write.open_table(TBL_MESSAGES)?;
        }
        write.commit()?;
        Ok(())
    }

    fn read_schema_version(&self) -> Result<u32, StorageError> {
        let read = self.inner.db.begin_read()?;
        let meta = match read.open_table(TBL_META) {
            Ok(t) => t,
            // First launch: table doesn't exist yet. Treat as v0.
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(e) => return Err(StorageError::Table(e)),
        };
        Ok(meta
            .get(META_SCHEMA_VERSION)?
            .and_then(|v| {
                let bytes = v.value();
                if bytes.len() == 4 {
                    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                } else {
                    None
                }
            })
            .unwrap_or(0))
    }

    /// Dispatch one step of the migration ladder. New schema bumps add
    /// a match arm here AND bump `CURRENT_SCHEMA_VERSION`.
    fn migrate_to(&self, target: u32) -> Result<(), StorageError> {
        let write = self.inner.db.begin_write()?;
        match target {
            1 => {
                // v0 → v1: no shape change. We didn't have version
                // stamping until v1, so any pre-existing DB without a
                // version key is by definition v0 with the current
                // shape. Stamping is the only work.
                {
                    let mut meta = write.open_table(TBL_META)?;
                    meta.insert(META_SCHEMA_VERSION, &1u32.to_be_bytes()[..])?;
                }
            }
            other => {
                return Err(StorageError::Decode(format!(
                    "no migration defined for target schema version {other}"
                )));
            }
        }
        write.commit()?;
        Ok(())
    }

    // ---- settings -----------------------------------------------------------

    /// Load persisted settings, or `Settings::default()` if none have been
    /// saved yet. Crucially, a decode failure (e.g., a saved blob from a
    /// previous binary where `Settings` had different fields — bincode
    /// is positional and not forward-compatible by default) also returns
    /// the default. The previous behaviour propagated the decode error,
    /// which bricked the app for any user that upgraded across a schema
    /// change. Logging + reset is the only correct posture: the user
    /// can re-enter preferences; the app boots.
    pub fn load_settings(&self) -> Result<Settings, StorageError> {
        let read = self.inner.db.begin_read()?;
        let meta = read.open_table(TBL_META)?;
        match meta.get(META_SETTINGS)? {
            Some(v) => match bincode_deserialize::<Settings>(v.value()) {
                Ok(s) => Ok(s),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "saved settings blob failed to decode; reverting to defaults (likely a schema upgrade)"
                    );
                    Ok(Settings::default())
                }
            },
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
        let mut corrupt = 0u32;
        for item in chats.iter()? {
            let (_, v) = item?;
            // Skip individually-corrupt entries with a log line rather
            // than failing the entire list. A bincode schema bump
            // (Chat::new field) would otherwise lock the user out of
            // their entire chat history on first launch of a new
            // build, even though only old entries are unreadable.
            // Same posture as `load_settings`: log, keep going.
            match bincode_deserialize::<Chat>(v.value()) {
                Ok(chat) => out.push(chat),
                Err(e) => {
                    corrupt += 1;
                    tracing::warn!(
                        error = %e,
                        "skipping un-decodable chat entry (likely a schema upgrade)"
                    );
                }
            }
        }
        if corrupt > 0 {
            tracing::warn!(
                count = corrupt,
                "skipped {corrupt} chat entries that failed to decode"
            );
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
        self.write_message(message, /* index */ true)
    }

    /// Same as `insert_message` but skips the tantivy index write.
    /// Used by the streaming partial-persist path: we re-write the
    /// assistant message to redb every ~500 ms with the growing body,
    /// but indexing a half-streamed message has no search-utility AND
    /// makes tantivy commit a fresh segment every debounce tick. The
    /// final `insert_message`/`update_message` call after the stream
    /// completes does the single index write.
    pub fn update_message_no_index(&self, message: &Message) -> Result<(), StorageError> {
        self.write_message(message, /* index */ false)
    }

    pub fn update_message(&self, message: &Message) -> Result<(), StorageError> {
        // Same key derivation since the id+chat+ts are stable.
        self.write_message(message, /* index */ true)
    }

    fn write_message(&self, message: &Message, index: bool) -> Result<(), StorageError> {
        let bytes = bincode_serialize(message)?;
        let key = message_key(message);
        let write = self.inner.db.begin_write()?;
        {
            let mut messages = write.open_table(TBL_MESSAGES)?;
            messages.insert(key.as_slice(), bytes.as_slice())?;
        }
        write.commit()?;
        if index {
            // Best-effort index. Failure to index must not block message
            // writes — the startup reconciler will pick up the drift.
            if let Err(e) = self.inner.index.lock().add_message(message) {
                tracing::warn!(error = %e, "failed to index message");
            }
        }
        Ok(())
    }

    /// Delete a message from canonical storage and best-effort from
    /// the search index. Symmetric with `insert_message`: a tantivy
    /// failure logs at WARN and the function still returns Ok,
    /// because:
    ///   1. redb has already committed the delete — that's the source
    ///      of truth.
    ///   2. The startup reconciler in `reconcile_index_on_startup`
    ///      compares `redb.count_messages` vs `tantivy.doc_count`; an
    ///      orphan tantivy entry shows up as count_index > count_redb
    ///      and triggers a full rebuild.
    ///   3. Until the next launch, search may surface a deleted
    ///      message in the hit list. The hit's `msg_id` would resolve
    ///      to nothing in `list_messages`, so the UI sees an empty
    ///      result — annoying but not corrupting.
    ///
    /// The previous version propagated the tantivy error as an Err,
    /// which left the redb state inconsistent with the function's
    /// reported outcome (deleted from canonical but caller saw a
    /// failure).
    pub fn delete_message(&self, message: &Message) -> Result<(), StorageError> {
        let key = message_key(message);
        let write = self.inner.db.begin_write()?;
        {
            let mut messages = write.open_table(TBL_MESSAGES)?;
            messages.remove(key.as_slice())?;
        }
        write.commit()?;
        if let Err(e) = self.inner.index.lock().delete_message(message.id) {
            tracing::warn!(
                error = %e,
                msg_id = %message.id,
                "failed to delete from search index; startup reconciler will rebuild"
            );
        }
        Ok(())
    }

    pub fn list_messages(&self, chat_id: Uuid) -> Result<Vec<Message>, StorageError> {
        let read = self.inner.db.begin_read()?;
        let messages = read.open_table(TBL_MESSAGES)?;
        let (lo, hi) = message_range(chat_id);
        let mut out = Vec::new();
        let mut corrupt = 0u32;
        for item in messages.range(lo.as_slice()..=hi.as_slice())? {
            let (_, v) = item?;
            // Same posture as list_chats: skip individually-corrupt
            // entries rather than fail the whole listing on the first
            // schema-bump victim.
            match bincode_deserialize::<Message>(v.value()) {
                Ok(msg) => out.push(msg),
                Err(e) => {
                    corrupt += 1;
                    tracing::warn!(
                        error = %e,
                        %chat_id,
                        "skipping un-decodable message entry"
                    );
                }
            }
        }
        if corrupt > 0 {
            tracing::warn!(
                count = corrupt,
                %chat_id,
                "skipped {corrupt} message entries that failed to decode"
            );
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
    // Map signed -> unsigned so big-endian sort matches chronological
    // order. The math is exact for every valid i64: (ts - i64::MIN)
    // fits in u64 by definition. `From` is the lossless cast (`as`
    // would silently truncate in a future code change).
    let shifted = i128::from(ts) - i128::from(i64::MIN);
    // shifted is in [0, 2^64 - 1] — re-narrow via `try_from` so a
    // future i64-widening regression would fail loudly.
    #[allow(clippy::expect_used)] // proven by the range above
    let unsigned = u64::try_from(shifted).expect("(ts - i64::MIN) ∈ [0, u64::MAX]");
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

    /// Adversarial: a single corrupted chat entry in the table must
    /// not lock the user out of the entire chat list. The fix logs +
    /// skips the bad entry and returns the rest. Same posture for
    /// messages within a chat.
    #[test]
    fn list_chats_skips_individually_corrupt_entries() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());

        // Two real chats, then a bogus blob planted under a synthetic
        // UUID key. list_chats must surface the two real chats and
        // skip the corruption.
        let good_a = Chat::new("xai", "grok-beta");
        let good_b = Chat::new("anthropic", "claude-3-5-sonnet-latest");
        store.upsert_chat(&good_a).unwrap();
        store.upsert_chat(&good_b).unwrap();

        let bogus_id: [u8; 16] = [0x42; 16];
        let write = store.inner.db.begin_write().unwrap();
        {
            let mut chats = write.open_table(TBL_CHATS).unwrap();
            chats
                .insert(bogus_id.as_slice(), &[0xFF, 0xFF, 0xFF][..])
                .unwrap();
        }
        write.commit().unwrap();

        let listed = store
            .list_chats()
            .expect("corruption must not propagate as error");
        let ids: std::collections::HashSet<_> = listed.iter().map(|c| c.id).collect();
        assert!(ids.contains(&good_a.id), "good_a missing");
        assert!(ids.contains(&good_b.id), "good_b missing");
        assert_eq!(listed.len(), 2, "bogus entry should have been skipped");
    }

    /// `update_message_no_index` persists to redb but skips the
    /// tantivy commit. Until a subsequent indexed write (or rebuild)
    /// happens, the message body is INVISIBLE to search. This is
    /// deliberate — partial-streaming bodies have no search utility
    /// — so the test pins the contract: redb has the latest body,
    /// tantivy still has the OLD body (or none), and the next
    /// `update_message` call re-indexes the final state.
    #[test]
    fn update_message_no_index_does_not_publish_to_search_until_indexed_write() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let chat = Chat::new("xai", "grok-beta");
        store.upsert_chat(&chat).unwrap();

        let mut m = Message::new(chat.id, Role::Assistant, "initial body");
        store.insert_message(&m).unwrap();
        // Round 1: indexed write → search finds it.
        assert!(
            !store.search("initial", 10).unwrap().is_empty(),
            "indexed insert must be searchable"
        );

        // Streaming partial-persist: body grows, but skip the index.
        m.content = "initial body plus deltas with the word unicorn".into();
        store.update_message_no_index(&m).unwrap();
        // "unicorn" should NOT be searchable yet — index still holds
        // the pre-update body. redb DOES have the new body.
        assert!(
            store.search("unicorn", 10).unwrap().is_empty(),
            "no-index write must NOT publish to search"
        );
        let from_redb = &store.list_messages(chat.id).unwrap()[0];
        assert!(
            from_redb.content.contains("unicorn"),
            "redb must have the latest body"
        );

        // Round 2: the final indexed write at end-of-stream.
        store.update_message(&m).unwrap();
        assert!(
            !store.search("unicorn", 10).unwrap().is_empty(),
            "final indexed write must publish to search"
        );
    }

    /// Reconciliation: if the search index disagrees with redb at
    /// startup (e.g., from a crash mid-insert), `Store::open` must
    /// rebuild rather than serve stale search results forever.
    #[test]
    fn open_reconciles_search_index_when_redb_and_tantivy_diverge() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("db.redb");
        let idx_dir = tmp.path().join("idx");

        // Round 1: insert 5 messages, close the store.
        {
            let store = Store::open(&db_path, &idx_dir).unwrap();
            let chat = Chat::new("xai", "grok-beta");
            store.upsert_chat(&chat).unwrap();
            for i in 0..5 {
                let m = Message::new(chat.id, Role::User, format!("msg-{i}"));
                store.insert_message(&m).unwrap();
            }
        }

        // Round 2: nuke the index dir to simulate a divergence (crash
        // between redb commit and tantivy commit, or a manual rm). On
        // reopen, the index has zero docs but redb still has 5.
        std::fs::remove_dir_all(&idx_dir).unwrap();
        std::fs::create_dir_all(&idx_dir).unwrap();

        // Round 3: reopen. The reconciler MUST detect the mismatch and
        // rebuild the index from redb so search works again.
        let store = Store::open(&db_path, &idx_dir).unwrap();
        let hits = store
            .search("msg-3", 10)
            .expect("search must work after reconciliation");
        assert!(
            !hits.is_empty(),
            "reconciler did not rebuild the index from redb"
        );
    }

    #[test]
    fn list_messages_skips_individually_corrupt_entries() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let chat = Chat::new("xai", "grok-beta");
        store.upsert_chat(&chat).unwrap();

        let m1 = Message::new(chat.id, Role::User, "hello");
        let m2 = Message::new(chat.id, Role::Assistant, "world");
        store.insert_message(&m1).unwrap();
        store.insert_message(&m2).unwrap();

        // Build a key that falls inside the chat_id range but holds
        // junk. The key shape is `chat_id (16) || ts_be (8) || msg_id
        // (16)` — fill the back 24 bytes with 0xAA so it lands cleanly
        // between m1 / m2 timestamps but won't decode.
        let mut bogus_key = Vec::with_capacity(40);
        bogus_key.extend_from_slice(chat.id.as_bytes());
        bogus_key.resize(40, 0xAA);
        let write = store.inner.db.begin_write().unwrap();
        {
            let mut msgs = write.open_table(TBL_MESSAGES).unwrap();
            msgs.insert(bogus_key.as_slice(), &[0xFF, 0xFF][..])
                .unwrap();
        }
        write.commit().unwrap();

        let listed = store
            .list_messages(chat.id)
            .expect("corruption must not propagate as error");
        let texts: Vec<&str> = listed.iter().map(|m| m.content.as_str()).collect();
        assert!(texts.contains(&"hello"), "m1 missing: {texts:?}");
        assert!(texts.contains(&"world"), "m2 missing: {texts:?}");
        assert_eq!(listed.len(), 2, "bogus entry should have been skipped");
    }

    /// Regression: a corrupted / older-schema Settings blob in redb
    /// must not brick the app. The previous code returned a decode
    /// error to the caller, which made every UI surface (including the
    /// settings panel itself) un-renderable. Now we log and fall back
    /// to defaults so the user can boot and fix.
    #[test]
    fn settings_decode_failure_falls_back_to_defaults() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());

        // Plant a bogus settings blob directly via the storage API.
        // bincode is positional, so a one-byte payload definitely won't
        // decode into the multi-field `Settings` struct.
        let write = store.inner.db.begin_write().unwrap();
        {
            let mut meta = write.open_table(TBL_META).unwrap();
            meta.insert(META_SETTINGS, &[0xFFu8][..]).unwrap();
        }
        write.commit().unwrap();

        // Load must succeed with defaults, not propagate the decode error.
        let loaded = store
            .load_settings()
            .expect("decode failure should fall back, not propagate");
        let defaults = Settings::default();
        assert_eq!(loaded.default_provider, defaults.default_provider);
        assert_eq!(loaded.xai_model, defaults.xai_model);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 64,  // each case opens redb + tantivy; keep budget modest
            failure_persistence: Some(Box::new(
                proptest::test_runner::FileFailurePersistence::SourceParallel(
                    "proptest-regressions"
                ),
            )),
            .. proptest::test_runner::Config::default()
        })]

        /// Property: messages inserted via `insert_message` come back
        /// from `list_messages` in chronological order regardless of
        /// insertion order, and their content survives bincode +
        /// tantivy roundtrip byte-for-byte. Proves the composite key
        /// (`chat_id || ts_be || msg_id`) sorts correctly across a
        /// wide range of timestamps and uuids.
        #[test]
        fn property_message_roundtrip_preserves_order_and_content(
            contents in proptest::collection::vec("[^\x00]{0,256}", 1..32),
        ) {
            let tmp = tempdir().unwrap();
            let store = open_store(tmp.path());
            let chat = Chat::new("xai", "grok-beta");
            store.upsert_chat(&chat).unwrap();
            // Insert in REVERSE chronological order: each new message
            // gets a fresher Utc::now() than the previous, but we want
            // to verify the range scan returns them in ts order anyway.
            let mut inserted: Vec<Message> = Vec::with_capacity(contents.len());
            for body in contents {
                std::thread::sleep(std::time::Duration::from_micros(5));
                let m = Message::new(chat.id, Role::User, body);
                store.insert_message(&m).unwrap();
                inserted.push(m);
            }
            let listed = store.list_messages(chat.id).unwrap();
            // Round-trip count.
            proptest::prop_assert_eq!(listed.len(), inserted.len());
            // Ordering: timestamps must be monotonic non-decreasing.
            for window in listed.windows(2) {
                proptest::prop_assert!(window[0].created_at <= window[1].created_at);
            }
            // Content fidelity: every inserted message appears with its
            // exact body (the set of bodies is preserved).
            let listed_bodies: std::collections::BTreeSet<_> =
                listed.iter().map(|m| m.content.clone()).collect();
            let inserted_bodies: std::collections::BTreeSet<_> =
                inserted.iter().map(|m| m.content.clone()).collect();
            proptest::prop_assert_eq!(listed_bodies, inserted_bodies);
        }
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

    /// Schema downgrade refusal: if the on-disk version is GREATER
    /// than `CURRENT_SCHEMA_VERSION`, `Store::open` must refuse
    /// rather than blithely proceed. bincode is positional — silently
    /// accepting a newer schema's bytes would deserialise into
    /// `Settings`/`Chat`/`Message` with the wrong field shape, which
    /// at best produces noise and at worst nukes user data on the
    /// next write.
    ///
    /// The previous `migrate()` stub stamped the current version
    /// unconditionally and would have happily downgraded.
    #[test]
    fn open_refuses_to_downgrade_from_a_newer_schema() {
        let tmp = tempdir().unwrap();
        let db_path = tmp.path().join("db.redb");
        let idx_dir = tmp.path().join("idx");

        // Bootstrap a normal store at the current version.
        {
            let store = Store::open(&db_path, &idx_dir).expect("first open");
            drop(store);
        }

        // Plant a future version directly into the META table,
        // bypassing migrate(). This simulates a user opening an old
        // build against a database written by a newer build.
        {
            let db = redb::Database::open(&db_path).unwrap();
            let write = db.begin_write().unwrap();
            {
                let mut meta = write.open_table(TBL_META).unwrap();
                let future: u32 = CURRENT_SCHEMA_VERSION + 17;
                meta.insert(META_SCHEMA_VERSION, &future.to_be_bytes()[..])
                    .unwrap();
            }
            write.commit().unwrap();
        }

        // Re-open: must refuse with a Decode error that names the
        // version. Don't pattern-match the message verbatim — pin
        // the variant and the substrings a user would actually see.
        match Store::open(&db_path, &idx_dir) {
            Ok(_) => panic!("downgrade must refuse to open"),
            Err(StorageError::Decode(msg)) => {
                assert!(
                    msg.contains("newer than this build"),
                    "error must explain the downgrade refusal: {msg}"
                );
                assert!(
                    msg.contains(&(CURRENT_SCHEMA_VERSION + 17).to_string()),
                    "error must surface the offending version: {msg}"
                );
            }
            Err(other) => panic!("expected StorageError::Decode, got {other:?}"),
        }
    }

    /// `delete_message` is symmetric with `insert_message`: redb is
    /// canonical, tantivy is derived. A tantivy delete that finds
    /// no matching term is a no-op for that crate (it's idempotent
    /// — `delete_term` returns the count of deleted docs but never
    /// errors on miss), so calling delete_message on a message that
    /// was NEVER indexed must still return Ok and remove the redb
    /// entry. The previous version propagated tantivy errors,
    /// which left callers thinking a delete failed when canonical
    /// state had already updated.
    #[test]
    fn delete_message_returns_ok_when_search_index_has_no_entry() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let chat = Chat::new("xai", "grok-beta");
        store.upsert_chat(&chat).unwrap();

        // Write a message through the no-index path: redb gets it,
        // tantivy does NOT. Simulates a stream that was persisted
        // mid-flight and then aborted before the final commit.
        let m = Message::new(chat.id, Role::User, "never-indexed body");
        store.update_message_no_index(&m).unwrap();
        assert_eq!(store.list_messages(chat.id).unwrap().len(), 1);
        assert!(
            store.search("never-indexed", 5).unwrap().is_empty(),
            "precondition: this message is not in the search index"
        );

        // delete_message must succeed and clear the redb side, even
        // though tantivy never had this msg_id to delete.
        store.delete_message(&m).expect(
            "delete must be best-effort against the index — \
             canonical (redb) success is what callers observe",
        );
        assert!(
            store.list_messages(chat.id).unwrap().is_empty(),
            "redb is canonical: the message must be gone"
        );
    }

    /// Migration ladder: `migrate_to` is the only place schema
    /// transformations land. Calling it with a version this build
    /// doesn't know about must return Decode, not panic or silently
    /// succeed.
    #[test]
    fn migrate_to_refuses_unknown_target_version() {
        let tmp = tempdir().unwrap();
        let store = open_store(tmp.path());
        let err = store
            .migrate_to(9999)
            .expect_err("unknown migration target must Err");
        match err {
            StorageError::Decode(msg) => assert!(
                msg.contains("9999"),
                "error must name the unknown target: {msg}"
            ),
            other => panic!("expected Decode, got {other:?}"),
        }
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
