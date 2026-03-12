use cordis_runtime::plugin::tooling::ensure_fixture_artifacts;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static FIXTURES_ROOT: OnceLock<PathBuf> = OnceLock::new();

pub fn fixtures_root() -> PathBuf {
    let root = FIXTURES_ROOT
        .get_or_init(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures")
                .canonicalize()
                .expect("fixtures must exist")
        })
        .clone();
    ensure_fixture_artifacts(&root).expect("fixture artifacts should be ready");
    root
}
