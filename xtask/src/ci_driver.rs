//! CI driver — the "thin GHA YAML over a Rust orchestrator" half of the
//! pipeline. Workflow files no longer contain step bodies; each `run:` is
//! `cargo xtask ci --stage <name>` and the truth about what a stage does
//! lives in `CiStage::run` below.
//!
//! Local invocation:
//!   cargo xtask ci                  # default pipeline for this host
//!   cargo xtask ci --stage fmt      # run one stage
//!
//! CI invocation: see .github/workflows/ci.yml.

use crate::commands::{
    audit, dist, ensure_installed, git_commit_timestamp, hygiene, install_deps, reproducible, sbom,
};
use crate::{bin_name, cargo, host_target_triple, run_cmd, which, workspace_root};
use anyhow::{bail, Context, Result};
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Clone, Copy)]
pub(crate) enum CiStage {
    InstallDeps,
    Fmt,
    Clippy,
    Test,
    Build,
    Audit,
    Sbom,
    Hygiene,
    Reproducible,
    // Release-only stages.
    PackageTarball,
    PackageZip,
    SignArtifacts,
    ComputeSourceDateEpoch,
    Nextest,
}

impl CiStage {
    fn name(self) -> &'static str {
        match self {
            CiStage::InstallDeps => "install-deps",
            CiStage::Fmt => "fmt",
            CiStage::Clippy => "clippy",
            CiStage::Test => "test",
            CiStage::Build => "build",
            CiStage::Audit => "audit",
            CiStage::Sbom => "sbom",
            CiStage::Hygiene => "hygiene",
            CiStage::Reproducible => "reproducible",
            CiStage::PackageTarball => "package-tarball",
            CiStage::PackageZip => "package-zip",
            CiStage::SignArtifacts => "sign-artifacts",
            CiStage::ComputeSourceDateEpoch => "compute-source-date-epoch",
            CiStage::Nextest => "nextest",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "install-deps" => CiStage::InstallDeps,
            "fmt" => CiStage::Fmt,
            "clippy" => CiStage::Clippy,
            "test" => CiStage::Test,
            "build" => CiStage::Build,
            "audit" => CiStage::Audit,
            "sbom" => CiStage::Sbom,
            "hygiene" => CiStage::Hygiene,
            "reproducible" => CiStage::Reproducible,
            "package-tarball" => CiStage::PackageTarball,
            "package-zip" => CiStage::PackageZip,
            "sign-artifacts" => CiStage::SignArtifacts,
            "compute-source-date-epoch" | "compute-sde" => CiStage::ComputeSourceDateEpoch,
            "nextest" => CiStage::Nextest,
            other => bail!(
                "unknown ci stage `{other}`. valid stages: install-deps, fmt, \
                 clippy, test, build, audit, sbom, hygiene, reproducible, \
                 package-tarball, package-zip, sign-artifacts, \
                 compute-source-date-epoch, nextest"
            ),
        })
    }

    fn run(self) -> Result<()> {
        match self {
            CiStage::InstallDeps => install_deps(),
            CiStage::Fmt => cargo(["fmt", "--all", "--", "--check"]),
            CiStage::Clippy => cargo([
                "clippy",
                "--all-targets",
                "--workspace",
                "--",
                "-D",
                "warnings",
            ]),
            CiStage::Test => cargo(["test", "--workspace"]),
            CiStage::Build => dist(),
            CiStage::Audit => audit(),
            CiStage::Sbom => sbom(),
            CiStage::Hygiene => hygiene(),
            CiStage::Reproducible => reproducible(),
            CiStage::PackageTarball => package_tarball(),
            CiStage::PackageZip => package_zip(),
            CiStage::SignArtifacts => sign_artifacts(),
            CiStage::ComputeSourceDateEpoch => compute_source_date_epoch(),
            CiStage::Nextest => nextest(),
        }
    }

    fn default_pipeline() -> Vec<CiStage> {
        let mut v = Vec::new();
        if cfg!(target_os = "linux") {
            v.push(CiStage::InstallDeps);
        }
        v.extend([CiStage::Fmt, CiStage::Clippy, CiStage::Test, CiStage::Build]);
        v
    }
}

pub(crate) fn ci(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        None => {
            for stage in CiStage::default_pipeline() {
                run_stage(stage)?;
            }
            Ok(())
        }
        Some("--stage") => {
            let name = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("--stage requires a value"))?;
            let stage = CiStage::parse(name)?;
            run_stage(stage)
        }
        Some(other) => bail!("unknown ci flag `{other}`. usage: cargo xtask ci [--stage NAME]"),
    }
}

fn run_stage(stage: CiStage) -> Result<()> {
    let in_ci = env::var("GITHUB_ACTIONS").as_deref() == Ok("true");
    if in_ci {
        println!("::group::xtask ci: {}", stage.name());
    } else {
        println!("\n==> ci stage: {}", stage.name());
    }
    let started = Instant::now();
    let result = stage.run();
    let elapsed = started.elapsed();
    if in_ci {
        println!("::endgroup::");
    }
    append_step_summary(stage, &result, elapsed)?;
    result
}

