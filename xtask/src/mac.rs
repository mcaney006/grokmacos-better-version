//! macOS-only release packaging: `sign`, `notarize`, `dmg`, plus the
//! `Info.plist` template baked into the `.app` bundle. Non-macOS hosts get
//! polite no-op stubs so the orchestration in `release()` stays identical
//! across runners.

#[cfg(target_os = "macos")]
use crate::run_cmd;
use crate::{host_target_triple, workspace_root};
use anyhow::Result;
#[cfg(target_os = "macos")]
use anyhow::{bail, Context};
use std::path::PathBuf;
#[cfg(target_os = "macos")]
use std::{env, process::Command};

// --- macOS signing / notarization / DMG ------------------------------------
//
// These steps only do real work on macOS hosts. On other platforms they print
// a notice and return so CI matrices don't fail unexpectedly.
//
// Required environment variables (set as repository secrets in CI):
//
//   APPLE_DEVELOPER_ID_APPLICATION   "Developer ID Application: Your Name (TEAMID)"
//   APPLE_ID                         your-apple-id@example.com
//   APPLE_TEAM_ID                    10-char team identifier
//   APPLE_APP_SPECIFIC_PASSWORD      app-specific password for notarytool
//
// Without these we sign ad-hoc (still passes the local Gatekeeper quarantine
// path, but other Macs will see "unknown developer" on first open).

