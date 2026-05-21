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
            // Try non-empty fallbacks in order. The first version
            // chained an `unwrap_or_else(|| ... .unwrap())` at the end
            // that was a tautological infinite-loop-of-failure: the
            // outer call had already returned None, the inner call used
            // the SAME argument value and would naturally return None
            // too. Now each fallback uses a DIFFERENT non-empty
            // argument; the final panic only fires if literally every
            // attempt returned None, which would be a `directories`
            // crate regression worth surfacing rather than papering
            // over.
            ProjectDirs::from_path(fallback)
                .or_else(|| ProjectDirs::from_path(PathBuf::from(APPLICATION)))
                .or_else(|| ProjectDirs::from_path(PathBuf::from(".")))
                .unwrap_or_else(|| {
                    tracing::error!(
                        "ProjectDirs::from_path returned None for the fallback dir, \
                         the literal {APPLICATION:?}, and \".\". This is a directories \
                         crate regression — please file an issue."
                    );
                    // We've exhausted our options. Panicking here is
                    // genuinely the right outcome: the only alternative
                    // is to fabricate a ProjectDirs by transmute, which
                    // would be unsafe AND wrong.
                    #[allow(clippy::expect_used)]
                    {
                        panic!(
                            "ProjectDirs::from_path returned None for all fallbacks; \
                             cannot resolve any data directory on this platform"
                        );
                    }
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
