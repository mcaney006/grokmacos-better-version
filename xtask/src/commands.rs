//! Routine `cargo xtask` commands — check/fmt/lint/test/dev/dist/bundle and
//! the supply-chain + reproducibility suite (audit/sbom/doctor/preflight/
//! hygiene/reproducible/release). All commands are pure Rust + Command.

use crate::{bin_name, cargo, host_target_triple, run_cmd, which, workspace_root};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

pub(crate) fn check() -> Result<()> {
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

pub(crate) fn fmt() -> Result<()> {
    cargo(["fmt", "--all"])
}

pub(crate) fn lint() -> Result<()> {
    cargo([
        "clippy",
        "--all-targets",
        "--workspace",
        "--",
        "-D",
        "warnings",
    ])
}

pub(crate) fn test() -> Result<()> {
    cargo(["test", "--workspace"])
}

pub(crate) fn dev(args: &[String]) -> Result<()> {
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

pub(crate) fn dist() -> Result<()> {
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
    // table on every linker.
    strip_binary(&dest).ok();

    let hash = sha256_file(&dest).with_context(|| format!("hash {}", dest.display()))?;
    let sums = out_dir.join("SHA256SUMS");
    std::fs::write(&sums, format!("{hash}  {}\n", bin_name))
        .with_context(|| format!("write {}", sums.display()))?;
    println!("hash: {} -> {}", bin_name, hash);

    Ok(())
}

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
        if which("llvm-strip").is_some() {
            let mut cmd = Command::new("llvm-strip");
            cmd.arg(path);
            run_cmd(&mut cmd).ok();
        }
    }
    let _ = path;
    Ok(())
}

pub(crate) fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

pub(crate) fn audit() -> Result<()> {
    ensure_installed("cargo-audit")?;
    ensure_installed("cargo-deny")?;
    // `cargo audit` and `cargo deny` see different graphs:
    //   * `audit` scans Cargo.lock and is feature-agnostic — every crate
    //     that exists in the lockfile counts, including those behind
    //     optional features that aren't enabled in the default build.
    //   * `deny` scans the actual dep graph for the requested feature set.
    //
    // The fastembed-gated `--features rag` brings in three advisory-bearing
    // transitives (number_prefix, paste, lru) that NEVER reach a default
    // build but DO appear in Cargo.lock, so audit flags them. We ignore
    // them at the audit level (they're warnings, not vulnerabilities) and
    // do NOT ignore them at the deny level (so a future default-feature
    // pull-in surfaces immediately).
    cargo([
        "audit",
        "--deny",
        "warnings",
        "--ignore",
        "RUSTSEC-2024-0384",
        "--ignore",
        "RUSTSEC-2024-0320",
        "--ignore",
        "RUSTSEC-2025-0141",
        // ---- lockfile-only warnings (behind --features rag) ----
        "--ignore",
        "RUSTSEC-2024-0436", // paste unmaintained (via tokenizers, rav1e, image)
        "--ignore",
        "RUSTSEC-2025-0119", // number_prefix unmaintained (via indicatif → hf-hub)
        "--ignore",
        "RUSTSEC-2026-0002", // lru unsound (via tantivy)
    ])?;
    cargo(["deny", "check", "advisories", "bans", "sources", "licenses"])?;
    Ok(())
}