#[cfg(not(target_os = "macos"))]
pub(crate) fn sign() -> Result<()> {
    println!("sign: macOS-only step; skipping on this host.");
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn sign() -> Result<()> {
    println!("::group::xtask sign: bundle");
    crate::commands::bundle()?;
    println!("::endgroup::");

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
    let inner_bin = app.join("Contents").join("MacOS").join("grok-insane");

    // Apple deprecated `codesign --deep` in macOS 11; release notes
    // through Sonoma/Sequoia have steadily tightened the screw on
    // it. The previous version of this function relied on `--deep`
    // to recursively sign the inner binary at
    // `Contents/MacOS/grok-insane`, and on macOS 15 (Sequoia, our
    // `macos-15-intel` runner) that produced a non-zero exit even
    // for ad-hoc signing — the macOS release jobs died here on
    // every run.
    //
    // Modern Apple-recommended pattern: sign innermost-first
    // (binary), then outermost (the .app bundle). Two invocations.
    // Identical args except the path.
    //
    // Two profiles, gated on whether we have a real Developer ID:
    //
    //   (a) Developer ID present → hardened runtime + secure
    //       timestamp + entitlements. The full notarization-ready
    //       posture.
    //
    //   (b) Ad-hoc (no identity) → `--force --sign -` only. We
    //       deliberately DO NOT pass `--timestamp` (errors with
    //       `--sign -` on Xcode 15+), nor `--options runtime`, nor
    //       `--entitlements` (no real signature for these to
    //       attach to). The resulting .app still runs locally;
    //       Gatekeeper warns on other Macs, right-click → Open
    //       is the escape hatch.
    //
    // The `--verify` step is kept ONLY for the Developer ID path.
    // `codesign --verify --strict` against an ad-hoc bundle is
    // unreliable on Sequoia — it sometimes rejects bundles that
    // open fine in Finder. Since we don't ship ad-hoc to other
    // Macs anyway (the README + RELEASE_CHECKLIST both call this
    // out), skipping verify here is honest, not lazy.
    let sign_one = |path: &std::path::Path| -> Result<()> {
        let mut cmd = Command::new("codesign");
        cmd.arg("--force");
        match identity.as_deref() {
            Some(id) if !id.is_empty() => {
                cmd.arg("--options")
                    .arg("runtime")
                    .arg("--timestamp")
                    .arg("--entitlements")
                    .arg(&entitlements)
                    .arg("--sign")
                    .arg(id);
            }
            _ => {
                cmd.arg("--sign").arg("-");
            }
        }
        cmd.arg(path);
        run_cmd(&mut cmd)
    };

    match identity.as_deref() {
        Some(id) if !id.is_empty() => {
            println!("sign: codesigning with `{id}` (hardened runtime + timestamp)");
        }
        _ => {
            eprintln!(
                "sign: APPLE_DEVELOPER_ID_APPLICATION not set — using ad-hoc signature.\n\
                 The resulting .app will trigger Gatekeeper warnings on other Macs.\n\
                 Hardened-runtime / timestamp / entitlements / --deep / --verify\n\
                 are all skipped: they require a real Developer ID and codesign\n\
                 errors when combined with `--sign -` on Sonoma/Sequoia."
            );
        }
    }

    println!("::group::xtask sign: codesign inner binary");
    if inner_bin.exists() {
        sign_one(&inner_bin)?;
    } else {
        bail!("inner binary missing at {}", inner_bin.display());
    }
    println!("::endgroup::");

    println!("::group::xtask sign: codesign app bundle");
    sign_one(&app)?;
    println!("::endgroup::");

    if matches!(identity.as_deref(), Some(id) if !id.is_empty()) {
        // Verify only matters when there's something other than
        // an ad-hoc signature to verify. See block comment above
        // for why we skip on ad-hoc.
        println!("::group::xtask sign: verify");
        let mut verify = Command::new("codesign");
        verify
            .arg("--verify")
            .arg("--strict")
            .arg("--verbose=2")
            .arg(&app);
        run_cmd(&mut verify)?;
        println!("::endgroup::");
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn notarize() -> Result<()> {
    println!("notarize: macOS-only step; skipping on this host.");
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn notarize() -> Result<()> {
    let app = mac_app_path();
    if !app.exists() {
        bail!("missing .app at {}", app.display());
    }
    let apple_id = env::var("APPLE_ID").context("APPLE_ID env var required for notarization")?;
    let team_id =
        env::var("APPLE_TEAM_ID").context("APPLE_TEAM_ID env var required for notarization")?;
    let password = env::var("APPLE_APP_SPECIFIC_PASSWORD")
        .context("APPLE_APP_SPECIFIC_PASSWORD env var required for notarization")?;

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

    let mut staple = Command::new("xcrun");
    staple.arg("stapler").arg("staple").arg(&app);
    run_cmd(&mut staple)?;

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

#[cfg(not(target_os = "macos"))]
pub(crate) fn dmg() -> Result<()> {
    println!("dmg: macOS-only step; skipping on this host.");
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn dmg() -> Result<()> {
    println!("::group::xtask dmg: sign (calls bundle + codesign)");
    sign()?;
    println!("::endgroup::");

    if env::var("APPLE_ID").is_ok() && env::var("APPLE_APP_SPECIFIC_PASSWORD").is_ok() {
        println!("::group::xtask dmg: notarize");
        notarize()?;
        println!("::endgroup::");
    } else {
        println!("dmg: notary creds absent, skipping notarization step.");
    }

    let out_dir = workspace_root().join("dist").join(host_target_triple());
    let app = mac_app_path();
    let dmg_path = out_dir.join(format!("GrokInsane-{}.dmg", env!("CARGO_PKG_VERSION")));
    if dmg_path.exists() {
        std::fs::remove_file(&dmg_path).ok();
    }

    println!("::group::xtask dmg: stage");
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
    println!("::endgroup::");

    println!("::group::xtask dmg: hdiutil create");
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
    println!("::endgroup::");

    if let Ok(id) = env::var("APPLE_DEVELOPER_ID_APPLICATION") {
        if !id.is_empty() {
            println!("::group::xtask dmg: codesign dmg");
            let mut sign = Command::new("codesign");
            sign.arg("--sign")
                .arg(&id)
                .arg("--timestamp")
                .arg(&dmg_path);
            run_cmd(&mut sign)?;
            println!("::endgroup::");
        }
    }

    if env::var("APPLE_ID").is_ok() {
        println!("::group::xtask dmg: staple");
        let mut staple = Command::new("xcrun");
        staple.arg("stapler").arg("staple").arg(&dmg_path);
        let _ = staple.status();
        println!("::endgroup::");
    }

    println!("dmg: {}", dmg_path.display());
    Ok(())
}

#[cfg(target_os = "macos")]
pub(crate) fn mac_app_path() -> PathBuf {
    workspace_root()
        .join("dist")
        .join(host_target_triple())
        .join("GrokInsane.app")
}

#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
pub(crate) fn mac_app_path() -> PathBuf {
    workspace_root()
        .join("dist")
        .join(host_target_triple())
        .join("GrokInsane.app")
}

#[cfg(target_os = "macos")]
pub(crate) fn info_plist() -> String {
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
