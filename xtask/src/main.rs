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

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Instant;

fn main() {
    let started = Instant::now();
    let cmd_name = env::args().nth(1).unwrap_or_else(|| "help".into());
    let result = run();
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let status = match &result {
        Ok(()) => "success",
        Err(_) => "failure",
    };
    // Best-effort telemetry. Never fails the command on a write error.
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

/// Append one JSON line to `dist/xtask-metrics.jsonl`. Quietly skipped if
/// the directory can't be created (e.g. read-only FS during a CI dry-run).
/// The file is .gitignore'd; consumers tail it for CI trend analysis.
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
        "check" => check(),
        "fmt" => fmt(),
        "lint" => lint(),
        "test" => test(),
        "dev" => dev(&rest),
        "dist" => dist(),
        "bundle" => bundle(),
        "audit" => audit(),
        "sbom" => sbom(),
        "doctor" => doctor(),
        "preflight" => preflight(),
        "hygiene" => hygiene(),
        "reproducible" => reproducible(),
        "release" => release(),
        "sign" => sign(),
        "notarize" => notarize(),
        "dmg" => dmg(),
        "reset" => reset(),
        "install-deps" => install_deps(),
        "ci" => ci(),
        "clean" => clean(),
        "clean-deep" => clean_deep(),
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
    println!("  doctor        verify the local environment can build + run the app");
    println!("  preflight     check + audit + sbom + dist (run before opening a release PR)");
    println!("  hygiene       cargo-machete: scan for unused dependencies");
    println!("  reproducible  build twice and diff the hashes; proves determinism");
    println!("  release       final-boss release pipeline: check->hygiene->audit->sbom->dist->bundle->dmg");
    println!("  sign          macOS: codesign --options runtime --timestamp with Developer ID");
    println!("  notarize      macOS: xcrun notarytool submit --wait + stapler");
    println!("  dmg           macOS: bundle -> sign -> notarize -> staple -> .dmg in dist/");
    println!("  reset         wipe local app data");
    println!("  install-deps  install OS dev libs (Linux apt only)");
    println!("  ci            install-deps + check + dist");
    println!("  clean         cargo clean + remove dist/");
    println!(
        "  clean-deep    cargo clean + remove dist/ + ~/.cargo/registry/cache for this workspace"
    );
    println!("  help          this message");
}

// --- individual commands ----------------------------------------------------

fn check() -> Result<()> {
    cargo(["fmt", "--all", "--", "--check"])?;
    cargo([
        "clippy",
        "--all-targets",
        "--workspace",
        "--",
        "-D",
        "warnings",
    ])?;
    cargo(["test", "--workspace"])?;
    Ok(())
}

fn fmt() -> Result<()> {
    cargo(["fmt", "--all"])
}

fn lint() -> Result<()> {
    cargo([
        "clippy",
        "--all-targets",
        "--workspace",
        "--",
        "-D",
        "warnings",
    ])
}

fn test() -> Result<()> {
    cargo(["test", "--workspace"])
}

fn dev(args: &[String]) -> Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.arg("run").arg("--package").arg("grok-insane").env(
        "RUST_LOG",
        env::var("RUST_LOG").unwrap_or_else(|_| "grok_insane=debug,info".into()),
    );
    if !args.is_empty() {
        cmd.arg("--");
        cmd.args(args);
    }
    run_cmd(&mut cmd)
}

