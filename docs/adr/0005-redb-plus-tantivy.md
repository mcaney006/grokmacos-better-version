# ADR 0005: redb (K/V) + tantivy (full-text) split

## Status

Accepted (2026-05).

## Context

We need local persistence for:

1. Settings (single struct, small, frequently read).
2. Chats (~10s-100s per user, small bodies, ordered by `updated_at`).
3. Messages (~100s-10000s per user, variable body size, ordered by
   `created_at` within a chat, scanned in range queries).
4. Full-text search over message bodies (BM25, fuzzy, snippet
   highlighting).

SQLite via `rusqlite` was the obvious default. We considered:

- SQLite + FTS5 — single store, mature, but pulls a C dep with a
  build-script that compiles ~150 kLOC of C code. Cross-compile pain.
- SeaORM / Diesel — ORM overhead, schema migrations that don't pay
  for themselves at our scale.
- sled — abandoned upstream as of 2025; explicit "do not use in
  production" notice.
- redb — pure Rust, ACID, single-writer-multi-reader, simple
  composite-key range queries.
- tantivy — pure Rust Lucene-shaped index, BM25 + fuzzy + snippets,
  no SQL.

## Decision

Two engines, eventually consistent:

- **redb is the source of truth** for chats, messages, settings. The
  message key layout is `chat_id (16) || created_at_micros_be (8) ||
  msg_id (16)` = 40 bytes, which sorts chronologically within a chat
  via big-endian timestamp encoding (with the `i64 → u64` shift in
  `message_key`).
- **tantivy is a derived index** populated synchronously inside
  `insert_message` and reset/rebuilt by `Store::rebuild_index`.
  Tantivy failure does NOT block message writes — it's
  `tracing::warn!`'d and the message persists in redb regardless.
- bincode 2 with the `serde` shim is the wire format for redb values.
  See ADR 0004 for the per-entry-decode-skip posture against schema
  drift.

## Consequences

- Pure Rust dep tree: no C compiler, no `pkg-config` for sqlite,
  cross-compile to all four release targets without surprise.
- Two engines means two failure modes. Mitigated by:
  - `Store::rebuild_index` exists and is invokable to rebuild the
    derived index from redb if it diverges.
  - `list_chats` / `list_messages` skip per-entry decode failures
    with a log line; one bad row doesn't lock the user out (see ADR
    0004 partnership).
- Property tests in `storage::tests::property_message_roundtrip_*`
  verify the composite key sorts correctly across arbitrary message
  counts.

## Rejected alternatives

- **SQLite + FTS5** — the C build-script hurts cross-compile and
  reproducible-build stories more than the engine combinatorics hurt
  us. ~150 KB of compiled bincode + ~1 MB of tantivy index per 10k
  messages is well within budget.
- **A single embedded vector DB** (qdrant-rs, etc.) — overkill for
  v0.1; we already get BM25 + fuzzy from tantivy. Semantic re-ranking
  is opt-in behind `--features rag` via `fastembed`.
