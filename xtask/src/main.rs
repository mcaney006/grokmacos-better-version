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
        "sign" => sign(),
        "notarize" => notarize(),
        "dmg" => dmg(),
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
    println!("  bundle        per-OS bundle in dist/  (.app / .exe / staged dir)");
    println!("  sign          macOS: codesign --options runtime --timestamp with Developer ID");
    println!("  notarize      macOS: xcrun notarytool submit --wait + stapler");
    println!("  dmg           macOS: bundle -> sign -> notarize -> staple -> .dmg in dist/");
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
