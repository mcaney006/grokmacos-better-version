#![forbid(unsafe_code)]

//! `cargo xtask` — Rust replacements for what would normally be shell scripts.
//!
//! Run any command with `cargo xtask <name>`. The set of names is:
//!
//! * `check`          — `cargo fmt --check` + `clippy -D warnings` + `test`
//! * `fmt`            — `cargo fmt --all`
//! * `lint`           — `cargo clippy --all-targets -- -D warnings`
//! * `test`           — `cargo test --all`
//! * `dev`            — `cargo run` with verbose logs enabled
//! * `dist`           — release build, strip symbols, copy to `dist/<target>/`
//! * `bundle`         — per-OS bundle (`.app` / `.exe` / staged dir) in `dist/`
//! * `sign`           — macOS only: hardened-runtime codesign with Developer ID
//! * `notarize`       — macOS only: submit + wait + staple via `xcrun notarytool`
//! * `dmg`            — macOS only: end-to-end `.app` -> signed -> notarized -> DMG
//! * `reset`          — wipe local app data via `grok-insane --reset-db --yes`
//! * `install-deps`   — install OS dev libs (Linux apt-get only)
//! * `ci`             — what CI runs: install-deps + check + dist
//! * `clean`          — `cargo clean` + remove `dist/`
//! * `help`           — print usage
//!
//! Everything is pure Rust + `std::process::Command`. macOS packaging shells
//! out to system binaries (`codesign`, `hdiutil`, `xcrun notarytool`,
//! `xcrun stapler`) — those are Apple's tools, not third-party shell scripts.
//!
//! Module layout:
//!   * `commands`  — routine build/test/release helpers
//!   * `mac`       — macOS signing, notarization, DMG packaging
//!   * `ci_driver` — `CiStage` enum + GHA-aware stage runner + release packaging

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

mod ci_driver;
mod commands;
mod mac;

fn main() {
    let started = Instant::now();
    let cmd_name = env::args().nth(1).unwrap_or_else(|| "help".into());
    let result = run();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let status = match &result {
        Ok(()) => "success",
        Err(_) => "failure",
    };
    let _ = record_metric(TelemetryEvent {
        command: &cmd_name,
        duration_ms: elapsed_ms,
        status,
        target: env!("BUILD_TARGET_TRIPLE"),
        version: env!("CARGO_PKG_VERSION"),
    });
    if let Err(err) = result {
        eprintln!("xtask: {err:#}");
        std::process::exit(1);
    }
}

#[derive(Serialize)]
struct TelemetryEvent<'a> {
    command: &'a str,
    duration_ms: u64,
    status: &'a str,
    target: &'a str,
    version: &'a str,
}

/// Append one JSON line to `dist/xtask-metrics.jsonl`. The file is
/// .gitignore'd; consumers tail it for CI trend analysis.
fn record_metric(ev: TelemetryEvent<'_>) -> std::io::Result<()> {
    let dist = workspace_root().join("dist");
    std::fs::create_dir_all(&dist)?;
    let path = dist.join("xtask-metrics.jsonl");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{}", serde_json::to_string(&ev).unwrap_or_default())?;
    Ok(())
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_else(|| "help".into());
    let rest: Vec<String> = args.collect();
    match cmd.as_str() {
        "check" => commands::check(),
        "fmt" => commands::fmt(),
        "lint" => commands::lint(),
        "test" => commands::test(),
        "dev" => commands::dev(&rest),
        "dist" => commands::dist(),
        "bundle" => commands::bundle(),
        "audit" => commands::audit(),
        "sbom" => commands::sbom(),
        "doctor" => commands::doctor(&rest),
        "preflight" => commands::preflight(),
        "hygiene" => commands::hygiene(),
        "reproducible" => commands::reproducible(),
        "release" => commands::release(),
        "sign" => mac::sign(),
        "notarize" => mac::notarize(),
        "dmg" => mac::dmg(),
        "reset" => commands::reset(),
        "install-deps" => commands::install_deps(),
        "ci" => ci_driver::ci(&rest),
        "clean" => commands::clean(),
        "clean-deep" => commands::clean_deep(),
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        other => {
            print_help();
            bail!("unknown xtask command: {other}");
        }
    }
}

