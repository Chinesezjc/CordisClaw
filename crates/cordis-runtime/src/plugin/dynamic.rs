use crate::core::error::RuntimeError;
use crate::core::models::RUST_PLUGIN_ENTRY_SYMBOL;
use crate::plugin::abi::RustPluginApiV2;
use libloading::Library;
use std::path::{Path, PathBuf};

pub struct LoadedDylibApi {
    _lib: Library,
    api_ptr: *const RustPluginApiV2,
}

impl LoadedDylibApi {
    pub fn open(path: &Path) -> Result<Self, RuntimeError> {
        let lib = unsafe { Library::new(path) }.map_err(|e| RuntimeError::Io {
            path: path.to_path_buf(),
            message: format!("load dylib failed: {e}"),
        })?;

        let symbol_name = format!("{RUST_PLUGIN_ENTRY_SYMBOL}\0");
        let symbol = unsafe { lib.get::<*const RustPluginApiV2>(symbol_name.as_bytes()) }.map_err(
            |e| RuntimeError::Io {
                path: path.to_path_buf(),
                message: format!("symbol lookup failed ({RUST_PLUGIN_ENTRY_SYMBOL}): {e}"),
            },
        )?;

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
}

pub fn is_dylib_path(path: &Path) -> bool {
    match path.extension().and_then(|x| x.to_str()) {
        Some("so") | Some("dylib") | Some("dll") => true,
        _ => false,
    }
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
