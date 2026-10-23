//! Test support shared by this crate's integration tests.
//!
//! `cordis-tooling` was extracted from `cordis-runtime`'s `plugin/tooling.rs`,
//! so its integration tests only need the two fixture helpers that file used:
//! the canonical repo `fixtures/` root (with artifacts prepared) and the
//! host-native dylib probe.

use cordis_tooling::ensure_fixture_artifacts;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[allow(dead_code)]
static FIXTURES_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// The pre-built `.so` artifacts under `fixtures/artifacts/` are all
/// `x86_64-unknown-linux-gnu` ELF binaries (their target triple is recorded
/// in `index.json`). On any other host the loader marks them
/// `Unavailable(AbiMismatch)`, so tests that need real dylib loading must skip
/// declaratively.
#[allow(dead_code)]
pub fn linux_dylib_artifacts_available() -> bool {
    cordis_plugin_sdk::CORDIS_TARGET == "x86_64-unknown-linux-gnu"
}

#[allow(dead_code)]
pub fn fixtures_root() -> PathBuf {
    FIXTURES_ROOT
        .get_or_init(|| {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures")
                .canonicalize()
                .expect("fixtures must exist");
            ensure_fixture_artifacts(&root).expect("fixture artifacts should be ready");
            root
        })
        .clone()
}
