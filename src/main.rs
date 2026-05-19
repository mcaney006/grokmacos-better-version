// The `Err` variants of our error enums embed `reqwest::Error` (and friends),
// which are intentionally large. We accept the size hit in exchange for clean
// `?`-propagation; callers immediately surface the error to a toast and the
// `Result` does not stay live on the stack.
#![allow(clippy::result_large_err)]
#![allow(clippy::items_after_test_module)]
#![allow(clippy::field_reassign_with_default)]

//! GrokInsane — cross-platform desktop client for xAI Grok and friends.
//!
//! Entrypoint sets up logging + paths, opens persistent storage, and hands
//! control to `app::GrokApp` via eframe.

mod app;
mod config;
mod error;
mod models;
mod paths;
mod secrets;
mod services;
mod storage;
mod theme;
mod ui;

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    paths::ensure_dirs().context("failed to create app directories")?;

    let _guards = init_tracing();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        rust = env!("CARGO_PKG_RUST_VERSION"),
        "starting grok-insane"
    );

    let store = storage::Store::open(&paths::db_path(), &paths::index_path())
        .context("failed to open storage")?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("grok-net")
        .build()
        .context("failed to build tokio runtime")?;

    let viewport = eframe::egui::ViewportBuilder::default()
        .with_title("GrokInsane")
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([880.0, 540.0]);

    let native = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "GrokInsane",
        native,
        Box::new(move |cc| Ok(Box::new(app::GrokApp::new(cc, store, runtime)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe exited: {e}"))?;

    Ok(())
}

fn init_tracing() -> Vec<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, EnvFilter};

    let log_dir = paths::log_dir();
    let file_appender = tracing_appender::rolling::daily(&log_dir, "grok-insane.log");
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,grok_insane=debug,wgpu=warn,winit=warn"));

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false).with_writer(std::io::stderr))
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_writer(file_writer),
        )
        .init();

    vec![file_guard]
}
