use crate::core::error::RuntimeError;
use crate::core::models::RUST_PLUGIN_ENTRY_SYMBOL;
use crate::plugin::abi::RustPluginApiV2;
use libloading::Library;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// A `dlclose` policy for a [`LoadedDylibApi`] handle.
///
/// The *safe default* is [`UnloadPolicy::Retain`]: an unload only happens when
/// the owner has positive evidence that no plugin thread can still execute the
/// dylib. `plugin::invoke` is the owner that has that evidence (the plugin's
/// exported thread contract) and flips the policy explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnloadPolicy {
    /// All plugin threads are joined (or there never were any): `dlclose` the
    /// mapping on drop.
    Release,
    /// Keep the mapping mapped. Used for handles whose lifetime is the process
    /// (artifact doc extraction) and for handles that are still shared with
    /// another `dlopen` reference.
    Retain,
    /// Keep the mapping **and** report that the plugin did not provide a
    /// joinable thread contract (or its threads refused to exit). This is the
    /// historical-plugin fallback: correct but leaky.
    RetainWithDiagnostic,
}

/// Process-wide list of mappings kept alive after their handle is dropped.
///
/// Only handles that could not prove "no live plugin thread" land here. A
/// plugin that exports the thread contract and answers `0` live threads is
/// released normally, so a reload of a local plugin no longer leaks a mapping.
static RETAINED_DYLIBS: OnceLock<Mutex<Vec<Library>>> = OnceLock::new();

/// Number of handles released with `dlclose` (tests assert a clean join really
/// unloads instead of being retained).
static RELEASED_DYLIBS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Last `RetainWithDiagnostic` message per dylib path. Keyed so a test can
/// assert the exact text for *its* dylib without racing on stderr or on other
/// plugins' diagnostics.
static LAST_RETENTION_DIAGNOSTICS: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();

/// Access the retained-handle list with the same tolerant lock policy as
/// `plugin::invoke`: a poisoned lock cannot corrupt a keep-alive registry, and
/// a panic here would abort the drop.
fn retained_dylibs_lock() -> std::sync::MutexGuard<'static, Vec<Library>> {
    RETAINED_DYLIBS
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

fn last_retention_diagnostics_lock() -> std::sync::MutexGuard<'static, HashMap<PathBuf, String>> {
    LAST_RETENTION_DIAGNOSTICS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Number of dylib handles retained instead of released.
///
/// Exposed for tests: a growing count proves that dropping a `LoadedDylibApi`
/// kept its library mapped rather than `dlclose`ing it.
pub fn retained_dylib_count() -> usize {
    retained_dylibs_lock().len()
}

/// Number of dylib handles actually released (`dlclose`).
pub fn released_dylib_count() -> usize {
    RELEASED_DYLIBS.load(std::sync::atomic::Ordering::SeqCst)
}

/// The diagnostic emitted when a plugin dylib is kept mapped because its
/// threads could not be joined.
///
/// Single source of truth: the drop path prints exactly this string, and
/// `plugin::dynamic::tests::retention_diagnostic_is_byte_exact` asserts it
/// byte for byte.
pub fn retention_diagnostic(path: &Path) -> String {
    format!(
        "plugin {} unloaded without joined threads; retained",
        path.display()
    )
}

/// Last retention diagnostic emitted for `path`, if any.
pub fn last_retention_diagnostic_for(path: &Path) -> Option<String> {
    last_retention_diagnostics_lock().get(path).cloned()
}

/// A plugin's exported thread contract (`_cordis_join_service_threads` /
/// `_cordis_live_service_threads`), resolved from its dylib.
///
/// Presence of this contract is the opt-in that lets the runtime unload a
/// plugin at all; a dylib built without it (any pre-SDK-contract artifact) is
/// kept mapped for the lifetime of the process.
#[derive(Clone, Copy)]
pub struct PluginThreadContract {
    join: unsafe extern "C" fn(u64) -> u64,
    live: unsafe extern "C" fn() -> u64,
}

