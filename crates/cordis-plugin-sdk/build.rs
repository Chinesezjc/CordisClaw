//! P2-13: emit `CORDIS_RUSTC_VERSION` and `CORDIS_TARGET` at build time
//! so the SDK's `AbiFingerprint::current_build()` (see lib.rs) can fill
//! them in from `env!(...)` — plugins no longer have to hard-code
//! `rustc_version = "1.85.1"` / `target_triple = "x86_64-unknown-linux-gnu"`
//! in every `AbiFingerprint`.
//!
//! `RUSTC` is always set by cargo. `TARGET` is set for the crate being
//! built (matches the plugin's own target when the plugin depends on
//! `cordis-plugin-sdk`, which is what we want).

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let rustc_version = Command::new(&rustc)
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                String::from_utf8(out.stdout).ok()
            } else {
                None
            }
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown-rustc".to_string());

    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown-target".to_string());

    println!("cargo:rustc-env=CORDIS_RUSTC_VERSION={rustc_version}");
    println!("cargo:rustc-env=CORDIS_TARGET={target}");
}
