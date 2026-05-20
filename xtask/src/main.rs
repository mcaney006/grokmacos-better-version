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
//! * `bundle`         — per-OS bundle (`.app` / `.exe` zip / `.tar.gz`) in `dist/`
//! * `reset`          — wipe local app data via `grok-insane --reset-db --yes`
//! * `install-deps`   — install OS dev libs (Linux apt-get only)
//! * `ci`             — what CI runs: install-deps + check + dist
//! * `clean`          — `cargo clean` + remove `dist/`
//! * `help`           — print usage
//!
//! Everything is pure Rust + `std::process::Command` so there is exactly zero
//! shell in this repo.

use anyhow::{bail, Context, Result};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    if let Err(err) = run() {
        eprintln!("xtask: {err:#}");
        std::process::exit(1);
    }
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
        "reset" => reset(),
        "install-deps" => install_deps(),
        "ci" => ci(),
        "clean" => clean(),
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
    println!("  bundle        per-OS bundle in dist/  (.app / zip / tar.gz)");
    println!("  reset         wipe local app data");
    println!("  install-deps  install OS dev libs (Linux apt only)");
    println!("  ci            install-deps + check + dist");
    println!("  clean         cargo clean + remove dist/");
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
    Ok(())
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
        cmd.arg("apt-get").arg("install").arg("-y");
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

fn host_target_triple() -> String {
    // `rustc -vV` prints the host triple in a `host: ...` line.
    let out = Command::new(env::var("RUSTC").unwrap_or_else(|_| "rustc".into()))
        .arg("-vV")
        .output();
    if let Ok(out) = out {
        if let Ok(s) = std::str::from_utf8(&out.stdout) {
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("host:") {
                    return rest.trim().to_string();
                }
            }
        }
    }
    "unknown-target".into()
}

#[cfg(target_os = "linux")]
fn which(prog: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path).find_map(|p| {
        let candidate = p.join(prog);
        if candidate.is_file() {
            Some(candidate)
        } else {
            None
        }
    })
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