fn append_step_summary(
    stage: CiStage,
    result: &Result<()>,
    elapsed: std::time::Duration,
) -> Result<()> {
    let Ok(summary_path) = env::var("GITHUB_STEP_SUMMARY") else {
        return Ok(());
    };
    let path = PathBuf::from(summary_path);
    let need_header = !path.exists()
        || std::fs::metadata(&path)
            .map(|m| m.len() == 0)
            .unwrap_or(true);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if need_header {
        writeln!(
            f,
            "## CI stages\n\n| Stage | Status | Duration | Target |\n| --- | --- | --- | --- |"
        )?;
    }
    let icon = if result.is_ok() { "✅" } else { "❌" };
    let status = if result.is_ok() { "pass" } else { "fail" };
    writeln!(
        f,
        "| `{}` | {icon} {} | {:.2}s | `{}` |",
        stage.name(),
        status,
        elapsed.as_secs_f32(),
        env!("BUILD_TARGET_TRIPLE"),
    )?;
    Ok(())
}

// =========================================================================
// Release packaging + signing stages
// =========================================================================

pub(crate) fn compute_source_date_epoch() -> Result<()> {
    let sde = git_commit_timestamp().context("read commit timestamp via `git log`")?;
    println!("SOURCE_DATE_EPOCH={sde}");
    if let Ok(path) = env::var("GITHUB_ENV") {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {path} for append"))?;
        writeln!(f, "SOURCE_DATE_EPOCH={sde}")?;
    }
    Ok(())
}

pub(crate) fn package_tarball() -> Result<()> {
    let triple = host_target_triple();
    let out_dir = workspace_root().join("dist").join(&triple);
    std::fs::create_dir_all(&out_dir).context("create dist dir")?;

    let bin = out_dir.join(bin_name());
    if !bin.exists() {
        bail!(
            "expected {} (run `cargo xtask ci --stage build` first)",
            bin.display()
        );
    }
    if let Ok(readme) = std::fs::read_to_string(workspace_root().join("README.md")) {
        let _ = std::fs::write(out_dir.join("README.md"), readme);
    }

    let ref_name = env::var("GITHUB_REF_NAME").unwrap_or_else(|_| "dev".into());
    let tar_path = workspace_root()
        .join("dist")
        .join(format!("grok-insane-{ref_name}-{triple}.tar.gz"));

    let mut cmd = Command::new("tar");
    cmd.current_dir(workspace_root().join("dist"))
        .arg("-czf")
        .arg(&tar_path)
        .arg(&triple);
    run_cmd(&mut cmd)?;
    println!("tarball: {}", tar_path.display());
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn package_zip() -> Result<()> {
    println!("package-zip: not on Windows, skipping.");
    Ok(())
}

#[cfg(target_os = "windows")]
pub(crate) fn package_zip() -> Result<()> {
    let triple = host_target_triple();
    let out_dir = workspace_root().join("dist").join(&triple);
    std::fs::create_dir_all(&out_dir).context("create dist dir")?;
    let exe = out_dir.join("grok-insane.exe");
    if !exe.exists() {
        bail!(
            "expected {} (run `cargo xtask ci --stage build` first)",
            exe.display()
        );
    }
    let ref_name = env::var("GITHUB_REF_NAME").unwrap_or_else(|_| "dev".into());
    let zip_path = workspace_root()
        .join("dist")
        .join(format!("grok-insane-{ref_name}-{triple}.zip"));

    let mut cmd = Command::new("powershell");
    cmd.arg("-NoProfile").arg("-Command").arg(format!(
        "Compress-Archive -Path '{}' -DestinationPath '{}' -Force",
        exe.display(),
        zip_path.display()
    ));
    run_cmd(&mut cmd)?;
    println!("zip: {}", zip_path.display());
    Ok(())
}

pub(crate) fn sign_artifacts() -> Result<()> {
    if which("cosign").is_none() {
        println!("sign-artifacts: cosign not on PATH; skipping (set up via sigstore/cosign-installer in CI)");
        return Ok(());
    }
    let triple = host_target_triple();
    let dist_root = workspace_root().join("dist");
    let triple_dir = dist_root.join(&triple);

    let mut signed = 0u32;
    for scan in [&dist_root, &triple_dir] {
        if !scan.exists() {
            continue;
        }
        signed += sign_files_in(scan)?;
    }
    if signed == 0 {
        bail!(
            "sign-artifacts: no signable files in {} or {} (did build/package stages run?)",
            dist_root.display(),
            triple_dir.display()
        );
    }
    println!("sign-artifacts: signed {signed} file(s)");
    Ok(())
}

fn sign_files_in(dir: &Path) -> Result<u32> {
    let signable_exts = ["dmg", "tar.gz", "zip", "json"];
    let mut count = 0u32;
    for entry in std::fs::read_dir(dir).context("read dist dir")? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if name.ends_with(".cosign-bundle") || name == "xtask-metrics.jsonl" {
            continue;
        }
        let signable = signable_exts
            .iter()
            .any(|ext| name.ends_with(&format!(".{ext}")));
        if !signable {
            continue;
        }
        let bundle = path.with_extension(format!(
            "{}.cosign-bundle",
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("bundle")
        ));
        println!("::group::cosign sign-blob {}", path.display());
        let mut cmd = Command::new("cosign");
        cmd.arg("sign-blob")
            .arg("--yes")
            .arg("--bundle")
            .arg(&bundle)
            .arg(&path);
        run_cmd(&mut cmd)?;
        println!("::endgroup::");
        count += 1;
    }
    Ok(count)
}

/// `cargo-nextest` runner. Used by CI for faster, more isolated test
/// execution. Falls back to a clear error if nextest isn't installed —
/// installation is handled in the workflow via `taiki-e/install-action`.
fn nextest() -> Result<()> {
    if which("cargo-nextest").is_none() {
        ensure_installed("cargo-nextest")?;
    }
    cargo(["nextest", "run", "--workspace", "--locked"])
}
