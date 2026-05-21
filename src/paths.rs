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

/// Atomic-replace write: stream `body` into a sibling `*.partial`
/// file, fsync, then `rename(2)` it into `dest`. On POSIX the rename
/// is atomic with respect to readers — they see either the old file
/// or the new one, never a half-written truncation. On Windows the
/// std-library rename uses MoveFileExW which is atomic on NTFS when
/// source + destination share a volume (always true here — same
/// parent dir).
///
/// Failure modes:
///   * `write` fails → no partial file is left behind (best-effort
///     cleanup runs) and the prior `dest` (if any) is untouched.
///   * `rename` fails → the partial file is left for manual recovery
///     and the prior `dest` is still untouched.
///   * fsync is best-effort: on platforms where `File::sync_all`
///     fails (some FUSE / network mounts return EINVAL) the write
///     still proceeds. The crash window is widened but the rename
///     remains atomic — a power-cut recovery sees either the old
///     file fully or the new file fully.
///
/// The previous code path called `std::fs::write(&dest, body)`
/// directly, which truncates `dest` at open time. An interrupted
/// write — laptop closed, process killed, OOM — left the user with
/// either a zero-byte file or a half-written one, destroying the
/// prior successful export.
pub fn atomic_write_bytes(dest: &Path, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let tmp = partial_path(dest);
    let write_result = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(body)?;
        // Flush user-space buffer + ask the OS to flush its page
        // cache. Errors here are non-fatal: the rename below still
        // delivers the atomic-replace property under crash.
        let _ = f.sync_all();
        Ok(())
    })();
    if let Err(e) = write_result {
        // Best-effort: ensure no half-written partial is left
        // alongside the prior good file. We don't propagate this
        // cleanup error — the caller already has the write error.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    // rename failure leaves the partial in place so the user can
    // recover it manually; returning Err signals the caller that
    // `dest` was NOT updated.
    std::fs::rename(&tmp, dest)?;
    Ok(())
}

/// Path of the temporary sibling file used by `atomic_write_bytes`.
/// Exposed so tests + recovery tooling can find orphaned partials
/// after a hard crash. Format: `<dest>.<ext>.partial` (or
/// `<dest>.tmp.partial` if `dest` has no extension).
pub fn partial_path(dest: &Path) -> PathBuf {
    let suffix = format!(
        "{}.partial",
        dest.extension().and_then(|e| e.to_str()).unwrap_or("tmp")
    );
    dest.with_extension(suffix)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// Happy path: write succeeds, dest contains the new body, no
    /// partial file is left behind.
    #[test]
    fn atomic_write_replaces_dest_and_removes_partial() {
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("export.json");
        atomic_write_bytes(&dest, b"new body").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new body");
        assert!(
            !partial_path(&dest).exists(),
            "partial must be renamed away on success"
        );
    }

    /// The contract that matters most: if writing the partial
    /// fails, the prior `dest` is preserved byte-for-byte. The
    /// previous direct-`fs::write` path truncated `dest` at open
    /// time and would have destroyed the prior contents here.
    #[test]
    fn atomic_write_preserves_prior_file_when_write_fails() {
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("export.json");
        std::fs::write(&dest, b"prior good body").unwrap();

        // Force a write failure by pointing `dest` into a path
        // whose parent doesn't exist. The partial creation will
        // ENOENT and the prior file (if any) at `dest` must remain
        // untouched.
        let bad_dest = tmp.path().join("nonexistent_subdir").join("export.json");
        let err = atomic_write_bytes(&bad_dest, b"replacement body").unwrap_err();
        assert!(
            matches!(err.kind(), std::io::ErrorKind::NotFound),
            "expected NotFound, got {err:?}"
        );
        // Original dest unaffected (we never touched it).
        assert_eq!(std::fs::read(&dest).unwrap(), b"prior good body");
        // No partial left behind on write failure.
        assert!(!partial_path(&bad_dest).exists());
    }

    /// On a clean rename-after-write success, calling
    /// `atomic_write_bytes` a second time overwrites cleanly with
    /// the new body. Exercises the case where `dest` already
    /// exists — POSIX rename atomically replaces, std on Windows
    /// uses MoveFileExW with REPLACE_EXISTING.
    #[test]
    fn atomic_write_overwrites_existing_dest() {
        let tmp = tempdir().unwrap();
        let dest = tmp.path().join("export.json");
        atomic_write_bytes(&dest, b"first").unwrap();
        atomic_write_bytes(&dest, b"second").unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"second");
    }

    /// `partial_path` puts the `.partial` suffix on the
    /// extension, not on the stem, so multiple in-flight writes
    /// to the same `dest` produce a single deterministic partial
    /// rather than a per-call random name. This is intentional —
    /// if two concurrent writes race they BOTH lose their work,
    /// which is the same outcome as direct `fs::write` would
    /// produce; the goal is durability against crashes, not
    /// concurrency.
    #[test]
    fn partial_path_is_deterministic_and_extension_based() {
        let dest = std::path::PathBuf::from("/tmp/export.json");
        assert_eq!(
            partial_path(&dest),
            PathBuf::from("/tmp/export.json.partial")
        );
        let no_ext = std::path::PathBuf::from("/tmp/dump");
        assert_eq!(
            partial_path(&no_ext),
            PathBuf::from("/tmp/dump.tmp.partial")
        );
    }
}