impl PluginThreadContract {
    /// Threads spawned by this plugin image that have not exited.
    pub fn live(&self) -> u64 {
        // SAFETY: the symbol is defined by the SDK macro with this exact
        // signature and the owning `Library` is mapped for the call.
        unsafe { (self.live)() }
    }

    /// Set the plugin-wide shutdown flag and join every registered thread.
    /// Returns the number of threads still running after `timeout_ms`.
    pub fn join(&self, timeout_ms: u64) -> u64 {
        // SAFETY: same as `live`; the SDK entry point only touches SDK state.
        unsafe { (self.join)(timeout_ms) }
    }
}

pub struct LoadedDylibApi {
    /// Never released in place when `unload` is a retain policy: `Drop` moves
    /// it into [`RETAINED_DYLIBS`]. `ManuallyDrop` is what makes that possible
    /// — `Library` is not `Copy` and cannot be moved out of a type that
    /// implements `Drop`.
    _lib: ManuallyDrop<Library>,
    api_ptr: *const RustPluginApiV2,
    path: PathBuf,
    /// `(dev, ino)` of the mapped artifact. Two handles with the same identity
    /// map the same image, so replacing one with the other may release the old
    /// `dlopen` reference without unmapping the code.
    identity: Option<(u64, u64)>,
    unload: UnloadPolicy,
}

impl Drop for LoadedDylibApi {
    fn drop(&mut self) {
        // SAFETY: `_lib` is moved out exactly once, here, while `self` is being
        // dropped; nothing reads it afterwards.
        let lib = unsafe { ManuallyDrop::take(&mut self._lib) };
        match self.unload {
            UnloadPolicy::Release => {
                RELEASED_DYLIBS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                drop(lib);
            }
            UnloadPolicy::Retain => retained_dylibs_lock().push(lib),
            UnloadPolicy::RetainWithDiagnostic => {
                let message = retention_diagnostic(&self.path);
                last_retention_diagnostics_lock().insert(self.path.clone(), message.clone());
                eprintln!("{message}");
                retained_dylibs_lock().push(lib);
            }
        }
    }
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

        let api_ptr = reject_null_api_ptr(*symbol, path)?;

        Ok(Self {
            _lib: ManuallyDrop::new(lib),
            api_ptr,
            path: path.to_path_buf(),
            identity: file_identity(path),
            // Safe default: a handle nobody vouched for stays mapped.
            unload: UnloadPolicy::Retain,
        })
    }

    /// Path this handle was opened from (diagnostics).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Identity of the mapped file, when the platform can provide one.
    ///
    /// `None` means "unknown": callers must treat two handles as mapping
    /// different images and go through the join-before-unload path.
    pub fn identity(&self) -> Option<(u64, u64)> {
        self.identity
    }

    /// Resolve the plugin's joinable-thread contract, if it exports one.
    ///
    /// Both symbols must be present. A dylib built before the contract existed
    /// has neither, and is treated as legacy (retained with a diagnostic).
    pub fn thread_contract(&self) -> Option<PluginThreadContract> {
        type LiveFn = unsafe extern "C" fn() -> u64;
        type JoinFn = unsafe extern "C" fn(u64) -> u64;
        let live = unsafe { self.lib().get::<LiveFn>(b"_cordis_live_service_threads\0") }
            .ok()
            .map(|symbol| *symbol)?;
        let join = unsafe { self.lib().get::<JoinFn>(b"_cordis_join_service_threads\0") }
            .ok()
            .map(|symbol| *symbol)?;
        Some(PluginThreadContract { join, live })
    }

    /// Whether the plugin explicitly permits unloading its dylib.
    ///
    /// The contract is `_cordis_allows_unload() -> bool`, emitted by the SDK's
    /// `allow_plugin_unload!` opt-in after a plugin migrates **every**
    /// background thread to `ServiceThreads::spawn`. A missing symbol (any
    /// artifact predating the opt-in) is a hard *no*: a raw
    /// `std::thread::spawn` thread cannot be observed by the runtime, so
    /// "zero live threads" is not evidence that `dlclose` is safe.
    pub fn allows_unload(&self) -> bool {
        type AllowsFn = unsafe extern "C" fn() -> bool;
        // SAFETY: the symbol, when present, is defined by the SDK macro with
        // this exact signature; `self` owns the mapping for the call.
        unsafe { self.lib().get::<AllowsFn>(b"_cordis_allows_unload\0") }
            .ok()
            .is_some_and(|allows| unsafe { allows() })
    }

    /// Permit `dlclose` on drop: all plugin threads have been joined.
    pub fn release_on_drop(&mut self) {
        self.unload = UnloadPolicy::Release;
    }

    /// Retain on drop and emit the "unloaded without joined threads"
    /// diagnostic.
    pub fn retain_with_diagnostic(&mut self) {
        self.unload = UnloadPolicy::RetainWithDiagnostic;
    }

    /// Override the drop policy (used when a handle is replaced by a newer
    /// generation that keeps the image mapped anyway).
    pub fn set_unload_policy(&mut self, policy: UnloadPolicy) {
        self.unload = policy;
    }

    pub fn api(&self) -> &RustPluginApiV2 {
        unsafe { &*self.api_ptr }
    }

    pub fn lib(&self) -> &Library {
        &self._lib
    }
}

