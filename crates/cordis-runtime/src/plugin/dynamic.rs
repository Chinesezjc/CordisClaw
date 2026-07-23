use crate::core::error::RuntimeError;
use crate::core::models::RUST_PLUGIN_ENTRY_SYMBOL;
use crate::plugin::abi::RustPluginApiV2;
use libloading::Library;
use std::path::{Path, PathBuf};

pub struct LoadedDylibApi {
    _lib: Library,
    api_ptr: *const RustPluginApiV2,
}

// Safety: _lib is owned and stays in memory; api_ptr is valid as long as _lib is alive.
unsafe impl Send for LoadedDylibApi {}

impl LoadedDylibApi {
    pub fn open(path: &Path) -> Result<Self, RuntimeError> {
        let lib = unsafe { Library::new(path) }.map_err(|e| RuntimeError::Io {
            path: path.to_path_buf(),
            message: format!("load dylib failed: {e}"),
        })?;

        let symbol_name = format!("{RUST_PLUGIN_ENTRY_SYMBOL}\0");
        let symbol =
            unsafe { lib.get::<*const RustPluginApiV2>(symbol_name.as_bytes()) }.map_err(|e| {
                RuntimeError::Io {
                    path: path.to_path_buf(),
                    message: format!("symbol lookup failed ({RUST_PLUGIN_ENTRY_SYMBOL}): {e}"),
                }
            })?;

        let api_ptr = *symbol;
        if api_ptr.is_null() {
            return Err(RuntimeError::Io {
                path: path.to_path_buf(),
                message: "symbol resolved to null pointer".to_string(),
            });
        }

        Ok(Self { _lib: lib, api_ptr })
    }

    pub fn api(&self) -> &RustPluginApiV2 {
        unsafe { &*self.api_ptr }
    }

    pub fn lib(&self) -> &Library {
        &self._lib
    }
}

pub fn is_dylib_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|x| x.to_str()),
        Some("so" | "dylib" | "dll")
    )
}

pub fn sidecar_json_path(path: &Path) -> PathBuf {
    let mut out = path.to_path_buf();
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .map(|x| format!("{x}.json"))
        .unwrap_or_else(|| "json".to_string());
    out.set_extension(ext);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_dylib_path -------------------------------------------------------

    #[test]
    fn is_dylib_path_recognises_platform_extensions() {
        assert!(is_dylib_path(Path::new("libfoo.so")));
        assert!(is_dylib_path(Path::new("libfoo.dylib")));
        assert!(is_dylib_path(Path::new("foo.dll")));
    }

    #[test]
    fn is_dylib_path_rejects_non_dylib() {
        assert!(!is_dylib_path(Path::new("foo.json")));
        assert!(!is_dylib_path(Path::new("foo")));
        assert!(!is_dylib_path(Path::new("foo.SO"))); // case-sensitive match
    }

    // --- sidecar_json_path ---------------------------------------------------

    #[test]
    fn sidecar_json_path_appends_json_to_extension() {
        assert_eq!(
            sidecar_json_path(Path::new("/a/b/libfoo.so")),
            PathBuf::from("/a/b/libfoo.so.json")
        );
        assert_eq!(
            sidecar_json_path(Path::new("/a/b/libfoo.dylib")),
            PathBuf::from("/a/b/libfoo.dylib.json")
        );
    }

    #[test]
    fn sidecar_json_path_no_extension_gets_json() {
        assert_eq!(
            sidecar_json_path(Path::new("/a/b/plain")),
            PathBuf::from("/a/b/plain.json")
        );
    }

    // --- LoadedDylibApi::open error branches ---------------------------------

    #[test]
    fn open_missing_file_is_io_error() {
        // LoadedDylibApi has no Debug impl, so match rather than unwrap_err().
        match LoadedDylibApi::open(Path::new("/no/such/lib.dylib")) {
            Err(RuntimeError::Io { message, .. }) => {
                assert!(message.contains("load dylib failed"), "{message}");
            }
            Err(other) => panic!("expected Io load failure, got {other:?}"),
            Ok(_) => panic!("expected open to fail for missing file"),
        }
    }

    #[test]
    fn open_dylib_without_entry_symbol_reports_symbol_lookup() {
        // libSystem is loadable on macOS via the dyld shared cache but has no
        // `cordis_plugin_api_rust_v2` symbol, exercising the symbol-lookup
        // failure branch. Skip elsewhere.
        let sys = Path::new("/usr/lib/libSystem.B.dylib");
        if unsafe { Library::new(sys) }.is_err() {
            eprintln!("[skip] libSystem.B.dylib not loadable on this host");
            return;
        }
        match LoadedDylibApi::open(sys) {
            Err(RuntimeError::Io { message, .. }) => {
                assert!(message.contains("symbol lookup failed"), "{message}");
            }
            Err(other) => panic!("expected symbol lookup failure, got {other:?}"),
            Ok(_) => panic!("expected symbol lookup to fail for libSystem"),
        }
    }

    // --- LoadedDylibApi happy path (real fixture dylib) ----------------------

    fn host_native_fixture_dylib() -> Option<PathBuf> {
        // Fixture .dylib files match the local host arch (arm64 on this repo's
        // dev machine); .so files are x86_64-linux. Pick the matching one.
        let ext = if cfg!(target_os = "macos") {
            "dylib"
        } else if cfg!(target_os = "linux") {
            "so"
        } else {
            return None;
        };
        let candidate = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/artifacts")
            .join(format!("expr.{ext}"));
        candidate.exists().then_some(candidate)
    }

    #[test]
    fn open_real_fixture_dylib_exposes_api_and_lib() {
        let Some(path) = host_native_fixture_dylib() else {
            eprintln!("[skip] no host-native fixture dylib available");
            return;
        };
        let loaded = match LoadedDylibApi::open(&path) {
            Ok(l) => l,
            Err(err) => {
                // A cross-arch fixture (e.g. arm64 dylib on x86_64) legitimately
                // fails dlopen; treat as skip rather than failure.
                eprintln!("[skip] fixture dylib not loadable on this host: {err:?}");
                return;
            }
        };
        // api() dereferences the resolved symbol; docs() must return JSON.
        let api = loaded.api();
        let docs_json = (api.docs)().payload;
        assert!(docs_json.contains("plugin_path"), "docs: {docs_json}");
        // lib() returns the owned Library handle.
        let _lib: &Library = loaded.lib();
    }
}