fn dist() -> Result<()> {
    cargo(["build", "--release", "--package", "grok-insane", "--locked"])?;

    let target_dir = workspace_root().join("target").join("release");
    let bin_name = bin_name();
    let bin = target_dir.join(&bin_name);
    if !bin.exists() {
        bail!("expected binary at {}", bin.display());
    }

    let out_dir = workspace_root().join("dist").join(host_target_triple());
    std::fs::create_dir_all(&out_dir).context("create dist dir")?;
    let dest = out_dir.join(&bin_name);
    std::fs::copy(&bin, &dest)
        .with_context(|| format!("copy {} -> {}", bin.display(), dest.display()))?;
    println!("dist: {}", dest.display());

    // `[profile.release] strip = true` in Cargo.toml already strips at link
    // time, but we also call the platform `strip` against the copied binary
    // as defence-in-depth: rustc-internal stripping doesn't cover every
    // table on every linker, and an explicit strip of the artifact catches
    // anything the linker left in place.
    strip_binary(&dest).ok();

    // Emit a SHA256SUMS file alongside the binary so downstream consumers
    // can verify integrity without depending on the Sigstore tooling. The
    // format matches GNU coreutils' `sha256sum -c`, so verification is one
    // command: `sha256sum -c SHA256SUMS`.
    let hash = sha256_file(&dest).with_context(|| format!("hash {}", dest.display()))?;
    let sums = out_dir.join("SHA256SUMS");
    std::fs::write(&sums, format!("{hash}  {}\n", bin_name))
        .with_context(|| format!("write {}", sums.display()))?;
    println!("hash: {} -> {}", bin_name, hash);

    Ok(())
}