fn print_help() {
    println!("cargo xtask <command>");
    println!();
    println!("Commands:");
    println!("  check         fmt --check + clippy -D warnings + cargo test");
    println!("  fmt           cargo fmt --all");
    println!("  lint          cargo clippy --all-targets -- -D warnings");
    println!("  test          cargo test --all");
    println!("  dev [args]    cargo run with debug logging (forwards extra args)");
    println!("  dist          cargo build --release, strip, copy to dist/<target>/");
    println!("  bundle        per-OS bundle in dist/  (.app / .exe / staged dir)");
    println!("  audit         run cargo-audit + cargo-deny (installs them if missing)");
    println!("  sbom          emit CycloneDX SBOM at dist/<triple>/grok-insane.sbom.json");
    println!("  doctor [--strict]");
    println!("                verify the local environment can build + run the app.");
    println!("                --strict turns missing optional tools into hard failures.");
    println!("  preflight     check + audit + sbom + dist (run before opening a release PR)");
    println!("  hygiene       cargo-machete: scan for unused dependencies");
    println!("  reproducible  build twice and diff the hashes; proves determinism");
    println!("  release       final-boss release pipeline: check->hygiene->audit->sbom->dist->bundle->dmg");
    println!("  sign          macOS: codesign --options runtime --timestamp with Developer ID");
    println!("  notarize      macOS: xcrun notarytool submit --wait + stapler");
    println!("  dmg           macOS: bundle -> sign -> notarize -> staple -> .dmg in dist/");
    println!("  reset         wipe local app data");
    println!("  install-deps  install OS dev libs (Linux apt only)");
    println!("  ci [--stage NAME]");
    println!("                CI driver. No flag: run the whole pipeline.");
    println!("                --stage NAME: run one stage (install-deps | fmt | clippy");
    println!("                | test | build | audit | sbom | hygiene | reproducible");
    println!("                | nextest | package-tarball | package-zip | sign-artifacts");
    println!("                | compute-source-date-epoch).");
    println!("                Inside GitHub Actions, emits ::group:: markers and writes");
    println!("                rows to $GITHUB_STEP_SUMMARY.");
    println!("  clean         cargo clean + remove dist/");
    println!(
        "  clean-deep    cargo clean + remove dist/ + ~/.cargo/registry/cache for this workspace"
    );
    println!("  help          this message");
}

// --- shared helpers --------------------------------------------------------

pub(crate) fn cargo<I, S>(args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let cargo_bin = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo_bin);
    cmd.args(args);
    run_cmd(&mut cmd)
}

pub(crate) fn run_cmd(cmd: &mut Command) -> Result<()> {
    let pretty = format!("{:?}", cmd);
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn {pretty}"))?;
    if !status.success() {
        bail!("command failed ({}): {pretty}", status);
    }
    Ok(())
}

pub(crate) fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(".").to_path_buf())
}

pub(crate) fn bin_name() -> String {
    #[cfg(windows)]
    {
        "grok-insane.exe".into()
    }
    #[cfg(not(windows))]
    {
        "grok-insane".into()
    }
}

/// Compile-time-baked target triple. The build script in `xtask/build.rs`
/// records the Cargo-provided `TARGET` env var as `BUILD_TARGET_TRIPLE`,
/// so this is a constant load — no `rustc -vV` subprocess per call.
pub(crate) fn host_target_triple() -> String {
    env!("BUILD_TARGET_TRIPLE").to_string()
}

/// Cross-platform PATH lookup. Delegates to the `which` crate because the
/// previous std-only implementation missed Windows extensions (`.exe`,
/// `.cmd`, `.bat`) and ignored `PATHEXT`.
pub(crate) fn which(prog: &str) -> Option<PathBuf> {
    which::which(prog).ok()
}
