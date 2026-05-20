//! Criterion micro-benches over the two hottest persistence paths:
//!
//! 1. `Store::insert_message`  — redb write + tantivy index per call.
//! 2. `Store::search`          — tantivy query against a warm index.
//!
//! Each bench builds a fresh `Store` in a `tempfile::TempDir` so runs are
//! independent. Search benches first populate the index with a fixed corpus
//! (1k messages, ~80B each) so the latency numbers reflect a realistic-ish
//! steady state instead of an empty-index path.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use grok_insane::models::{Chat, Message, Role};
use grok_insane::storage::Store;
use tempfile::TempDir;

fn make_store() -> (Store, TempDir) {
    let dir = TempDir::new().expect("tempdir");
    let db = dir.path().join("db.redb");
    let idx = dir.path().join("idx");
    let store = Store::open(&db, &idx).expect("open store");
    (store, dir)
}

fn seed_chat(store: &Store) -> Chat {
    let chat = Chat::new("xai", "grok-beta");
    store.upsert_chat(&chat).expect("upsert chat");
    chat
}

fn bench_insert_message(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_insert_message");
    group.throughput(Throughput::Elements(1));
    let (store, _dir) = make_store();
    let chat = seed_chat(&store);
    group.bench_function("single", |b| {
        b.iter(|| {
            let msg = Message::new(
                chat.id,
                Role::User,
                "the quick brown fox jumps over the lazy dog",
            );
            store.insert_message(black_box(&msg)).expect("insert");
        })
    });
    group.finish();
}

fn bench_search(c: &mut Criterion) {
    let mut group = c.benchmark_group("storage_search");
    let (store, _dir) = make_store();
    let chat = seed_chat(&store);

    // Populate a 1k-message corpus with predictable phrasing so the query
    // hits real index entries instead of the empty-index fast path.
    let phrases = [
        "the quick brown fox jumps over the lazy dog",
        "rust ownership and the borrow checker",
        "tokio runtime spawn blocking task",
        "websocket keepalive heartbeat ping",
        "tantivy full text search index",
        "redb embedded key value store",
        "vector embedding cosine similarity",
        "criterion benchmark micro performance",
    ];
    for i in 0..1024 {
        let phrase = phrases[i % phrases.len()];
        let msg = Message::new(
            chat.id,
            if i % 2 == 0 { Role::User } else { Role::Assistant },
            format!("{phrase} ({i})"),
        );
        store.insert_message(&msg).expect("seed insert");
    }

    group.bench_function("query_one_word", |b| {
        b.iter(|| {
            let hits = store.search(black_box("tokio"), 10).expect("search");
            black_box(hits)
        })
    });

    group.bench_function("query_phrase", |b| {
        b.iter(|| {
            let hits = store
                .search(black_box("websocket keepalive"), 10)
                .expect("search");
            black_box(hits)
        })
    });

    group.finish();
}

criterion_group!(benches, bench_insert_message, bench_search);
criterion_main!(benches);