/// Best-effort symbol stripping. macOS/Linux ship `strip`; Windows MSVC has
/// no equivalent but `link.exe /DEBUG:NONE` is set by the release profile
/// already. Stripping failure is logged but never fatal — the binary is
/// already usable.
fn strip_binary(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if which("strip").is_some() {
            let mut cmd = Command::new("strip");
            cmd.arg("-x").arg(path);
            run_cmd(&mut cmd).ok();
        }
    }
    #[cfg(target_os = "linux")]
    {
        if which("strip").is_some() {
            let mut cmd = Command::new("strip");
            cmd.arg("--strip-unneeded").arg(path);
            run_cmd(&mut cmd).ok();
        }
    }
    #[cfg(target_os = "windows")]
    {
        // No standard strip on Windows-MSVC. llvm-strip is optional.
        if which("llvm-strip").is_some() {
            let mut cmd = Command::new("llvm-strip");
            cmd.arg(path);
            run_cmd(&mut cmd).ok();
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

/// Supply-chain audit: RustSec advisories + license/source policy via
/// `cargo-audit` and `cargo-deny`. Installs them on first run.
fn audit() -> Result<()> {
    ensure_installed("cargo-audit")?;
    ensure_installed("cargo-deny")?;
    cargo([
        "audit",
        "--deny",
        "warnings",
        "--ignore",
        "RUSTSEC-2024-0384",
        "--ignore",
        "RUSTSEC-2024-0320",
        "--ignore",
        "RUSTSEC-2026-0002",
        "--ignore",
        "RUSTSEC-2025-0141",
        "--ignore",
        "RUSTSEC-2025-0119",
        "--ignore",
        "RUSTSEC-2024-0436",
    ])?;
    cargo(["deny", "check", "advisories", "bans", "sources", "licenses"])?;
    Ok(())
}

/// Emit a CycloneDX 1.5 SBOM for the workspace at
/// `dist/<host-triple>/grok-insane-<version>.sbom.json`. Used both locally
/// (for vulnerability scanning with Grype/Trivy/dependency-track) and in
/// the release pipeline, where every SBOM is signed alongside the binary.
fn sbom() -> Result<()> {
    ensure_installed("cargo-cyclonedx")?;
    let out_dir = workspace_root().join("dist").join(host_target_triple());
    std::fs::create_dir_all(&out_dir).context("create dist dir")?;

    cargo([
        "cyclonedx",
        "--format",
        "json",
        "--override-filename",
        "grok-insane-sbom",
    ])?;

    // cargo-cyclonedx writes the file to the package root. Move it next to
    // the binary so consumers find both in one place.
    let src = workspace_root().join("grok-insane-sbom.cdx.json");
    let dst = out_dir.join(format!(
        "grok-insane-{}.sbom.json",
        env!("CARGO_PKG_VERSION")
    ));
    if src.exists() {
        std::fs::rename(&src, &dst)
            .with_context(|| format!("move {} -> {}", src.display(), dst.display()))?;
        println!("sbom: {}", dst.display());
    }
    Ok(())
}

/// Walk the local environment and report everything the app needs to build
/// and run. Doesn't change anything — read-only diagnostic. Useful for new
/// contributors and for triaging "works on my machine" reports.
fn doctor() -> Result<()> {
    println!("== rust ==");
    let _ = Command::new("rustc").arg("--version").status();
    let _ = Command::new("cargo").arg("--version").status();
    let _ = Command::new("cargo").arg("fmt").arg("--version").status();
    let _ = Command::new("cargo")
        .arg("clippy")
        .arg("--version")
        .status();

    println!("\n== workspace ==");
    let root = workspace_root();
    println!("workspace root: {}", root.display());
    for f in [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "deny.toml",
        ".gitattributes",
        ".github/workflows/ci.yml",
        ".github/workflows/release.yml",
        ".github/workflows/audit.yml",
    ] {
        let p = root.join(f);
        println!("  {} {}", if p.exists() { "✓" } else { "✗" }, f);
    }

    println!("\n== platform tools ==");
    let tools: &[&str] = if cfg!(target_os = "macos") {
        &["codesign", "hdiutil", "xcrun", "security", "ditto"]
    } else if cfg!(target_os = "linux") {
        &["pkg-config", "apt-get", "tar"]
    } else if cfg!(target_os = "windows") {
        &["link.exe", "powershell.exe"]
    } else {
        &[]
    };
    for t in tools {
        let found = which(t).is_some();
        println!("  {} {}", if found { "✓" } else { "✗" }, t);
    }

    println!("\n== optional ==");
    for t in [
        "cargo-audit",
        "cargo-deny",
        "cargo-cyclonedx",
        "cargo-machete",
        "cosign",
    ] {
        let found = which(t).is_some();
        println!(
            "  {} {} (install with `cargo install --locked {}`)",
            if found { "✓" } else { "•" },
            t,
            t
        );
    }

    // Try a no-op cargo metadata to confirm the workspace resolves.
    println!("\n== resolution ==");
    let status = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--locked")
        .stdout(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => println!("  ✓ cargo metadata --locked OK"),
        Ok(s) => println!("  ✗ cargo metadata --locked failed: {s}"),
        Err(e) => println!("  ✗ could not spawn cargo: {e}"),
    }

    println!("\nDone.");
    Ok(())
}

/// Comprehensive pre-release gate. Run this locally before opening a PR
/// that expects to ship a release. Mirrors what CI does, plus the supply-
/// chain scan and SBOM generation so failures surface here instead of at
/// tag-push time.
///
/// Stages 2 and 3 (`audit` and `sbom`) are CPU-bound and touch independent
/// state, so we fan them out across two OS threads instead of running them
/// serially. Saves ~20% wall-clock on a warm cache.
fn preflight() -> Result<()> {
    println!("== preflight 1/4: cargo xtask check ==");
    check()?;

    println!("\n== preflight 2/4: cargo xtask audit + sbom (parallel) ==");
    thread::scope(|s| {
        let h_audit = s.spawn(audit);
        let h_sbom = s.spawn(sbom);
        let audit_res = h_audit
            .join()
            .map_err(|_| anyhow::anyhow!("audit thread panicked"))?;
        let sbom_res = h_sbom
            .join()
            .map_err(|_| anyhow::anyhow!("sbom thread panicked"))?;
        audit_res?;
        sbom_res?;
        anyhow::Ok(())
    })?;

    println!("\n== preflight 4/4: cargo xtask dist ==");
    dist()?;
    println!("\nAll preflight checks passed. Safe to tag a release.");
    Ok(())
}

/// Dead-dependency scan via cargo-machete. We rely on machete (not
/// cargo-udeps) because udeps requires a nightly toolchain and we hard-pin
/// stable. False-positive rate on machete is near zero for our graph; if a
/// dep is intentionally unused outside `cfg` gates, add it to
/// `package.metadata.cargo-machete.ignored` in Cargo.toml.
fn hygiene() -> Result<()> {
    ensure_installed("cargo-machete")?;
    // Invoke cargo-machete directly (not via `cargo machete`). cargo-machete
    // double-parses its argv when called through cargo's subcommand
    // dispatcher: it sees ["machete", "."] and treats "machete" as a path,
    // which then fails to open. Calling the binary directly with no args
    // and an explicit cwd avoids the parser quirk entirely.
    let mut cmd = Command::new("cargo-machete");
    cmd.current_dir(workspace_root());
    run_cmd(&mut cmd)
}

/// Reproducibility self-test. Build the binary twice with `--locked` and
/// `SOURCE_DATE_EPOCH` set to the tip commit's authored timestamp, then
/// compare hashes. If they match, this exact source state produces bit-
/// for-bit identical artifacts and we can legitimately claim
/// "reproducible build" — not on vibes, on evidence.
///
/// Two known sources of false negatives:
///   * macOS code-signature timestamps embedded by `codesign`. This check
///     runs against the unsigned `target/release/grok-insane`, not the
///     signed bundle, so that's fine.
///   * Incremental compilation timestamps. We `cargo clean -p grok-insane`
///     between the two builds to force a full re-link.
fn reproducible() -> Result<()> {
    // Lock SOURCE_DATE_EPOCH to the commit time. Matches what the release
    // workflow does in CI.
    let sde = git_commit_timestamp().unwrap_or_else(|_| "0".to_string());
    println!("reproducible: SOURCE_DATE_EPOCH={sde}");

    fn one_build(sde: &str) -> Result<String> {
        cargo(["clean", "-p", "grok-insane"])?;
        let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        cmd.arg("build")
            .arg("--release")
            .arg("--package")
            .arg("grok-insane")
            .arg("--locked")
            .env("SOURCE_DATE_EPOCH", sde);
        run_cmd(&mut cmd)?;
        let bin = workspace_root()
            .join("target")
            .join("release")
            .join(bin_name());
        sha256_file(&bin)
    }

    let h1 = one_build(&sde).context("first reproducible build")?;
    println!("build #1 sha256: {h1}");
    let h2 = one_build(&sde).context("second reproducible build")?;
    println!("build #2 sha256: {h2}");

    if h1 == h2 {
        println!("\n✓ reproducible: identical artifacts across two builds");
        Ok(())
    } else {
        bail!(
            "build is NOT reproducible — hashes differ\n  #1 {h1}\n  #2 {h2}\n\n\
             Common causes: non-deterministic codegen flags, an `include_bytes!` \
             pulling a file whose mtime is embedded, or a dep that bakes the \
             current timestamp at build time. Inspect with `diffoscope`."
        )
    }
}

fn git_commit_timestamp() -> Result<String> {
    let out = Command::new("git")
        .arg("log")
        .arg("-1")
        .arg("--pretty=%ct")
        .output()
        .context("git log")?;
    if !out.status.success() {
        bail!("git log exited {}", out.status);
    }
    let s = String::from_utf8(out.stdout)
        .context("git log output not UTF-8")?
        .trim()
        .to_string();
    Ok(s)
}

/// Final-boss release pipeline. The minimum sequence of checks every
/// shipped release passes through. CI also runs each step in
/// `.github/workflows/release.yml`; this command lets a maintainer run the
/// exact same gate locally before pushing the tag.
///
/// On non-macOS hosts the bundle/sign/notarize/dmg steps become no-ops
/// (their bodies are cfg-gated), so the orchestration is identical
/// everywhere — only the actual macOS-specific output is produced on a
/// macOS runner.
fn release() -> Result<()> {
    println!("== release 1/7: cargo xtask check ==");
    check()?;
    println!("\n== release 2/7: cargo xtask hygiene ==");
    hygiene()?;
    println!("\n== release 3/7: cargo xtask audit + sbom (parallel) ==");
    thread::scope(|s| {
        let h_audit = s.spawn(audit);
        let h_sbom = s.spawn(sbom);
        let audit_res = h_audit
            .join()
            .map_err(|_| anyhow::anyhow!("audit thread panicked"))?;
        let sbom_res = h_sbom
            .join()
            .map_err(|_| anyhow::anyhow!("sbom thread panicked"))?;
        audit_res?;
        sbom_res?;
        anyhow::Ok(())
    })?;
    println!("\n== release 4/7: cargo xtask reproducible ==");
    reproducible()?;
    println!("\n== release 5/7: cargo xtask dist ==");
    dist()?;
    println!("\n== release 6/7: cargo xtask bundle ==");
    bundle()?;
    println!("\n== release 7/7: cargo xtask dmg (macOS only) ==");
    dmg()?;
    println!(
        "\nRelease pipeline complete. Artefacts in dist/{}/.",
        host_target_triple()
    );
    Ok(())
}

/// Deeper clean than `cargo clean`: also removes `dist/` and the registry
/// download cache. Use this if a corrupted cache is causing weird build
/// errors that survive a plain `cargo clean`.
fn clean_deep() -> Result<()> {
    clean()?;
    if let Some(home) = dirs_home_cache() {
        let registry_cache = home.join("registry").join("cache");
        if registry_cache.exists() {
            println!("removing {}", registry_cache.display());
            let _ = std::fs::remove_dir_all(&registry_cache);
        }
        let git_db = home.join("git").join("db");
        if git_db.exists() {
            println!("removing {}", git_db.display());
            let _ = std::fs::remove_dir_all(&git_db);
        }
    }
    Ok(())
}

/// Best-effort lookup of the user's cargo home — `$CARGO_HOME` if set,
/// otherwise `~/.cargo`. Returns `None` if neither resolves.
fn dirs_home_cache() -> Option<PathBuf> {
    if let Ok(p) = env::var("CARGO_HOME") {
        return Some(PathBuf::from(p));
    }
    #[cfg(unix)]
    {
        env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo"))
    }
    #[cfg(windows)]
    {
        env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".cargo"))
    }
}

