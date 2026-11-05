//! Build-system seam.
//!
//! Building plugin artifacts (running `cargo`, computing dirty fingerprints,
//! rewriting `artifacts/index.json`) is *not* a runtime mechanism: a bare
//! runtime only has to read artifacts that already exist. The runtime
//! therefore exposes this trait and never links a build backend itself; the
//! embedding binary (`cordis-cli`) installs one at start-up
//! (`install_plugin_builder(Box::new(cordis_tooling::CargoBuilder))`).
//!
//! Dependency direction stays acyclic: `cordis-cli` → {`cordis-runtime`,
//! `cordis-tooling`}, `cordis-tooling` → `cordis-runtime`. The runtime has no
//! normal dependency on `cordis-tooling` (only a dev-dependency, so runtime
//! tests can build fixture artifacts).

use crate::core::error::RuntimeError;
use std::path::Path;

/// Outcome of a [`PluginBuilder::prepare`] pass.
///
/// Field-for-field the same shape the build tooling reports
/// (`PrepareArtifactsReport`); kept here so the trait surface does not name a
/// type that lives outside the runtime.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PreparedArtifacts {
    /// `(plugin_path, artifact_sha256)` for every artifact rebuilt this pass.
    pub rebuilt: Vec<(String, String)>,
    /// Plugin paths whose existing artifact was reused as-is.
    pub reused: Vec<String>,
    /// Whether this pass forced a full rebuild.
    pub full_rebuild: bool,
}

/// The one interface to the build system: the runtime only calls, the
/// embedder injects.
pub trait PluginBuilder: Send + Sync {
    /// Rebuild stale artifacts under `fixtures_root` (`full` forces every
    /// plugin through the build path).
    fn prepare(&self, fixtures_root: &Path, full: bool) -> Result<PreparedArtifacts, RuntimeError>;

    /// Rebuild every artifact under `fixtures_root`.
    fn rebuild_all(&self, fixtures_root: &Path) -> Result<Vec<(String, String)>, RuntimeError>;

    /// Rebuild a single plugin subtree.
    ///
    /// `plugin_path` keeps the host's convention: `/` means "all plugins",
    /// `/qq` means just `qq`.
    fn rebuild_plugin(
        &self,
        workspace_root: &Path,
        plugin_path: &str,
    ) -> Result<Vec<(String, String)>, RuntimeError>;
}

static BUILDER: std::sync::OnceLock<Box<dyn PluginBuilder>> = std::sync::OnceLock::new();

/// Install the process-wide build backend. Idempotent: the second install
/// returns `false` and leaves the first builder in place.
pub fn install_plugin_builder(b: Box<dyn PluginBuilder>) -> bool {
    BUILDER.set(b).is_ok()
}

/// The installed build backend, or `None` in a build that has no tooling
/// (e.g. a library-only embedding).
pub fn plugin_builder() -> Option<&'static dyn PluginBuilder> {
    BUILDER.get().map(|b| b.as_ref())
}

/// The uniform error for a path that needs build tooling but has none
/// installed. Text is pinned byte-for-byte by tests.
pub fn missing_builder_error() -> RuntimeError {
    RuntimeError::Invariant {
        message: "no plugin builder installed (this build has no build tooling)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand-in for the embedding binary's builder.
    ///
    /// It has to be defined here rather than imported from `cordis-tooling`:
    /// `cordis-tooling` depends back on `cordis-runtime`, so a dev-dependency
    /// on it links the **non-test** build of this crate, and
    /// `cordis_tooling::CargoBuilder` implements that copy's `PluginBuilder` —
    /// not the `cfg(test)` one this module sees. The integration tests (which
    /// link the normal lib) install the real `CargoBuilder`.
    struct TestBuilder;

    impl PluginBuilder for TestBuilder {
        fn prepare(
            &self,
            fixtures_root: &Path,
            full: bool,
        ) -> Result<PreparedArtifacts, RuntimeError> {
            Ok(PreparedArtifacts {
                rebuilt: vec![(
                    "plug".to_string(),
                    format!("{}:{full}", fixtures_root.display()),
                )],
                reused: Vec::new(),
                full_rebuild: full,
            })
        }

        fn rebuild_all(&self, fixtures_root: &Path) -> Result<Vec<(String, String)>, RuntimeError> {
            Ok(vec![(
                "all".to_string(),
                fixtures_root.display().to_string(),
            )])
        }

        fn rebuild_plugin(
            &self,
            workspace_root: &Path,
            plugin_path: &str,
        ) -> Result<Vec<(String, String)>, RuntimeError> {
            Ok(vec![(
                workspace_root.display().to_string(),
                plugin_path.to_string(),
            )])
        }
    }

    /// One test drives the whole registry: [`BUILDER`] is process-global, so
    /// splitting "uninstalled" / "installed" / "duplicate install" into
    /// separate `#[test]`s would make the assertions order-dependent.
    #[test]
    fn builder_registry_installs_once_and_is_read_back() {
        // Nothing else in the lib test binary installs one, so the registry
        // starts empty here.
        assert!(plugin_builder().is_none());

        assert!(install_plugin_builder(Box::new(TestBuilder)));

        // Read back through the trait object and dispatch every method, so the
        // installed `dyn PluginBuilder` is proven to be the one that was
        // installed (and the three vtable entries are exercised).
        let installed = plugin_builder().expect("installed on the previous line");
        let prepared = installed.prepare(Path::new("fx"), true).unwrap();
        assert_eq!(
            prepared,
            PreparedArtifacts {
                rebuilt: vec![("plug".to_string(), "fx:true".to_string())],
                reused: Vec::new(),
                full_rebuild: true,
            }
        );
        assert_eq!(
            installed.rebuild_all(Path::new("fx")).unwrap(),
            vec![("all".to_string(), "fx".to_string())]
        );
        assert_eq!(
            installed.rebuild_plugin(Path::new("ws"), "/qq").unwrap(),
            vec![("ws".to_string(), "/qq".to_string())]
        );

        // Second install is a no-op and reports `false`.
        assert!(!install_plugin_builder(Box::new(TestBuilder)));
        assert!(plugin_builder().is_some());
    }

    /// The "no tooling in this build" error text, asserted byte-for-byte
    /// (message field *and* rendered form).
    #[test]
    fn missing_builder_error_is_the_pinned_invariant() {
        let pinned = "no plugin builder installed (this build has no build tooling)";
        let err = missing_builder_error();
        let carries_pinned_message =
            matches!(&err, RuntimeError::Invariant { message } if message == pinned);
        assert!(carries_pinned_message);
        assert_eq!(
            err.to_string(),
            format!("internal invariant broken: {pinned}")
        );
    }
}
