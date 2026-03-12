use cordis_runtime::plugin::tooling::ensure_fixture_artifacts;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static FIXTURES_ROOT: OnceLock<PathBuf> = OnceLock::new();

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