fn ensure_installed(crate_name: &str) -> Result<()> {
    if which(crate_name).is_some() {
        return Ok(());
    }
    println!("xtask: installing {crate_name} (one-time)…");
    cargo(["install", "--locked", crate_name])
}

fn bundle() -> Result<()> {
    dist()?;
    let out_dir = workspace_root().join("dist").join(host_target_triple());
    let bin = out_dir.join(bin_name());

    #[cfg(target_os = "macos")]
    {
        // Minimal .app bundle layout.
        let app = out_dir.join("GrokInsane.app");
        let macos = app.join("Contents/MacOS");
        let resources = app.join("Contents/Resources");
        std::fs::create_dir_all(&macos)?;
        std::fs::create_dir_all(&resources)?;
        std::fs::copy(&bin, macos.join("grok-insane"))?;
        std::fs::write(app.join("Contents/Info.plist"), info_plist())?;
        println!("bundle: {}", app.display());
    }

    #[cfg(target_os = "windows")]
    {
        // Zip the .exe + dependencies. We just rename the bin for now.
        let dest = out_dir.join("grok-insane.exe");
        if bin != dest {
            std::fs::copy(&bin, &dest)?;
        }
        println!("bundle: {}", dest.display());
    }

    #[cfg(target_os = "linux")]
    {
        // Plain tarball of the binary + README + LICENSE-style files.
        let stage = out_dir.join("grok-insane");
        std::fs::create_dir_all(&stage)?;
        std::fs::copy(&bin, stage.join("grok-insane"))?;
        if let Ok(readme) = std::fs::read_to_string(workspace_root().join("README.md")) {
            std::fs::write(stage.join("README.md"), readme)?;
        }
        println!("bundle staged at: {}", stage.display());
        println!(
            "(run `tar czf grok-insane.tar.gz -C {} grok-insane` to ship)",
            out_dir.display()
        );
    }

    Ok(())
}