/// Reject a null API pointer resolved from a dylib's entry symbol.
///
/// Extracted from [`LoadedDylibApi::open`] so the null-pointer fail-closed arm
/// is directly unit-testable: producing a genuinely null `cordis_plugin_api_rust_v2`
/// symbol from a real dylib is impractical (the SDK's `export_plugin_api!` macro
/// always stamps a non-null static), so the guard is exercised by passing a null
/// pointer to this helper. The `path` is threaded through only to tag the error.
fn reject_null_api_ptr(
    api_ptr: *const RustPluginApiV2,
    path: &Path,
) -> Result<*const RustPluginApiV2, RuntimeError> {
    if api_ptr.is_null() {
        return Err(RuntimeError::Io {
            path: path.to_path_buf(),
            message: "symbol resolved to null pointer".to_string(),
        });
    }
    Ok(api_ptr)
}

/// `(device, inode)` identity of a file, used to decide whether two `dlopen`
/// handles refer to the same mapped image.
#[cfg(unix)]
fn file_identity(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

/// Non-Unix platforms have no stable inode identity here; `None` forces the
/// conservative join-then-unload path.
#[cfg(not(unix))]
fn file_identity(_path: &Path) -> Option<(u64, u64)> {
    None
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

    // --- reject_null_api_ptr -------------------------------------------------

    // A null pointer must map to the tagged Io error. This is the fail-closed
    // arm inside `open` that a real SDK-built dylib can never hit (the export
    // macro stamps a non-null static), so it is exercised via the extracted
    // helper directly.
    #[test]
    fn reject_null_api_ptr_rejects_null() {
        let err = reject_null_api_ptr(std::ptr::null(), Path::new("/some/plugin.dylib"))
            .expect_err("null pointer must be rejected");
        assert!(
            matches!(&err, RuntimeError::Io { path, message } if path == &PathBuf::from("/some/plugin.dylib") && message == "symbol resolved to null pointer"),
            "wrong variant: {err:?}"
        );
    }

    // A non-null pointer passes through unchanged (the Ok arm). Use the address
    // of a real stack value so the pointer is genuinely non-null (and not a
    // hand-rolled dangling constant). It is never dereferenced.
    #[test]
    fn reject_null_api_ptr_passes_non_null() {
        let marker: u8 = 0;
        let dummy = std::ptr::addr_of!(marker) as *const RustPluginApiV2;
        let out = reject_null_api_ptr(dummy, Path::new("/some/plugin.dylib"))
            .expect("non-null pointer must pass");
        assert_eq!(out, dummy);
    }

    // --- LoadedDylibApi::open error branches ---------------------------------

    #[test]
    fn open_missing_file_is_io_error() {
        // LoadedDylibApi has no Debug impl, so use matches! rather than unwrap_err().
        let result = LoadedDylibApi::open(Path::new("/no/such/lib.dylib"));
        assert!(
            matches!(&result, Err(RuntimeError::Io { message, .. }) if message.contains("load dylib failed")),
            "expected Io load failure for missing file"
        );
    }

    /// `Some(path)` when `path` dlopens on this host, else `None`.
    ///
    /// Returning an `Option` lets callers gate a whole test body with
    /// `for`-over-`Option`, which has no not-taken arm — an `if`/`if let` gate
    /// leaves its closing brace as a permanently-uncovered region on hosts
    /// where the probe succeeds.
    fn dlopen_probe(path: &Path) -> Option<&Path> {
        // SAFETY: probe dlopen only; the handle is dropped before returning, and
        // only the path (not the handle) escapes.
        unsafe { Library::new(path) }.ok().map(|_lib| path)
    }

    /// A platform library that `dlopen`s but carries no
    /// `cordis_plugin_api_rust_v2` symbol. Tried in order so the symbol-lookup
    /// branch is exercised on macOS (dyld shared cache) and on Linux/CI alike;
    /// `dlopen_probe` filters to whichever actually loads on this host.
    fn symbolless_system_library() -> Option<&'static Path> {
        [
            "/usr/lib/libSystem.B.dylib",
            "/lib/x86_64-linux-gnu/libc.so.6",
            "/lib/aarch64-linux-gnu/libc.so.6",
            "/usr/lib/x86_64-linux-gnu/libc.so.6",
            "/usr/lib/aarch64-linux-gnu/libc.so.6",
            "/lib64/libc.so.6",
        ]
        .into_iter()
        .find_map(|c| dlopen_probe(Path::new(c)))
    }

    #[test]
    fn open_dylib_without_entry_symbol_reports_symbol_lookup() {
        for path in symbolless_system_library().into_iter() {
            let result = LoadedDylibApi::open(path);
            let is_symbol_lookup = matches!(&result, Err(RuntimeError::Io { message, .. }) if message.contains("symbol lookup failed"));
            assert!(
                is_symbol_lookup,
                "expected symbol lookup failure for {path:?}"
            );
        }
    }

    // --- LoadedDylibApi happy path (real fixture dylib) ----------------------

    fn host_native_fixture_dylib() -> Option<PathBuf> {
        // Fixture .dylib files match the local host arch (arm64 on this repo's
        // dev machine); .so files are x86_64-linux. `DLL_EXTENSION` yields
        // exactly the host's dylib extension ("dylib" on macOS, "so" on Linux),
        // so a single expression selects the matching fixture with no
        // platform-dead arms. On any host without a matching fixture (or a
        // different extension), `.exists()` returns None below.
        let candidate = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/artifacts")
            .join(format!("expr.{}", std::env::consts::DLL_EXTENSION));
        candidate.exists().then_some(candidate)
    }

    #[test]
    fn open_real_fixture_dylib_exposes_api_and_lib() {
        // Gate the body on fixture availability + loadability. A cross-arch
        // fixture (e.g. arm64 dylib on x86_64) legitimately fails dlopen; treat
        // it as a skip rather than a failure. `for`-over-`Option`/flat_map has
        // no not-taken arm, so no brace stays permanently uncovered here.
        for loaded in host_native_fixture_dylib()
            .into_iter()
            .flat_map(|path| LoadedDylibApi::open(&path))
        {
            // api() dereferences the resolved symbol; docs() must return JSON.
            let api = loaded.api();
            let docs_json = (api.docs)().payload;
            assert!(docs_json.contains("plugin_path"), "docs: {docs_json}");
            // lib() returns the owned Library handle.
            let _lib: &Library = loaded.lib();
        }
    }

    // The default drop policy is `Retain`: a handle nobody vouched for must
    // keep its code mapped, because a plugin's detached threads may still be
    // executing it. This is the safety net behind the 2026-11-26 crash:
    // `qq::qq_serve` was declared as a plain (non-Task) node, the invoke path
    // dropped its dylib, and the HTTP poller thread jumped into the unmapped
    // code (SIGSEGV at address 0). Asserted two ways: the handle is retained
    // instead of released, and a function pointer into the dylib still runs
    // after the handle is gone.
    #[test]
    fn dropping_a_loaded_dylib_keeps_its_code_mapped() {
        for loaded in host_native_fixture_dylib()
            .into_iter()
            .flat_map(|path| LoadedDylibApi::open(&path))
        {
            // Copy the vtable's docs entry point out before dropping the handle
            // that owns the mapping: it points into the dylib's code section.
            let docs = loaded.api().docs;
            let before = retained_dylib_count();
            drop(loaded);
            assert!(
                retained_dylib_count() > before,
                "dropping a LoadedDylibApi must retain the handle instead of dlclose'ing it"
            );
            let docs_json = (docs)().payload;
            assert!(
                docs_json.contains("plugin_path"),
                "dylib code must stay callable after its handle is dropped: {docs_json}"
            );
        }
    }

    // The diagnostic is part of the contract with operators and with the
    // integration tests, so its text is asserted byte for byte.
    #[test]
    fn retention_diagnostic_is_byte_exact() {
        assert_eq!(
            retention_diagnostic(Path::new("/plugins/qq.so")),
            "plugin /plugins/qq.so unloaded without joined threads; retained"
        );
    }

    // A plugin that cannot prove "no live thread" is retained *with* the
    // diagnostic; the message is recorded so tests do not race on stderr.
    #[test]
    #[serial_test::serial]
    fn retain_with_diagnostic_records_the_message_and_keeps_the_mapping() {
        for mut loaded in host_native_fixture_dylib()
            .into_iter()
            .flat_map(|path| LoadedDylibApi::open(&path))
        {
            let loaded_path = loaded.path().to_path_buf();
            let expected = retention_diagnostic(&loaded_path);
            let before = retained_dylib_count();
            loaded.retain_with_diagnostic();
            drop(loaded);
            assert_eq!(last_retention_diagnostic_for(&loaded_path), Some(expected));
            assert!(
                retained_dylib_count() > before,
                "a plugin without a thread contract must stay mapped"
            );
        }
    }

    // Once every plugin thread is joined, the handle may `dlclose`: this is
    // the "no leak after clean unload" half of the fix.
    #[test]
    fn release_on_drop_really_closes_the_mapping() {
        for mut loaded in host_native_fixture_dylib()
            .into_iter()
            .flat_map(|path| LoadedDylibApi::open(&path))
        {
            let released_before = released_dylib_count();
            let retained_before = retained_dylib_count();
            // `set_unload_policy` is the path the invoke layer uses after a
            // successful thread join; `release_on_drop` is its convenience
            // wrapper.
            loaded.set_unload_policy(UnloadPolicy::Release);
            loaded.release_on_drop();
            drop(loaded);
            assert_eq!(released_dylib_count(), released_before + 1);
            assert_eq!(retained_dylib_count(), retained_before);
        }
    }

    // A *pre-contract* dylib is not probed here on purpose: the fixture
    // artifacts are rebuilt by `prepare-artifacts` with the current SDK, which
    // stamps the joinable-thread contract into every plugin, so they are
    // contract-bearing. The legacy fallback (probe answers `None` → retain with
    // a diagnostic) is covered end-to-end by
    // `invoke_coverage::a_legacy_dylib_without_the_thread_contract_is_retained_with_a_diagnostic`,
    // which builds a deliberately contract-less dylib.
}
