//! GrokInsane — cross-platform desktop client for xAI Grok and friends.
//!
//! Entrypoint sets up logging + paths, parses a small CLI surface for headless
//! ops (`--version`, `--diag`, `--reset-db`), and otherwise hands control to
//! `app::GrokApp` via eframe. All real code lives in the library crate; this
//! file is the binary's `main()` plus its CLI dispatch.

use anyhow::Context;
use grok_insane::{app, models, paths, secrets, storage};

fn main() -> anyhow::Result<()> {
    paths::ensure_dirs().context("failed to create app directories")?;

    if let Some(action) = parse_cli() {
        return run_cli(action);
    }

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

#[derive(Debug)]
enum CliAction {
    Version,
    Help,
    Diag,
    ResetDb { confirm: bool },
}

fn parse_cli() -> Option<CliAction> {
    let mut args = std::env::args().skip(1);
    let first = args.next()?;
    let action = match first.as_str() {
        "--version" | "-V" => CliAction::Version,
        "--help" | "-h" => CliAction::Help,
        "--diag" => CliAction::Diag,
        "--reset-db" => CliAction::ResetDb {
            confirm: args.any(|a| a == "--yes"),
        },
        _ => CliAction::Help,
    };
    Some(action)
}

fn run_cli(action: CliAction) -> anyhow::Result<()> {
    match action {
        CliAction::Version => {
            println!(
                "grok-insane {} (rust >= {})",
                env!("CARGO_PKG_VERSION"),
                env!("CARGO_PKG_RUST_VERSION")
            );
            Ok(())
        }
        CliAction::Help => {
            print_help();
            Ok(())
        }
        CliAction::Diag => diag(),
        CliAction::ResetDb { confirm } => {
            let exit = reset_db(confirm)?;
            if let Some(msg) = exit.message {
                eprintln!("{msg}");
            }
            if exit.code != 0 {
                std::process::exit(exit.code);
            }
            Ok(())
        }
    }
}

fn print_help() {
    println!("grok-insane {}", env!("CARGO_PKG_VERSION"));
    println!();
    println!("USAGE:");
    println!("    grok-insane                 launch the desktop app");
    println!("    grok-insane --version       print version and exit");
    println!("    grok-insane --diag          self-test storage, paths, secrets");
    println!("    grok-insane --reset-db --yes  wipe local DB + search index");
    println!("    grok-insane --help          this message");
}

fn diag() -> anyhow::Result<()> {
    println!("data dir:    {}", paths::data_dir().display());
    println!("config dir:  {}", paths::config_dir().display());
    println!("cache dir:   {}", paths::cache_dir().display());
    println!("db path:     {}", paths::db_path().display());
    println!("index path:  {}", paths::index_path().display());
    println!();

    let store =
        storage::Store::open(&paths::db_path(), &paths::index_path()).context("open store")?;
    let n = store.count_messages().unwrap_or(0);
    let chats = store.list_chats().unwrap_or_default();
    println!("chats:       {}", chats.len());
    println!("messages:    {n}");
    println!();

    for p in models::Provider::all() {
        let label = match secrets::get_api_key(p.id()) {
            Ok(Some(_)) => "present",
            Ok(None) => "missing",
            Err(e) => {
                eprintln!("keyring [{}]: {e}", p.id());
                "error"
            }
        };
        println!("api key [{:9}] {}", p.id(), label);
    }
    Ok(())
}

/// Result returned by `reset_db`. Distinct from `anyhow::Result` because
/// the no-confirmation case is NOT a panic-worthy program error — it's
/// a normal "user forgot the safety flag" exit. The previous version
/// called `std::process::exit(2)` from inside a function that pretended
/// to return Result, which would have hidden the early-exit from
/// anything that wrapped reset_db in retry/log logic.
struct ResetExit {
    code: i32,
    message: Option<String>,
}

fn reset_db(confirm: bool) -> anyhow::Result<ResetExit> {
    if !confirm {
        return Ok(ResetExit {
            code: 2,
            message: Some("refusing to wipe data without --yes".to_owned()),
        });
    }
    let db = paths::db_path();
    let idx = paths::index_path();
    if db.exists() {
        std::fs::remove_file(&db).with_context(|| format!("remove {}", db.display()))?;
    }
    if idx.exists() {
        std::fs::remove_dir_all(&idx).with_context(|| format!("remove {}", idx.display()))?;
        std::fs::create_dir_all(&idx).ok();
    }
    println!("removed {}", db.display());
    println!("removed {}", idx.display());
    Ok(ResetExit {
        code: 0,
        message: None,
    })
}

fn init_tracing() -> Vec<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{fmt, EnvFilter};

    let log_dir = paths::log_dir();
    let file_appender = tracing_appender::rolling::daily(&log_dir, "grok-insane.log");
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    // Default filter is intentionally conservative: our own crate at `debug`,
    // every network/UI library at `warn`. This stops reqwest/hyper/tungstenite
    // from emitting low-level frame traces that could capture an Authorization
    // header or request body during a debug session.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new(
            "info,\
             grok_insane=debug,\
             wgpu=warn,\
             winit=warn,\
             reqwest=warn,\
             hyper=warn,\
             rustls=warn,\
             tungstenite=warn,\
             tokio_tungstenite=warn,\
             h2=warn",
        )
    });

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