pub(crate) fn sbom() -> Result<()> {
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

/// `cargo xtask doctor [--strict]`. Read-only environment audit. In strict
/// mode, missing OPTIONAL supply-chain tools (cargo-audit, cargo-deny,
/// cargo-cyclonedx, cargo-machete, cosign) flip from advisory to fatal —
/// useful for release-bench machines that must be able to produce a signed,
/// SBOM-bearing artifact in one shot.
pub(crate) fn doctor(args: &[String]) -> Result<()> {
    let strict = args.iter().any(|a| a == "--strict");

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
    let required_files = [
        "Cargo.toml",
        "Cargo.lock",
        "rust-toolchain.toml",
        "deny.toml",
        ".gitattributes",
        ".github/workflows/ci.yml",
        ".github/workflows/release.yml",
        ".github/workflows/audit.yml",
    ];
    let mut missing_files: Vec<&str> = Vec::new();
    for f in required_files {
        let p = root.join(f);
        let ok = p.exists();
        println!("  {} {}", if ok { "✓" } else { "✗" }, f);
        if !ok {
            missing_files.push(f);
        }
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
    let mut missing_platform: Vec<&str> = Vec::new();
    for t in tools {
        let found = which(t).is_some();
        println!("  {} {}", if found { "✓" } else { "✗" }, t);
        if !found {
            missing_platform.push(t);
        }
    }

    println!("\n== optional ==");
    let optional = [
        "cargo-audit",
        "cargo-deny",
        "cargo-cyclonedx",
        "cargo-machete",
        "cosign",
    ];
    let mut missing_optional: Vec<&str> = Vec::new();
    for t in optional {
        let found = which(t).is_some();
        println!(
            "  {} {} (install with `cargo install --locked {}`)",
            if found { "✓" } else { "•" },
            t,
            t
        );
        if !found {
            missing_optional.push(t);
        }
    }

    println!("\n== resolution ==");
    let status = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--locked")
        .stdout(std::process::Stdio::null())
        .status();
    let metadata_ok = matches!(status, Ok(ref s) if s.success());
    match status {
        Ok(s) if s.success() => println!("  ✓ cargo metadata --locked OK"),
        Ok(s) => println!("  ✗ cargo metadata --locked failed: {s}"),
        Err(e) => println!("  ✗ could not spawn cargo: {e}"),
    }

    if strict {
        if !missing_files.is_empty() {
            bail!("doctor --strict: missing files: {missing_files:?}");
        }
        if !missing_platform.is_empty() {
            bail!("doctor --strict: missing platform tools: {missing_platform:?}");
        }
        if !missing_optional.is_empty() {
            bail!(
                "doctor --strict: missing optional tools: {missing_optional:?}\n\
                 Install with `cargo install --locked <name>` for each."
            );
        }
        if !metadata_ok {
            bail!("doctor --strict: `cargo metadata --locked` failed");
        }
        println!("\nDone (strict OK).");
    } else {
        println!("\nDone.");
    }
    Ok(())
}

pub(crate) fn preflight() -> Result<()> {
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

pub(crate) fn hygiene() -> Result<()> {
    ensure_installed("cargo-machete")?;
    let mut cmd = Command::new("cargo-machete");
    cmd.current_dir(workspace_root());
    run_cmd(&mut cmd)
}

pub(crate) fn reproducible() -> Result<()> {
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

pub(crate) fn git_commit_timestamp() -> Result<String> {
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

pub(crate) fn release() -> Result<()> {
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
    crate::mac::dmg()?;
    println!(
        "\nRelease pipeline complete. Artefacts in dist/{}/.",
        host_target_triple()
    );
    Ok(())
}

pub(crate) fn clean_deep() -> Result<()> {
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

pub(crate) fn ensure_installed(crate_name: &str) -> Result<()> {
    if which(crate_name).is_some() {
        return Ok(());
    }
    println!("xtask: installing {crate_name} (one-time)…");
    cargo(["install", "--locked", crate_name])
}

pub(crate) fn bundle() -> Result<()> {
    dist()?;
    let out_dir = workspace_root().join("dist").join(host_target_triple());
    let bin = out_dir.join(bin_name());

    #[cfg(target_os = "macos")]
    {
        let app = out_dir.join("GrokInsane.app");
        let macos = app.join("Contents/MacOS");
        let resources = app.join("Contents/Resources");
        std::fs::create_dir_all(&macos)?;
        std::fs::create_dir_all(&resources)?;
        std::fs::copy(&bin, macos.join("grok-insane"))?;
        std::fs::write(app.join("Contents/Info.plist"), crate::mac::info_plist())?;
        println!("bundle: {}", app.display());
    }

    #[cfg(target_os = "windows")]
    {
        let dest = out_dir.join("grok-insane.exe");
        if bin != dest {
            std::fs::copy(&bin, &dest)?;
        }
        println!("bundle: {}", dest.display());
    }

    #[cfg(target_os = "linux")]
    {
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

    let _ = bin;
    let _ = out_dir;
    Ok(())
}

pub(crate) fn reset() -> Result<()> {
    cargo(["build", "--package", "grok-insane"])?;
    let bin = workspace_root()
        .join("target")
        .join("debug")
        .join(bin_name());
    let mut cmd = Command::new(&bin);
    cmd.args(["--reset-db", "--yes"]);
    run_cmd(&mut cmd)
}

pub(crate) fn install_deps() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if which("apt-get").is_none() {
            println!("install-deps: no apt-get found; install equivalents manually.");
            return Ok(());
        }
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

pub(crate) fn clean() -> Result<()> {
    cargo(["clean"])?;
    let dist = workspace_root().join("dist");
    if dist.exists() {
        std::fs::remove_dir_all(&dist).with_context(|| format!("rm {}", dist.display()))?;
    }
    Ok(())
}
