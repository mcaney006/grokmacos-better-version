//! Filesystem locations used by the app. Resolved once at startup.

use directories::ProjectDirs;
use once_cell::sync::OnceCell;
use std::path::{Path, PathBuf};

static PROJECT_DIRS: OnceCell<ProjectDirs> = OnceCell::new();

/// Project identifier for `directories`. Stable across releases.
const QUALIFIER: &str = "com";
const ORGANIZATION: &str = "GrokInsane";
const APPLICATION: &str = "grok-insane";

fn dirs() -> &'static ProjectDirs {
    PROJECT_DIRS.get_or_init(|| {
        ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION).unwrap_or_else(|| {
            // Fallback: a relative `.grok-insane/` directory. Only triggers on
            // exotic platforms where no XDG / Known Folder is resolvable.
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let fallback = cwd.join(".grok-insane");
            std::fs::create_dir_all(&fallback).ok();
            // `ProjectDirs::from_path` returns `None` for an empty path
            // and only an empty path. Cascade through a chain of
            // non-empty fallbacks so we can never `expect` ourselves into
            // a process-wide panic: try the fallback dir, then "." (the
            // current dir), then the literal "grok-insane" relative
            // path. One of these is guaranteed non-empty.
            ProjectDirs::from_path(fallback)
                .or_else(|| ProjectDirs::from_path(PathBuf::from(".")))
                .unwrap_or_else(|| {
                    // Last resort: a single-segment relative path is
                    // accepted by `from_path` on every platform we
                    // support. If even THIS returns None the directories
                    // crate has shipped a regression; surface it loudly.
                    ProjectDirs::from_path(PathBuf::from(APPLICATION)).unwrap_or_else(|| {
                        tracing::error!("ProjectDirs::from_path returned None for every fallback; using `.` as data root");
                        // Build a minimal ProjectDirs against the only
                        // value we know works: ".". `unwrap` here is
                        // only reached after both prior `or_else` checks
                        // failed, which would itself be a directories
                        // crate bug. Use `unwrap_or_else` with a
                        // panicking closure ONLY here so the diagnostic
                        // makes it clear to ops what broke.
                        #[allow(clippy::unwrap_used)]
                        ProjectDirs::from_path(PathBuf::from(".")).unwrap()
                    })
                })
        })
    })
}

pub fn data_dir() -> &'static Path {
    dirs().data_dir()
}

pub fn config_dir() -> &'static Path {
    dirs().config_dir()
}

pub fn cache_dir() -> &'static Path {
    dirs().cache_dir()
}

pub fn db_path() -> PathBuf {
    data_dir().join("grok-insane.redb")
}

pub fn index_path() -> PathBuf {
    data_dir().join("search-index")
}

pub fn log_dir() -> PathBuf {
    cache_dir().join("logs")
}

/// Ensure all directories the app writes to exist.
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::create_dir_all(config_dir())?;
    std::fs::create_dir_all(cache_dir())?;
    std::fs::create_dir_all(log_dir())?;
    std::fs::create_dir_all(index_path())?;
    Ok(())
}
