//! Background services: chat completions, voice realtime, audio I/O, search,
//! and (optionally) local embeddings for RAG.

pub mod anthropic;
pub mod audio;
pub mod chat;
pub mod embeddings;
pub mod export;
pub mod local;
pub mod openai;
pub mod providers;
pub mod voice;