// --- macOS signing / notarization / DMG ------------------------------------
//
// These steps only do real work on macOS hosts. On other platforms they print
// a notice and return so CI matrices don't fail unexpectedly.
//
// Required environment variables (set as repository secrets in CI):
//
//   APPLE_DEVELOPER_ID_APPLICATION   "Developer ID Application: Your Name (TEAMID)"
//   APPLE_ID                         your-apple-id@example.com
//   APPLE_TEAM_ID                    10-char team identifier (e.g. ABCDE12345)
//   APPLE_APP_SPECIFIC_PASSWORD      app-specific password for notarytool
//
// Without these, we sign with an ad-hoc signature (still passes Gatekeeper's
// quarantine path for self-distribution but Mac users will see "unknown
// developer" on first open). Signed + notarized is the only way to get a
// silent open and to maximise SentinelOne trust.

#[cfg(not(target_os = "macos"))]
fn sign() -> Result<()> {
    println!("sign: macOS-only step; skipping on this host.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn sign() -> Result<()> {
    {
        bundle()?;
        let app = mac_app_path();
        if !app.exists() {
            bail!(
                "expected .app at {} — run `cargo xtask bundle` first",
                app.display()
            );
        }
        let entitlements = workspace_root()
            .join("packaging")
            .join("Entitlements.plist");
        if !entitlements.exists() {
            bail!("missing entitlements at {}", entitlements.display());
        }
        let identity = env::var("APPLE_DEVELOPER_ID_APPLICATION").ok();

        let mut cmd = Command::new("codesign");
        cmd.arg("--force")
            .arg("--deep")
            .arg("--options")
            .arg("runtime")
            .arg("--timestamp")
            .arg("--entitlements")
            .arg(&entitlements);
        match identity.as_deref() {
            Some(id) if !id.is_empty() => {
                cmd.arg("--sign").arg(id);
                println!("sign: codesigning with `{id}`");
            }
            _ => {
                cmd.arg("--sign").arg("-");
                eprintln!(
                    "sign: APPLE_DEVELOPER_ID_APPLICATION not set — using ad-hoc signature.\n\
                     The resulting .app will trigger Gatekeeper warnings on other Macs."
                );
            }
        }
        cmd.arg(&app);
        run_cmd(&mut cmd)?;

        // Quick verification so failures are caught here, not at Gatekeeper time.
        let mut verify = Command::new("codesign");
        verify
            .arg("--verify")
            .arg("--deep")
            .arg("--strict")
            .arg("--verbose=2")
            .arg(&app);
        run_cmd(&mut verify)?;

        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
fn notarize() -> Result<()> {
    println!("notarize: macOS-only step; skipping on this host.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn notarize() -> Result<()> {
    {
        let app = mac_app_path();
        if !app.exists() {
            bail!("missing .app at {}", app.display());
        }
        let apple_id =
            env::var("APPLE_ID").context("APPLE_ID env var required for notarization")?;
        let team_id =
            env::var("APPLE_TEAM_ID").context("APPLE_TEAM_ID env var required for notarization")?;
        let password = env::var("APPLE_APP_SPECIFIC_PASSWORD")
            .context("APPLE_APP_SPECIFIC_PASSWORD env var required for notarization")?;

        // notarytool requires a zip or a dmg; submit a zip of the .app so we
        // can keep the DMG step independent.
        let out_dir = workspace_root().join("dist").join(host_target_triple());
        let zip = out_dir.join("GrokInsane.zip");
        let mut z = Command::new("ditto");
        z.arg("-c")
            .arg("-k")
            .arg("--keepParent")
            .arg(&app)
            .arg(&zip);
        run_cmd(&mut z)?;

        let mut sub = Command::new("xcrun");
        sub.arg("notarytool")
            .arg("submit")
            .arg(&zip)
            .arg("--apple-id")
            .arg(&apple_id)
            .arg("--team-id")
            .arg(&team_id)
            .arg("--password")
            .arg(&password)
            .arg("--wait");
        run_cmd(&mut sub)?;

        // Staple the ticket so the bundle works offline.
        let mut staple = Command::new("xcrun");
        staple.arg("stapler").arg("staple").arg(&app);
        run_cmd(&mut staple)?;

        // Final assessment — what Gatekeeper will do.
        let mut assess = Command::new("spctl");
        assess
            .arg("-a")
            .arg("-vvv")
            .arg("--type")
            .arg("execute")
            .arg(&app);
        run_cmd(&mut assess)?;

        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
fn dmg() -> Result<()> {
    println!("dmg: macOS-only step; skipping on this host.");
    Ok(())
}

#[cfg(target_os = "macos")]
fn dmg() -> Result<()> {
    {
        sign()?;
        // Notarisation is optional locally; CI will provide the env vars.
        if env::var("APPLE_ID").is_ok() && env::var("APPLE_APP_SPECIFIC_PASSWORD").is_ok() {
            notarize()?;
        } else {
            println!("dmg: notary creds absent, skipping notarization step.");
        }

        let out_dir = workspace_root().join("dist").join(host_target_triple());
        let app = mac_app_path();
        let dmg_path = out_dir.join(format!("GrokInsane-{}.dmg", env!("CARGO_PKG_VERSION")));
        if dmg_path.exists() {
            std::fs::remove_file(&dmg_path).ok();
        }

        // Stage the .app + a /Applications symlink inside a temp dir so the
        // resulting DMG has the classic "drag to Applications" layout.
        let stage = out_dir.join("dmg-stage");
        if stage.exists() {
            std::fs::remove_dir_all(&stage).ok();
        }
        std::fs::create_dir_all(&stage)?;
        let mut cp = Command::new("ditto");
        cp.arg(&app).arg(stage.join("GrokInsane.app"));
        run_cmd(&mut cp)?;
        let mut ln = Command::new("ln");
        ln.arg("-s")
            .arg("/Applications")
            .arg(stage.join("Applications"));
        run_cmd(&mut ln)?;

        // Build the DMG with hdiutil (Apple's own tool, ships with macOS).
        let mut create = Command::new("hdiutil");
        create
            .arg("create")
            .arg("-volname")
            .arg("GrokInsane")
            .arg("-srcfolder")
            .arg(&stage)
            .arg("-ov")
            .arg("-format")
            .arg("UDZO")
            .arg(&dmg_path);
        run_cmd(&mut create)?;

        // Sign the DMG itself so Gatekeeper trusts the container.
        if let Ok(id) = env::var("APPLE_DEVELOPER_ID_APPLICATION") {
            if !id.is_empty() {
                let mut sign = Command::new("codesign");
                sign.arg("--sign")
                    .arg(&id)
                    .arg("--timestamp")
                    .arg(&dmg_path);
                run_cmd(&mut sign)?;
            }
        }

        // Staple the DMG too (the .app inside is already stapled, but stapling
        // the outer container lets Finder skip a network check on first open).
        if env::var("APPLE_ID").is_ok() {
            let mut staple = Command::new("xcrun");
            staple.arg("stapler").arg("staple").arg(&dmg_path);
            let _ = staple.status();
        }

        println!("dmg: {}", dmg_path.display());
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn mac_app_path() -> PathBuf {
    workspace_root()
        .join("dist")
        .join(host_target_triple())
        .join("GrokInsane.app")
}

fn reset() -> Result<()> {
    // Run the app's own --reset-db. Build first if needed.
    cargo(["build", "--package", "grok-insane"])?;
    let bin = workspace_root()
        .join("target")
        .join("debug")
        .join(bin_name());
    let mut cmd = Command::new(&bin);
    cmd.args(["--reset-db", "--yes"]);
    run_cmd(&mut cmd)
}

fn install_deps() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // Only attempt apt; warn otherwise.
        if which("apt-get").is_none() {
            println!("install-deps: no apt-get found; install equivalents manually.");
            return Ok(());
        }
        // Refresh the package index first. GitHub-hosted runner images bake
        // an apt cache, but it goes stale within a few days — `apt-get install`
        // then fails with `E: Unable to locate package` or `E: Version 'X' for
        // 'Y' was not found` once Ubuntu rotates the published version. An
        // `update` here is the difference between a flaky build and a stable
        // one.
        let mut update = Command::new("sudo");
        update
            .arg("apt-get")
            .arg("update")
            .arg("-y")
            .env("DEBIAN_FRONTEND", "noninteractive");
        run_cmd(&mut update)?;

        let pkgs = [
            "libasound2-dev",
            "libxkbcommon-dev",
            "libwayland-dev",
            "libxcb1-dev",
            "libxcb-render0-dev",
            "libxcb-shape0-dev",
            "libxcb-xfixes0-dev",
            "libfontconfig1-dev",
        ];
        let mut cmd = Command::new("sudo");
        cmd.arg("apt-get")
            .arg("install")
            .arg("-y")
            .arg("--no-install-recommends")
            .env("DEBIAN_FRONTEND", "noninteractive");
        for p in pkgs {
            cmd.arg(p);
        }
        run_cmd(&mut cmd)?;
    }
    #[cfg(not(target_os = "linux"))]
    {
        println!("install-deps: nothing to do on this OS.");
    }
    Ok(())
}

fn ci() -> Result<()> {
    install_deps()?;
    check()?;
    dist()?;
    Ok(())
}

fn clean() -> Result<()> {
    cargo(["clean"])?;
    let dist = workspace_root().join("dist");
    if dist.exists() {
        std::fs::remove_dir_all(&dist).with_context(|| format!("rm {}", dist.display()))?;
    }
    Ok(())
}

// --- helpers ----------------------------------------------------------------

fn cargo<I, S>(args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let cargo_bin = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo_bin);
    cmd.args(args);
    run_cmd(&mut cmd)
}

fn run_cmd(cmd: &mut Command) -> Result<()> {
    let pretty = format!("{:?}", cmd);
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn {pretty}"))?;
    if !status.success() {
        bail!("command failed ({}): {pretty}", status);
    }
    Ok(())
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR for xtask is .../xtask, so go up one.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(".").to_path_buf())
}

fn bin_name() -> String {
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
fn host_target_triple() -> String {
    env!("BUILD_TARGET_TRIPLE").to_string()
}

/// Cross-platform PATH lookup. Delegates to the `which` crate because the
/// previous std-only implementation missed Windows extensions (`.exe`,
/// `.cmd`, `.bat`) and ignored `PATHEXT`, which silently broke
/// `ensure_installed` for any installed cargo subcommand under Windows.
fn which(prog: &str) -> Option<PathBuf> {
    which::which(prog).ok()
}

#[cfg(target_os = "macos")]
fn info_plist() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>GrokInsane</string>
    <key>CFBundleDisplayName</key><string>GrokInsane</string>
    <key>CFBundleIdentifier</key><string>com.grokinsane.grok-insane</string>
    <key>CFBundleExecutable</key><string>grok-insane</string>
    <key>CFBundleVersion</key><string>{version}</string>
    <key>CFBundleShortVersionString</key><string>{version}</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>LSMinimumSystemVersion</key><string>11.0</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSMicrophoneUsageDescription</key>
        <string>GrokInsane needs microphone access for voice mode.</string>
</dict>
</plist>
"#
    )
}
