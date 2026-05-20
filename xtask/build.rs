//! Capture the rustc target triple at compile time so `host_target_triple`
//! is a constant lookup instead of a `rustc -vV` subprocess on every xtask
//! invocation. Cargo sets the `TARGET` env var for build scripts; we
//! re-export it as `BUILD_TARGET_TRIPLE` for the main crate.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown-target".into());
    println!("cargo:rustc-env=BUILD_TARGET_TRIPLE={target}");
    println!("cargo:rerun-if-env-changed=TARGET");
}
