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
    crate::commands::bundle()?;
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

    // Two codesign invocation shapes:
    //
    //   (a) Real Developer ID identity present →
    //         --options runtime --timestamp --entitlements <plist>
    //         --sign "<Developer ID Application: ...>"
    //       The full hardened-runtime + secure-timestamp + entitlements
    //       set Apple expects for notarization.
    //
    //   (b) Ad-hoc (no identity) →
    //         --sign -
    //       Just enough to make the bundle openable. We deliberately
    //       DROP `--timestamp` because codesign on Xcode 15+ errors
    //       out with `--timestamp` + `--sign -` (the timestamp service
    //       requires a real cert chain). The previous code passed
    //       `--timestamp` unconditionally; macOS release jobs without
    //       Apple secrets failed silently after the keychain step.
    //       We also drop `--options runtime` + `--entitlements` for
    //       ad-hoc because the hardened-runtime ticket only matters
    //       under a real signature; layering it on an ad-hoc sig
    //       gains us nothing and increases the chance of a future
    //       codesign quirk failing the build.
    //       `--deep` is kept on both: the .app contains the binary
    //       at Contents/MacOS/grok-insane and we want it signed too.
    let mut cmd = Command::new("codesign");
    cmd.arg("--force").arg("--deep");
    match identity.as_deref() {
        Some(id) if !id.is_empty() => {
            cmd.arg("--options")
                .arg("runtime")
                .arg("--timestamp")
                .arg("--entitlements")
                .arg(&entitlements)
                .arg("--sign")
                .arg(id);
            println!("sign: codesigning with `{id}` (hardened runtime + timestamp)");
        }
        _ => {
            cmd.arg("--sign").arg("-");
            eprintln!(
                "sign: APPLE_DEVELOPER_ID_APPLICATION not set — using ad-hoc signature.\n\
                 The resulting .app will trigger Gatekeeper warnings on other Macs.\n\
                 Hardened-runtime / timestamp / entitlements skipped because they\n\
                 require a real Developer ID and codesign errors when combined\n\
                 with `--sign -` on Xcode 15+."
            );
        }
    }
    cmd.arg(&app);
    run_cmd(&mut cmd)?;

    // `--verify --strict` against an ad-hoc bundle is fine — strict
    // checks the signature is internally consistent, not that it
    // chains to a trusted CA. spctl is the assess-trust step and is
    // only run as part of `notarize()`.
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
    sign()?;
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

    if env::var("APPLE_ID").is_ok() {
        let mut staple = Command::new("xcrun");
        staple.arg("stapler").arg("staple").arg(&dmg_path);
        let _ = staple.status();
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
