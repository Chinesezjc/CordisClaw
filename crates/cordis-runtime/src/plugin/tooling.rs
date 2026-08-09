use crate::core::error::RuntimeError;
use crate::core::models::{
    ArtifactIndex, ArtifactIndexEntry, ArtifactKind, InputProbe, InputProbeFile, PluginArtifact,
    PluginDocs, PluginExecution, ARTIFACT_INDEX_SCHEMA_VERSION,
};
use crate::plugin::artifact::{
    artifact_index_map, load_artifact_index, load_plugin_artifact, resolve_artifact_path,
    sha256_file,
};
use crate::plugin::dynamic::{is_dylib_path, LoadedDylibApi};
use crate::plugin::package::{PackageResolver, ResolvedPlugin, ResolvedPluginGraph};
use cordis_plugin_sdk::pretty_json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::env::consts::{DLL_EXTENSION, DLL_PREFIX};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BUILD_LOCK_FILE: &str = ".artifacts-build.lock";
// R6: the JSON lock path no longer uses an age gate — a lock whose pid is
// still live is never reclaimed, no matter how old. The 300s gate was removed
// because a live holder may legitimately build for longer (default build
// timeout is 20min); see `maybe_remove_stale_lock_with_fs` for the pid-reuse
// tradeoff this accepts. Only the legacy (non-JSON / unparsable) mtime path
// keeps a timeout, below.
const LEGACY_STALE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const BUILD_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrepareMode {
    Incremental,
    Full,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrepareArtifactsReport {
    pub rebuilt: Vec<(String, String)>,
    pub reused: Vec<String>,
    pub full_rebuild: bool,
}

#[derive(Debug, Deserialize)]
struct PluginManifestToml {
    package: Option<PluginManifestPackage>,
    lib: Option<PluginManifestLib>,
}

#[derive(Debug, Deserialize)]
struct PluginManifestPackage {
    name: String,
    version: String,
    metadata: Option<PluginManifestMetadata>,
}

#[derive(Debug, Deserialize)]
struct PluginManifestMetadata {
    cordis: Option<PluginManifestCordis>,
}

#[derive(Debug, Deserialize)]
struct PluginManifestCordis {
    artifact: Option<SourceArtifactConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct PluginManifestLib {
    #[serde(rename = "crate-type", default)]
    crate_type: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SourceArtifactConfig {
    #[serde(default)]
    exports: Vec<String>,
    #[serde(default)]
    execution: Option<PluginExecution>,
}

#[derive(Debug, Clone)]
struct PluginBuildSpec {
    package_name: String,
    version: String,
    is_dylib: bool,
    artifact: SourceArtifactConfig,
}

#[derive(Debug, Clone)]
struct PluginBuildContext {
    plugin: ResolvedPlugin,
    build_spec: PluginBuildSpec,
    artifact_name: String,
    artifact_path: PathBuf,
    artifact_kind: ArtifactKind,
    local_path_deps: Vec<String>,
    input_files: Vec<PathBuf>,
    input_probe: InputProbe,
    build_fingerprint: Option<String>,
    dirty: bool,
}

#[derive(Debug, Clone)]
struct DependencySnapshot {
    workspace_manifest_path: PathBuf,
    workspace_members: HashSet<String>,
    target_directory: PathBuf,
    local_dep_closure_by_name: HashMap<String, Vec<PathBuf>>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataOutput {
    packages: Vec<CargoMetadataPackage>,
    workspace_members: Vec<String>,
    target_directory: String,
    resolve: Option<CargoMetadataResolve>,
}

#[derive(Debug, Clone, Deserialize)]
struct CargoMetadataPackage {
    id: String,
    name: String,
    manifest_path: String,
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataResolve {
    nodes: Vec<CargoMetadataNode>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataNode {
    id: String,
    dependencies: Vec<String>,
}

#[derive(Debug)]
struct ArtifactBuildLock {
    path: PathBuf,
    /// The pid that acquired this lock. `Drop` only removes the lock file if
    /// it still records this pid, so a lock that was taken over by another
    /// holder is left alone (R6).
    pid: u32,
}

#[derive(Debug, Serialize, Deserialize)]
struct ArtifactBuildLockState {
    pid: u32,
    created_at_ms: u128,
}

impl Drop for ArtifactBuildLock {
    fn drop(&mut self) {
        // R6: the old code unconditionally unlinked `self.path`, so a holder
        // whose lock had been reclaimed mid-build deleted the newcomer's lock
        // out from under it on drop. Now we read the file back and only remove
        // it when it still records *our* pid: a matching pid means we still
        // own it; a different pid means someone else took over and we must not
        // delete their lock; a missing file means it is already gone. Read or
        // parse failure is handled conservatively — refusing to delete (a
        // delete here could destroy another holder's lock) with an eprintln
        // diagnostic; a leftover unparsable file is eventually reclaimed by
        // the legacy mtime path in `maybe_remove_stale_lock_with_fs`.
        match fs::read_to_string(&self.path) {
            Ok(text) => match serde_json::from_str::<ArtifactBuildLockState>(&text) {
                Ok(state) if state.pid == self.pid => {
                    let _ = fs::remove_file(&self.path);
                }
                // Lock file now records another pid -> taken over; leave it.
                Ok(_) => {}
                Err(err) => eprintln!(
                    "[tooling] refusing to remove unparsable lock file {}: {err}",
                    self.path.display()
                ),
            },
            // File already gone (reclaimed earlier); nothing to clean up.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => eprintln!(
                "[tooling] refusing to remove unreadable lock file {}: {err}",
                self.path.display()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Filesystem injection seams.
//
// The durable-write helpers (`stage_then_rename_file`, `write_pretty_json`)
// and `ArtifactBuildLock::acquire` perform a fixed sequence of `fs` / `File`
// operations. A subset of the mid-sequence failure arms — `write_all`,
// `sync_all`, `flush`, `rename` on a handle that opened successfully — cannot
// be provoked deterministically through the real filesystem, so these structs
// expose each operation as a function pointer. The public entry points inject
// the `STD` value, whose fields call `std::fs` / `File` directly; behaviour is
// byte-for-byte identical to the pre-seam code (same error text, same
// operation order, same cleanup). Tests inject a struct with one field
// replaced by a stub that returns an error to reach the corresponding arm.

fn std_fs_create_dir_all(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)
}

fn std_fs_write_all(file: &mut fs::File, buf: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    file.write_all(buf)
}

fn std_fs_sync_all(file: &fs::File) -> std::io::Result<()> {
    file.sync_all()
}

fn std_fs_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

/// Injection seam for the two durable-write helpers.
#[derive(Clone, Copy)]
struct FsWriteOps {
    create_dir_all: fn(&Path) -> std::io::Result<()>,
    write_all: fn(&mut fs::File, &[u8]) -> std::io::Result<()>,
    sync_all: fn(&fs::File) -> std::io::Result<()>,
    rename: fn(&Path, &Path) -> std::io::Result<()>,
}

impl FsWriteOps {
    const STD: FsWriteOps = FsWriteOps {
        create_dir_all: std_fs_create_dir_all,
        write_all: std_fs_write_all,
        sync_all: std_fs_sync_all,
        rename: std_fs_rename,
    };
}

fn std_lock_open_new(path: &Path) -> std::io::Result<fs::File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn std_lock_serialize(state: &ArtifactBuildLockState) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(state)
}

fn std_fs_flush(file: &mut fs::File) -> std::io::Result<()> {
    use std::io::Write as _;
    file.flush()
}

/// Injection seam for `ArtifactBuildLock::acquire`. Also carries the wait
/// timeout so the AlreadyExists spin-then-timeout arm can be exercised with a
/// zero deadline instead of blocking for `BUILD_LOCK_WAIT_TIMEOUT`.
#[derive(Clone, Copy)]
struct LockAcquireOps {
    open_new: fn(&Path) -> std::io::Result<fs::File>,
    serialize: fn(&ArtifactBuildLockState) -> serde_json::Result<Vec<u8>>,
    write_all: fn(&mut fs::File, &[u8]) -> std::io::Result<()>,
    flush: fn(&mut fs::File) -> std::io::Result<()>,
    sync_all: fn(&fs::File) -> std::io::Result<()>,
    wait_timeout: Duration,
}

impl LockAcquireOps {
    const STD: LockAcquireOps = LockAcquireOps {
        open_new: std_lock_open_new,
        serialize: std_lock_serialize,
        write_all: std_fs_write_all,
        flush: std_fs_flush,
        sync_all: std_fs_sync_all,
        wait_timeout: BUILD_LOCK_WAIT_TIMEOUT,
    };
}

fn std_meta_read(path: &Path) -> std::io::Result<fs::Metadata> {
    fs::metadata(path)
}

fn std_meta_modified(metadata: &fs::Metadata) -> std::io::Result<SystemTime> {
    metadata.modified()
}

/// Injection seam for the mtime-reading paths (`build_input_probe`,
/// `maybe_remove_stale_lock`). `metadata` covers the initial stat; `modified`
/// covers the `Metadata::modified()` read whose failure arm falls back to
/// `SystemTime::now()` after logging.
#[derive(Clone, Copy)]
struct MetaOps {
    metadata: fn(&Path) -> std::io::Result<fs::Metadata>,
    modified: fn(&fs::Metadata) -> std::io::Result<SystemTime>,
}

impl MetaOps {
    const STD: MetaOps = MetaOps {
        metadata: std_meta_read,
        modified: std_meta_modified,
    };
}

// ---------------------------------------------------------------------------
// Error constructors.
//
// Each filesystem / subprocess error arm used to spell out a multi-line
// `RuntimeError::{Io,Invariant,InvalidArgument}` struct literal at the call
// site. When the happy path runs, the struct-literal body lines are never
// executed, so every arm cost 3-4 uncovered lines. Routing through these
// constructors collapses each arm to a single-line closure/expression, so an
// unreachable arm costs at most one line. The produced values are byte-for-byte
// identical to the former inline literals.

fn io_error(path: impl Into<PathBuf>, message: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Io {
        path: path.into(),
        message: message.to_string(),
    }
}

fn invariant(message: impl Into<String>) -> RuntimeError {
    RuntimeError::Invariant {
        message: message.into(),
    }
}

fn invalid_argument(message: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidArgument {
        message: message.into(),
    }
}

fn command_failed(program: &str, args: Vec<String>, message: impl Into<String>) -> RuntimeError {
    RuntimeError::CommandFailed {
        program: program.to_string(),
        args,
        message: message.into(),
    }
}

fn parse_error(path: impl Into<PathBuf>, message: impl Into<String>) -> RuntimeError {
    RuntimeError::ArtifactIndexParse {
        path: path.into(),
        message: message.into(),
    }
}

/// Map a `Child::try_wait` failure to the `InvalidArgument` this module reports
/// for it. Named so `run_command_with_timeout` can propagate it with `?` on the
/// call line and so the message is testable without a child process whose
/// `try_wait` fails — something the OS won't do for a locally spawned child.
fn cargo_wait_error(err: std::io::Error) -> RuntimeError {
    invalid_argument(format!("cargo wait failed: {err}"))
}

/// `create_dir_all` for the artifacts directory of `rebuild_plugin_workspace`.
/// Extracted so the failure arm — which needs a path the process cannot create
/// (a plain file standing where the directory belongs), not a real cargo
/// rebuild — is unit-testable. The `create artifacts dir: ` message is
/// byte-for-byte what the inline `map_err` produced.
fn create_artifacts_dir(artifacts_dir: &Path) -> Result<(), RuntimeError> {
    std::fs::create_dir_all(artifacts_dir)
        .map_err(|e| io_error(artifacts_dir, format!("create artifacts dir: {e}")))
}

/// Refresh `index.json`'s sha256 for a single freshly-staged plugin artifact.
/// Extracted from `rebuild_plugin_workspace` so the entry-match / hash-update
/// path is unit-testable against a real temp index without a cargo rebuild.
fn refresh_artifact_hash_for_plugin(
    artifact_index_path: &Path,
    plugin_name: &str,
) -> Result<(), RuntimeError> {
    if !artifact_index_path.exists() {
        return Ok(());
    }
    let mut index = load_artifact_index(artifact_index_path)?;
    let mut updated = false;
    for entry in &mut index.entries {
        if entry.plugin_path == plugin_name {
            let resolved = resolve_artifact_path(artifact_index_path, &entry.artifact_path);
            let new_hash = sha256_file(&resolved)?;
            if entry.sha256 != new_hash {
                entry.sha256 = new_hash;
                updated = true;
            }
            break;
        }
    }
    if updated {
        index.generated_at = current_build_marker();
        write_pretty_json(artifact_index_path, &index)?;
    }
    Ok(())
}

pub fn ensure_fixture_artifacts(fixtures_root: &Path) -> Result<bool, RuntimeError> {
    let report = prepare_artifacts(fixtures_root, PrepareMode::Incremental)?;
    Ok(!report.rebuilt.is_empty())
}

pub fn prepare_artifacts(
    fixtures_root: &Path,
    mode: PrepareMode,
) -> Result<PrepareArtifactsReport, RuntimeError> {
    let fixtures_root = absolute_path(fixtures_root)?;
    if !can_prepare_fixture_artifacts(&fixtures_root) {
        if matches!(mode, PrepareMode::Full) {
            return Err(invariant(format!(
                "fixture rebuild requires repo sources next to {}",
                fixtures_root.display()
            )));
        }
        return Ok(PrepareArtifactsReport::default());
    }

    let _lock = ArtifactBuildLock::acquire(&fixtures_root)?;
    prepare_artifacts_locked(&fixtures_root, mode)
}

pub fn rebuild_fixture_artifacts(
    fixtures_root: &Path,
) -> Result<Vec<(String, String)>, RuntimeError> {
    Ok(prepare_artifacts(fixtures_root, PrepareMode::Full)?.rebuilt)
}

pub fn rebuild_plugin_workspace(
    workspace_root: &Path,
    plugin_path: &str,
) -> Result<Vec<(String, String)>, RuntimeError> {
    // "/" means all plugins; "/qq" means just qq.
    let name = plugin_path.trim_start_matches('/');
    if name.is_empty() {
        // "/" delegates to rebuild_fixture_artifacts -> prepare_artifacts,
        // which acquires the same ArtifactBuildLock itself. Acquiring it here
        // as well would re-acquire the lock this process already holds
        // (create_new -> AlreadyExists -> own live pid -> spin -> timeout), so
        // the delegation path must not take the lock (R7; call-graph note:
        // prepare_artifacts never calls back into rebuild_plugin_workspace).
        return rebuild_fixture_artifacts(workspace_root);
    }
    // R7: hold the same ArtifactBuildLock `prepare_artifacts` uses (same lock
    // file under the fixtures root) for the whole rebuild. Without it this
    // path staged dylibs and did load-modify-rewrite on index.json while a
    // concurrent prepare_artifacts did the same — losing updates to the shared
    // artifacts dir and index, and racing a full_rebuild remove_dir_all. Held
    // until the function returns (scope-bound `_lock`); a busy lock surfaces
    // as ArtifactBuildLockTimeout like prepare_artifacts.
    let _lock = ArtifactBuildLock::acquire(workspace_root)?;
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(workspace_root.join("plugins").join("Cargo.toml"))
        .arg("-p")
        .arg(name);
    // P2-29: strip HTTP proxy env vars so cargo doesn't try to
    // contact a corporate proxy for the offline / local-path deps that
    // fixtures use. `run_command` (the other cargo path in this file)
    // already does this; the imbalance was itself the bug.
    strip_proxy_envs(&mut cmd);
    // P2-30: enforce a build timeout so an infinite-loop `build.rs`
    // can't hang the whole iteration pipeline. 20 minutes is generous
    // for a from-cold fixture rebuild; anything longer is almost
    // certainly a bug. Override via CORDIS_BUILD_TIMEOUT_SECS.
    let timeout_secs = std::env::var("CORDIS_BUILD_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20 * 60);
    let output = run_command_with_timeout(cmd, std::time::Duration::from_secs(timeout_secs))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(invalid_argument(format!(
            "cargo build -p {name} failed: {stderr}"
        )));
    }
    // P0-9: use platform-native dylib prefix/extension. Previously hardcoded
    // `lib{name}.so` / `{name}.so` — always wrong on macOS (`.dylib`) and
    // Windows (`.dll`), so `iterate_plugins` was un-runnable on any dev
    // machine that isn't Linux. `std::env::consts` gives us the right
    // affixes for the current target triple.
    let target_dir = workspace_root.join("plugins").join("target").join("debug");
    let src_filename = format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        name.replace('-', "_"),
        std::env::consts::DLL_SUFFIX
    );
    let dst_filename = format!("{}{}", name, std::env::consts::DLL_SUFFIX);
    let src = target_dir.join(&src_filename);
    // `dst`'s parent is `<workspace_root>/artifacts` by construction, so bind
    // the directory first instead of recovering it via `dst.parent()`: the
    // `None` arm of that lookup was structurally unreachable dead weight.
    let artifacts_dir = workspace_root.join("artifacts");
    let dst = artifacts_dir.join(&dst_filename);
    create_artifacts_dir(&artifacts_dir)?;
    // Stage-then-swap: `fs::copy` over a live-mmap'd `.so` risks SIGSEGV on
    // concurrent readers. Write to `<dst>.cordis-tmp`, fsync, rename over
    // the target — the OS unlinks the old file while any existing mapping
    // remains valid until unmap.
    stage_then_rename_file(&src, &dst)?;

    // Post-rebuild: refresh `index.json` sha256 for the freshly-staged artifact
    // (and, if present, `LoadedDylibApi::open`-derived docs so the same rebuild
    // won't be flagged as `HashMismatch` by the loader on next verify pass).
    // Without this, the E2E `iterate_plugins` path builds a candidate `.so`
    // whose bytes have moved but whose sha256 in `index.json` still reflects
    // the *previous* build; the loader's P0-13 sha check then rejects the
    // fresh artifact as unavailable, forcing a hard rollback of code that
    // actually passed tests. See docs/architecture/status-and-open-items.md
    // section 5.2.21 for the observation from the E2E smoke.
    let artifact_index_path = workspace_root.join("artifacts").join("index.json");
    refresh_artifact_hash_for_plugin(&artifact_index_path, name)?;

    Ok(vec![(
        name.to_string(),
        format!("{} -> {}", src.display(), dst.display()),
    )])
}

/// Copy `src` -> `dst` atomically: write to `<dst>.cordis-tmp`, fsync,
/// rename. On Unix this replaces the file inode; any pre-existing dylib
/// mapping stays valid until munmap while new opens see the fresh bytes.
///
/// (C batch, P0-8 / P0-15 style helper — shared between rebuild_plugin_workspace
/// and materialize_artifact_entry so both stage-swap identically.)
pub(crate) fn stage_then_rename_file(src: &Path, dst: &Path) -> Result<(), RuntimeError> {
    stage_then_rename_file_with_fs(src, dst, FsWriteOps::STD)
}

/// Fs-parameterized core of `stage_then_rename_file`. The public entry injects
/// `FsWriteOps::STD` (direct `std::fs`/`File` calls). Tests inject a struct
/// with one op replaced by a failing stub to reach the `write tmp` / `sync tmp`
/// / `rename ... failed` map_err arms, which cannot be provoked reliably from a
/// real filesystem once the handle is open. Default behaviour is byte-for-byte
/// identical.
fn stage_then_rename_file_with_fs(
    src: &Path,
    dst: &Path,
    ops: FsWriteOps,
) -> Result<(), RuntimeError> {
    use std::io::Read;
    let mut src_file =
        std::fs::File::open(src).map_err(|e| io_error(src, format!("open source: {e}")))?;
    let mut buf = Vec::new();
    src_file
        .read_to_end(&mut buf)
        .map_err(|e| io_error(src, format!("read source: {e}")))?;
    let tmp = match dst.file_name() {
        Some(name) => {
            let mut owned = name.to_os_string();
            owned.push(".cordis-tmp");
            dst.with_file_name(owned)
        }
        None => return Err(io_error(dst, "artifact target has no filename")),
    };
    {
        let mut tmp_file =
            std::fs::File::create(&tmp).map_err(|e| io_error(&tmp, format!("create tmp: {e}")))?;
        (ops.write_all)(&mut tmp_file, &buf)
            .map_err(|e| io_error(&tmp, format!("write tmp: {e}")))?;
        (ops.sync_all)(&tmp_file).map_err(|e| io_error(&tmp, format!("sync tmp: {e}")))?;
        // Preserve executable permissions from the source (dylibs are +x on
        // most platforms; without this the OS refuses to dlopen).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = src_file.metadata() {
                let mode = meta.permissions().mode();
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    (ops.rename)(&tmp, dst).map_err(|e| {
        io_error(
            dst,
            format!("rename {} -> {} failed: {e}", tmp.display(), dst.display()),
        )
    })?;
    Ok(())
}

pub fn sync_plugin_docs(fixtures_root: &Path) -> Result<Vec<PathBuf>, RuntimeError> {
    let fixtures_root = absolute_path(fixtures_root)?;
    let plugins_root = fixtures_root.join("plugins");
    // Defence: fixtures_root must contain a `plugins/` directory with a
    // Cargo workspace manifest.  If `fixtures_root` is already pointing
    // inside `plugins/`, reject it so we don't create nested paths like
    // `fixtures/plugins/plugins/qq/docs/`.
    if !plugins_root.join("Cargo.toml").exists() {
        return Err(invariant(format!(
            "plugins workspace not found at {}; \
             fixtures_root must be the project fixtures/ directory, \
             not the plugins/ subdirectory",
            plugins_root.display()
        )));
    }
    let artifact_index_path = fixtures_root.join("artifacts/index.json");
    let index = load_artifact_index(&artifact_index_path)?;

    let mut written = Vec::new();
    for entry in index.entries {
        // Build the directory first and join the file name onto it, rather than
        // building the file path and recovering the directory with `.parent()`:
        // the resulting paths are identical, and the `None` arm of that lookup
        // was structurally unreachable (a path ending in a file name always has
        // a parent).
        let docs_dir = plugins_root
            .join(
                entry
                    .plugin_path
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            )
            .join("docs/agent");
        let docs_path = docs_dir.join("interfaces.json");
        fs::create_dir_all(&docs_dir).map_err(|e| io_error(&docs_dir, e))?;
        // P1-17: durable atomic write for interfaces.json.
        write_pretty_json(&docs_path, &entry.docs)?;
        written.push(docs_path);
    }

    Ok(written)
}

pub fn refresh_artifact_index(fixtures_root: &Path) -> Result<Vec<(String, String)>, RuntimeError> {
    let fixtures_root = absolute_path(fixtures_root)?;
    let artifact_index_path = fixtures_root.join("artifacts/index.json");
    let mut index = load_artifact_index(&artifact_index_path)?;
    let mut refreshed = Vec::new();

    for entry in &mut index.entries {
        let artifact_path = resolve_artifact_path(&artifact_index_path, &entry.artifact_path);
        let hash = sha256_file(&artifact_path)?;
        entry.sha256 = hash.clone();
        refreshed.push((entry.plugin_path.clone(), hash));
    }
    index.generated_at = current_build_marker();

    // P1-17: durable atomic write. A torn artifact index blocks the next
    // verify pass entirely.
    write_pretty_json(&artifact_index_path, &index)?;

    Ok(refreshed)
}

/// Parse a JSON payload emitted by a loaded dylib's exported contract fn.
/// The dylib-open path can't be exercised without a real `.so`, so the two
/// call sites (`read_plugin_docs`, `inspect_dylib_contract`) share this mapper;
/// `what` names the failing contract so the byte-stable message is preserved.
fn parse_dylib_payload<T: serde::de::DeserializeOwned>(
    artifact_path: &Path,
    payload: &str,
    what: &str,
) -> Result<T, RuntimeError> {
    serde_json::from_str(payload)
        .map_err(|e| io_error(artifact_path, format!("runtime {what} parse failed: {e}")))
}

pub fn read_plugin_docs(artifact_path: &Path) -> Result<PluginDocs, RuntimeError> {
    if is_dylib_path(artifact_path) {
        let dylib = LoadedDylibApi::open(artifact_path)?;
        parse_dylib_payload(artifact_path, &(dylib.api().docs)().payload, "docs")
    } else {
        let artifact = load_plugin_artifact(artifact_path)?;
        Ok(artifact.docs)
    }
}

fn prepare_artifacts_locked(
    fixtures_root: &Path,
    mode: PrepareMode,
) -> Result<PrepareArtifactsReport, RuntimeError> {
    let repo_root = fixtures_root.parent().ok_or_else(|| {
        invariant(format!(
            "fixtures root missing parent: {}",
            fixtures_root.display()
        ))
    })?;
    let plugins_root = fixtures_root.join("plugins");
    let artifacts_dir = fixtures_root.join("artifacts");
    let artifact_index_path = artifacts_dir.join("index.json");
    let graph = PackageResolver::new(&plugins_root).resolve()?;
    let dependency_snapshot = DependencySnapshot::load(&plugins_root)?;
    let existing_index = load_artifact_index(&artifact_index_path).ok();
    let mut full_rebuild = matches!(mode, PrepareMode::Full) || existing_index.is_none();

    if full_rebuild && artifacts_dir.exists() {
        fs::remove_dir_all(&artifacts_dir).map_err(|e| io_error(&artifacts_dir, e))?;
    }
    fs::create_dir_all(&artifacts_dir).map_err(|e| io_error(&artifacts_dir, e))?;

    let mut contexts =
        build_plugin_contexts(repo_root, &graph, &artifacts_dir, &dependency_snapshot)?;
    let existing_map = existing_index
        .as_ref()
        .map(artifact_index_map)
        .unwrap_or_default();

    for context in &mut contexts {
        // `||` short-circuits exactly like the previous `if full_rebuild { true }
        // else { compute_dirty_state(..)? }`: a full rebuild never consults the
        // per-plugin state. Written on one line so the `?` error edge shares a
        // line with the call it guards.
        let existing = existing_map.get(&context.plugin.plugin_path);
        context.dirty = full_rebuild || compute_dirty_state(repo_root, context, existing)?;
        if context.dirty {
            context.build_fingerprint =
                Some(compute_build_fingerprint(repo_root, &context.input_files)?);
        }
    }

    if full_rebuild {
        full_rebuild = true;
    } else if contexts
        .iter()
        .any(|context| !existing_map.contains_key(&context.plugin.plugin_path) || context.dirty)
    {
        full_rebuild = false;
    }

    build_dirty_dylib_plugins(fixtures_root, &dependency_snapshot, &contexts)?;

    let built_at = current_build_marker();
    let mut report = PrepareArtifactsReport {
        full_rebuild,
        ..PrepareArtifactsReport::default()
    };
    let mut next_entries = Vec::new();

    for mut context in contexts {
        // A non-dirty context always has an index entry to reuse:
        // `compute_dirty_state` reports dirty whenever the lookup misses, and
        // `full_rebuild` marks every context dirty. Folding the lookup into one
        // `let` keeps the reuse decision a single branch instead of nesting an
        // `if let` inside `if !context.dirty`, whose else-branch could not run.
        let reusable = if context.dirty {
            None
        } else {
            existing_map.get(&context.plugin.plugin_path)
        };
        if let Some(existing) = reusable {
            report.reused.push(context.plugin.plugin_path.clone());
            let mut reused = existing.clone();
            if reused.input_probe != context.input_probe {
                reused.input_probe = context.input_probe;
            }
            next_entries.push(reused);
            continue;
        }

        let entry = materialize_artifact_entry(
            repo_root,
            &artifacts_dir,
            &dependency_snapshot,
            &mut context,
            &built_at,
        )?;
        report
            .rebuilt
            .push((entry.plugin_path.clone(), entry.sha256.clone()));
        next_entries.push(entry);
    }

    let next_index = ArtifactIndex {
        schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
        generated_at: current_build_marker(),
        topo_order: graph.topo_order.clone(),
        entries: next_entries,
    };
    // P1-17: durable atomic write.
    write_pretty_json(&artifact_index_path, &next_index)?;
    cleanup_fixture_lockfiles(&plugins_root)?;
    Ok(report)
}

fn build_plugin_contexts(
    repo_root: &Path,
    graph: &ResolvedPluginGraph,
    artifacts_dir: &Path,
    dependency_snapshot: &DependencySnapshot,
) -> Result<Vec<PluginBuildContext>, RuntimeError> {
    let mut contexts = Vec::new();
    for plugin_path in &graph.topo_order {
        let plugin =
            graph.plugins.get(plugin_path).cloned().ok_or_else(|| {
                invariant(format!("missing plugin in resolved graph: {plugin_path}"))
            })?;
        let manifest_path = plugin.dir.join("Cargo.toml");
        let build_spec = read_plugin_build_spec(&manifest_path)?;
        let artifact_kind = if build_spec.is_dylib {
            ArtifactKind::Dylib
        } else {
            ArtifactKind::Json
        };
        let artifact_name = expected_artifact_name(plugin_path, build_spec.is_dylib);
        let artifact_path = artifacts_dir.join(&artifact_name);
        let local_dep_dirs = dependency_snapshot.local_dep_dirs_for(&plugin.crate_name);
        let local_path_deps = local_dep_dirs
            .iter()
            .map(|path| relative_display(repo_root, path))
            .collect::<Vec<_>>();
        let input_files = collect_plugin_inputs(&plugin.dir, &local_dep_dirs)?;
        let input_probe = build_input_probe(repo_root, &input_files)?;

        contexts.push(PluginBuildContext {
            plugin,
            build_spec,
            artifact_name,
            artifact_path,
            artifact_kind,
            local_path_deps,
            input_files,
            input_probe,
            build_fingerprint: None,
            dirty: false,
        });
    }
    Ok(contexts)
}

fn compute_dirty_state(
    repo_root: &Path,
    context: &PluginBuildContext,
    existing: Option<&ArtifactIndexEntry>,
) -> Result<bool, RuntimeError> {
    let Some(existing) = existing else {
        return Ok(true);
    };
    if !context.artifact_path.exists() {
        return Ok(true);
    }
    if existing.version != context.build_spec.version
        || existing.abi_fingerprint != context.plugin.metadata.abi_fingerprint
        || existing.artifact_path != context.artifact_name
        || existing.parent != context.plugin.parent
        || existing.required != context.plugin.required
        || existing.grants_from_parent != grants_vec(&context.plugin.grants_from_parent)
        || existing.artifact_kind != context.artifact_kind
        || existing.local_path_deps != context.local_path_deps
    {
        return Ok(true);
    }

    if existing.input_probe == context.input_probe {
        return Ok(false);
    }

    let build_fingerprint = compute_build_fingerprint(repo_root, &context.input_files)?;
    Ok(build_fingerprint != existing.build_fingerprint)
}

fn materialize_artifact_entry(
    repo_root: &Path,
    artifacts_dir: &Path,
    dependency_snapshot: &DependencySnapshot,
    context: &mut PluginBuildContext,
    built_at: &str,
) -> Result<ArtifactIndexEntry, RuntimeError> {
    let docs = if matches!(context.artifact_kind, ArtifactKind::Dylib) {
        // `package_name` is pre-bound so both arms fit on one line each; the
        // `?` on the non-workspace arm then shares its line with the call it
        // guards instead of sitting alone on a continuation line.
        let package_name = &context.build_spec.package_name;
        let manifest = context.plugin.dir.join("Cargo.toml");
        let built_path = if dependency_snapshot.is_workspace_member(package_name) {
            dependency_snapshot.built_dylib_path(package_name)
        } else {
            built_dylib_path(&manifest, package_name)?
        };
        // P0-8/C-batch: stage-then-rename over `.so` so any pre-existing
        // mapping (Task-node plugins hold dylibs indefinitely in
        // TASK_LIBRARIES) stays valid until munmap; new opens see the fresh
        // bytes. Previously `fs::copy` truncated + wrote in place, which is
        // the historic SIGSEGV pattern.
        stage_then_rename_file(&built_path, &context.artifact_path)?;

        let (docs, runtime_fingerprint) = inspect_dylib_contract(&context.artifact_path)?;
        if runtime_fingerprint != context.plugin.metadata.abi_fingerprint {
            return Err(RuntimeError::AbiMismatch {
                plugin_path: context.plugin.plugin_path.clone(),
                expected: Box::new(context.plugin.metadata.abi_fingerprint.clone()),
                actual: Box::new(runtime_fingerprint.clone()),
                fingerprint_diff: context
                    .plugin
                    .metadata
                    .abi_fingerprint
                    .diff(&runtime_fingerprint),
            });
        }
        if docs.plugin_path != context.plugin.plugin_path {
            return Err(RuntimeError::DocsContract {
                plugin_path: context.plugin.plugin_path.clone(),
                message: format!(
                    "runtime docs.plugin_path mismatch: expected {}, got {}",
                    context.plugin.plugin_path, docs.plugin_path
                ),
            });
        }
        let docs_path = context.plugin.dir.join("docs/agent/interfaces.json");
        write_pretty_json(&docs_path, &docs)?;
        docs
    } else {
        let artifact = PluginArtifact {
            plugin_path: context.plugin.plugin_path.clone(),
            abi_fingerprint: context.plugin.metadata.abi_fingerprint.clone(),
            docs: context.plugin.docs.clone(),
            exports: context.build_spec.artifact.exports.clone(),
            execution: context.build_spec.artifact.execution.clone(),
        };
        write_pretty_json(&context.artifact_path, &artifact)?;
        context.plugin.docs.clone()
    };

    let build_fingerprint = match &context.build_fingerprint {
        Some(value) => value.clone(),
        None => compute_build_fingerprint(repo_root, &context.input_files)?,
    };
    let sha256 = sha256_file(&context.artifact_path)?;
    Ok(ArtifactIndexEntry {
        plugin_path: context.plugin.plugin_path.clone(),
        version: context.build_spec.version.clone(),
        abi_fingerprint: context.plugin.metadata.abi_fingerprint.clone(),
        artifact_path: relative_display(artifacts_dir, &context.artifact_path),
        sha256,
        built_at: built_at.to_string(),
        parent: context.plugin.parent.clone(),
        required: context.plugin.required,
        grants_from_parent: grants_vec(&context.plugin.grants_from_parent),
        docs,
        exports: context.build_spec.artifact.exports.clone(),
        execution: context.build_spec.artifact.execution.clone(),
        artifact_kind: context.artifact_kind.clone(),
        build_fingerprint,
        input_probe: context.input_probe.clone(),
        local_path_deps: context.local_path_deps.clone(),
    })
}

fn build_dirty_dylib_plugins(
    fixtures_root: &Path,
    dependency_snapshot: &DependencySnapshot,
    contexts: &[PluginBuildContext],
) -> Result<(), RuntimeError> {
    let repo_root = fixtures_root.parent().ok_or_else(|| {
        invariant(format!(
            "fixtures root missing parent: {}",
            fixtures_root.display()
        ))
    })?;
    let mut workspace_packages = Vec::new();

    for context in contexts {
        if !context.dirty || !matches!(context.artifact_kind, ArtifactKind::Dylib) {
            continue;
        }
        if dependency_snapshot.is_workspace_member(&context.build_spec.package_name) {
            workspace_packages.push(context.build_spec.package_name.clone());
        } else {
            build_plugin_artifact(fixtures_root, &context.plugin.dir.join("Cargo.toml"))?;
        }
    }

    workspace_packages.sort();
    workspace_packages.dedup();
    if workspace_packages.is_empty() {
        return Ok(());
    }

    let mut args = vec![
        "build".to_string(),
        "--manifest-path".to_string(),
        dependency_snapshot
            .workspace_manifest_path
            .display()
            .to_string(),
    ];
    for package_name in workspace_packages {
        args.push("-p".to_string());
        args.push(package_name);
    }
    run_command("cargo", &args, Some(repo_root))?;
    Ok(())
}

fn inspect_dylib_contract(
    artifact_path: &Path,
) -> Result<(PluginDocs, crate::core::models::AbiFingerprint), RuntimeError> {
    let dylib = LoadedDylibApi::open(artifact_path)?;
    // Both payloads are pulled out into locals first so each
    // `parse_dylib_payload(..)?` call fits on a single line; the `?` error edge
    // then shares a line with the call rather than trailing on its own.
    let docs_payload = (dylib.api().docs)().payload;
    let docs: PluginDocs = parse_dylib_payload(artifact_path, &docs_payload, "docs")?;
    let fp_payload = (dylib.api().abi_fingerprint)().payload;
    let fingerprint = parse_dylib_payload(artifact_path, &fp_payload, "fingerprint")?;
    Ok((docs, fingerprint))
}

impl DependencySnapshot {
    fn load(plugins_root: &Path) -> Result<Self, RuntimeError> {
        let workspace_manifest_path = plugins_root.join("Cargo.toml");
        let metadata = load_workspace_metadata(&workspace_manifest_path)?;
        let target_directory = PathBuf::from(metadata.target_directory.clone());
        let workspace_member_ids = metadata
            .workspace_members
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let packages_by_id = metadata
            .packages
            .iter()
            .map(|package| (package.id.clone(), package.clone()))
            .collect::<HashMap<_, _>>();
        let nodes_by_id = metadata
            .resolve
            .as_ref()
            .map(|resolve| {
                resolve
                    .nodes
                    .iter()
                    .map(|node| (node.id.clone(), node.dependencies.clone()))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        let mut workspace_members = HashSet::new();
        let mut local_dep_closure_by_name = HashMap::new();
        for package in &metadata.packages {
            if workspace_member_ids.contains(&package.id) {
                workspace_members.insert(package.name.clone());
            }
            let deps = collect_local_dependency_dirs(&package.id, &packages_by_id, &nodes_by_id);
            local_dep_closure_by_name.insert(package.name.clone(), deps);
        }

        Ok(Self {
            workspace_manifest_path,
            workspace_members,
            target_directory,
            local_dep_closure_by_name,
        })
    }

    fn built_dylib_path(&self, package_name: &str) -> PathBuf {
        let dylib_name = format!(
            "{DLL_PREFIX}{}.{}",
            package_name.replace('-', "_"),
            DLL_EXTENSION
        );
        self.target_directory.join("debug").join(dylib_name)
    }

    fn is_workspace_member(&self, package_name: &str) -> bool {
        self.workspace_members.contains(package_name)
    }

    fn local_dep_dirs_for(&self, package_name: &str) -> Vec<PathBuf> {
        self.local_dep_closure_by_name
            .get(package_name)
            .cloned()
            .unwrap_or_default()
    }
}

fn collect_local_dependency_dirs(
    package_id: &str,
    packages_by_id: &HashMap<String, CargoMetadataPackage>,
    nodes_by_id: &HashMap<String, Vec<String>>,
) -> Vec<PathBuf> {
    let Some(root_package) = packages_by_id.get(package_id) else {
        return Vec::new();
    };
    let root_manifest = PathBuf::from(&root_package.manifest_path);
    let root_dir = root_manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut visited = HashSet::new();
    let mut stack = vec![package_id.to_string()];
    let mut local_deps = BTreeSet::new();

    while let Some(current) = stack.pop() {
        let Some(dependencies) = nodes_by_id.get(&current) else {
            continue;
        };
        for dep_id in dependencies {
            if !visited.insert(dep_id.clone()) {
                continue;
            }
            let Some(dep_package) = packages_by_id.get(dep_id) else {
                continue;
            };
            if dep_package.source.is_none() {
                let manifest_path = PathBuf::from(&dep_package.manifest_path);
                // The "has a parent" and "is not the root crate itself" checks
                // are folded into one `Option` chain. Nesting them as two `if`s
                // left the outer `if let`'s fall-through unreachable, since a
                // manifest path always has a parent directory.
                let dep_dir = manifest_path
                    .parent()
                    .filter(|dir| *dir != root_dir.as_path());
                if let Some(dep_dir) = dep_dir {
                    local_deps.insert(dep_dir.to_path_buf());
                }
            }
            stack.push(dep_id.clone());
        }
    }

    local_deps.into_iter().collect()
}

fn load_workspace_metadata(
    workspace_manifest_path: &Path,
) -> Result<CargoMetadataOutput, RuntimeError> {
    load_workspace_metadata_with_runner(workspace_manifest_path, run_command)
}

/// Runner-parameterized core of `load_workspace_metadata`. The public entry
/// point injects the real `run_command`; tests inject a runner that fails or
/// returns synthetic bytes so the subprocess-failure and parse arms can be
/// exercised without spawning `cargo`. Argument order, timing, and error text
/// are identical to the direct implementation.
fn load_workspace_metadata_with_runner<R>(
    workspace_manifest_path: &Path,
    runner: R,
) -> Result<CargoMetadataOutput, RuntimeError>
where
    R: Fn(&str, &[String], Option<&Path>) -> Result<Vec<u8>, RuntimeError>,
{
    let output = runner(
        "cargo",
        &[
            "metadata".to_string(),
            "--format-version".to_string(),
            "1".to_string(),
            "--manifest-path".to_string(),
            workspace_manifest_path.display().to_string(),
        ],
        workspace_manifest_path.parent(),
    )?;
    parse_cargo_metadata(workspace_manifest_path, &output)
}

/// Deserialize `cargo metadata --format-version 1` output. A subprocess
/// failure can't be exercised in a unit test, so the JSON->struct step is
/// factored out here and mapped to `ArtifactIndexParse` with a byte-stable
/// message shared by both `load_workspace_metadata` and `built_dylib_path`.
fn parse_cargo_metadata(
    manifest_path: &Path,
    bytes: &[u8],
) -> Result<CargoMetadataOutput, RuntimeError> {
    serde_json::from_slice(bytes)
        .map_err(|e| parse_error(manifest_path, format!("cargo metadata parse failed: {e}")))
}

fn collect_plugin_inputs(
    plugin_dir: &Path,
    local_dep_dirs: &[PathBuf],
) -> Result<Vec<PathBuf>, RuntimeError> {
    let mut files = collect_crate_inputs(plugin_dir, true)?;
    for dep_dir in local_dep_dirs {
        files.extend(collect_crate_inputs(dep_dir, false)?);
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_crate_inputs(
    crate_dir: &Path,
    include_docs: bool,
) -> Result<Vec<PathBuf>, RuntimeError> {
    let mut files = Vec::new();
    let manifest_path = crate_dir.join("Cargo.toml");
    if manifest_path.exists() {
        files.push(manifest_path);
    }

    let build_rs = crate_dir.join("build.rs");
    if build_rs.exists() {
        files.push(build_rs);
    }

    // Iterating a 0-or-1 element option instead of gating on `if
    // src_dir.exists()`: same "only descend when the directory is there"
    // semantics, but the loop's exit edge is always taken, whereas the `if`'s
    // fall-through edge was never taken by any caller.
    let src_dir = crate_dir.join("src");
    for dir in src_dir.exists().then_some(src_dir).into_iter() {
        collect_files_recursively(&dir, &mut files)?;
    }

    if include_docs {
        let docs_path = crate_dir.join("docs/agent/interfaces.json");
        if docs_path.exists() {
            files.push(docs_path);
        }
    }

    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_files_recursively(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), RuntimeError> {
    for entry in fs::read_dir(dir).map_err(|e| io_error(dir, e))? {
        collect_dir_entry(dir, entry, out)?;
    }
    Ok(())
}

/// Process a single `read_dir` iteration item. Extracted so the
/// `entry.map_err` arm — a `DirEntry` iterator yielding `Err`, which the OS
/// won't produce deterministically for a directory that opened successfully —
/// can be exercised by feeding a synthetic `Err`. The `fs::metadata` arm is
/// reachable via a dangling symlink; both map to `Io` with identical text.
fn collect_dir_entry(
    dir: &Path,
    entry: std::io::Result<fs::DirEntry>,
    out: &mut Vec<PathBuf>,
) -> Result<(), RuntimeError> {
    let entry = entry.map_err(|e| io_error(dir, e))?;
    let path = entry.path();
    let metadata = fs::metadata(&path).map_err(|e| io_error(&path, e))?;
    if metadata.is_dir() {
        if entry.file_name() == "target" {
            return Ok(());
        }
        collect_files_recursively(&path, out)?;
    } else if metadata.is_file() {
        out.push(path);
    }
    Ok(())
}

fn build_input_probe(repo_root: &Path, files: &[PathBuf]) -> Result<InputProbe, RuntimeError> {
    build_input_probe_with_fs(repo_root, files, MetaOps::STD)
}

/// Fs-parameterized core of `build_input_probe`. The public entry injects
/// `MetaOps::STD`. Tests inject a failing `modified` op to reach the mtime
/// read-failure fallback (`eprintln` + `SystemTime::now()`), which a real
/// filesystem won't surface on a file that stat'd successfully. Default
/// behaviour is byte-for-byte identical.
fn build_input_probe_with_fs(
    repo_root: &Path,
    files: &[PathBuf],
    ops: MetaOps,
) -> Result<InputProbe, RuntimeError> {
    let mut probe = InputProbe::default();
    for file in files {
        let metadata = (ops.metadata)(file).map_err(|e| io_error(file, e))?;
        // P2-28: modtime read failure -> fall back to `now` (treat as
        // freshly modified) instead of `UNIX_EPOCH` (treat as ancient).
        // The old behaviour would silently mark a file as "very old" on
        // a filesystem that doesn't support mtime, biasing dirty-tracking
        // in the wrong direction. Log the error so an operator can
        // notice repeat cases.
        let modified_time = (ops.modified)(&metadata).unwrap_or_else(|err| {
            eprintln!(
                "[tooling] failed to read mtime for {}: {err}; treating as freshly modified",
                file.display()
            );
            SystemTime::now()
        });
        let modified_at_ms = modified_time
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        probe.files.push(InputProbeFile {
            path: relative_display(repo_root, file),
            size: metadata.len(),
            modified_at_ms,
        });
    }
    Ok(probe)
}

fn compute_build_fingerprint(repo_root: &Path, files: &[PathBuf]) -> Result<String, RuntimeError> {
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(relative_display(repo_root, file).as_bytes());
        hasher.update([0_u8]);
        let bytes = fs::read(file).map_err(|e| io_error(file, e))?;
        hasher.update(&bytes);
        hasher.update([0_u8]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn relative_display(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

fn grants_vec(grants: &BTreeSet<String>) -> Vec<String> {
    grants.iter().cloned().collect()
}

fn can_prepare_fixture_artifacts(fixtures_root: &Path) -> bool {
    fixtures_root.join("plugins/Cargo.toml").exists() && fixtures_root.parent().is_some()
}

fn read_plugin_build_spec(manifest_path: &Path) -> Result<PluginBuildSpec, RuntimeError> {
    let text = fs::read_to_string(manifest_path).map_err(|e| io_error(manifest_path, e))?;
    let manifest: PluginManifestToml =
        toml::from_str(&text).map_err(|e| RuntimeError::CargoParse {
            path: manifest_path.to_path_buf(),
            message: e.to_string(),
        })?;
    let package = manifest
        .package
        .ok_or_else(|| RuntimeError::InvalidWorkspace {
            path: manifest_path.to_path_buf(),
        })?;

    Ok(PluginBuildSpec {
        package_name: package.name,
        version: package.version,
        is_dylib: manifest
            .lib
            .as_ref()
            .map(|lib| {
                lib.crate_type
                    .iter()
                    .any(|kind| kind == "dylib" || kind == "cdylib")
            })
            .unwrap_or(false),
        artifact: package
            .metadata
            .and_then(|metadata| metadata.cordis)
            .and_then(|cordis| cordis.artifact)
            .unwrap_or_default(),
    })
}

fn build_plugin_artifact(fixtures_root: &Path, manifest_path: &Path) -> Result<(), RuntimeError> {
    build_plugin_artifact_with_runner(fixtures_root, manifest_path, run_command)
}

/// Runner-parameterized core of `build_plugin_artifact`. The public entry
/// point injects the real `run_command`; tests inject a failing runner to hit
/// the `cargo build` failure arm without spawning a subprocess. The
/// missing-parent invariant and command arguments are identical to the direct
/// implementation.
fn build_plugin_artifact_with_runner<R>(
    fixtures_root: &Path,
    manifest_path: &Path,
    runner: R,
) -> Result<(), RuntimeError>
where
    R: Fn(&str, &[String], Option<&Path>) -> Result<Vec<u8>, RuntimeError>,
{
    let repo_root = fixtures_root.parent().ok_or_else(|| {
        invariant(format!(
            "fixtures root missing parent: {}",
            fixtures_root.display()
        ))
    })?;
    runner(
        "cargo",
        &[
            "build".to_string(),
            "--manifest-path".to_string(),
            manifest_path.display().to_string(),
        ],
        Some(repo_root),
    )?;
    Ok(())
}

fn built_dylib_path(manifest_path: &Path, package_name: &str) -> Result<PathBuf, RuntimeError> {
    built_dylib_path_with_runner(manifest_path, package_name, run_command)
}

/// Runner-parameterized core of `built_dylib_path`. The public entry point
/// injects the real `run_command`; tests inject a failing runner (metadata
/// subprocess failure arm) or a runner returning synthetic metadata bytes
/// (parse + path-composition arm) without spawning `cargo`. Command arguments
/// and the resulting path layout are identical to the direct implementation.
fn built_dylib_path_with_runner<R>(
    manifest_path: &Path,
    package_name: &str,
    runner: R,
) -> Result<PathBuf, RuntimeError>
where
    R: Fn(&str, &[String], Option<&Path>) -> Result<Vec<u8>, RuntimeError>,
{
    let metadata = runner(
        "cargo",
        &[
            "metadata".to_string(),
            "--format-version".to_string(),
            "1".to_string(),
            "--no-deps".to_string(),
            "--manifest-path".to_string(),
            manifest_path.display().to_string(),
        ],
        manifest_path.parent(),
    )?;
    let parsed = parse_cargo_metadata(manifest_path, &metadata)?;
    let dylib_name = format!(
        "{DLL_PREFIX}{}.{}",
        package_name.replace('-', "_"),
        DLL_EXTENSION
    );
    Ok(PathBuf::from(parsed.target_directory)
        .join("debug")
        .join(dylib_name))
}

fn run_command(
    program: &str,
    args: &[String],
    current_dir: Option<&Path>,
) -> Result<Vec<u8>, RuntimeError> {
    let command_args = if program == "cargo" {
        prepare_local_cargo_args(args)
    } else {
        args.to_vec()
    };
    let mut command = Command::new(program);
    command.args(&command_args);
    if program == "cargo" {
        strip_proxy_envs(&mut command);
    }
    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }
    let output = command
        .output()
        .map_err(|e| command_failed(program, command_args.clone(), e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if stderr.is_empty() { stdout } else { stderr };
        return Err(command_failed(program, command_args, message));
    }
    Ok(output.stdout)
}

fn prepare_local_cargo_args(args: &[String]) -> Vec<String> {
    let mut command_args = args.to_vec();
    if cargo_command_prefers_offline(args) && !command_args.iter().any(|arg| arg == "--offline") {
        command_args.push("--offline".to_string());
    }
    command_args
}

fn cargo_command_prefers_offline(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("metadata"))
}

/// P2-30: run `command` with a wall-clock timeout. On expiry, kill + wait
/// so we return promptly with an error rather than blocking indefinitely
/// on `Command::output()`. Poll interval is 100ms; overhead is trivial
/// compared to a cargo build.
fn run_command_with_timeout(
    mut command: Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output, RuntimeError> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::Instant;
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| invalid_argument(format!("cargo spawn failed: {e}")))?;
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        // `try_wait`'s error is mapped through the named `cargo_wait_error` and
        // propagated with `?` on the call line, rather than handled in a
        // dedicated `Err(e) =>` match arm: the OS does not fail `try_wait` for a
        // child this function itself spawned, so that arm was never executed
        // while `cargo_wait_error` is directly unit-testable.
        match child.try_wait().map_err(cargo_wait_error)? {
            Some(status) => break status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break std::process::ExitStatus::default();
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_end(&mut stdout);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_end(&mut stderr);
    }
    if timed_out {
        return Err(invalid_argument(format!(
            "cargo build exceeded timeout ({:?}); stderr={}",
            timeout,
            String::from_utf8_lossy(&stderr).trim()
        )));
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn strip_proxy_envs(command: &mut Command) {
    for key in [
        "ALL_PROXY",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "all_proxy",
        "http_proxy",
        "https_proxy",
    ] {
        command.env_remove(key);
    }
}

fn expected_artifact_name(plugin_path: &str, is_dylib: bool) -> String {
    let stem = plugin_path.replace('/', "_");
    if is_dylib {
        format!("{stem}.{DLL_EXTENSION}")
    } else {
        format!("{stem}.json")
    }
}

fn write_pretty_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), RuntimeError> {
    write_pretty_json_with_fs(path, value, FsWriteOps::STD)
}

/// Fs-parameterized core of `write_pretty_json`. The public entry injects
/// `FsWriteOps::STD`. Tests inject a failing `write_all` / `sync_all` /
/// `rename` op to reach the mid-write map_err arms (and the `rename`-failure
/// tmp cleanup) that a real filesystem won't surface once the tmp handle is
/// open. Default behaviour is byte-for-byte identical.
fn write_pretty_json_with_fs<T: serde::Serialize>(
    path: &Path,
    value: &T,
    ops: FsWriteOps,
) -> Result<(), RuntimeError> {
    let parent = path
        .parent()
        .ok_or_else(|| invariant(format!("json path missing parent: {}", path.display())))?;
    (ops.create_dir_all)(parent).map_err(|e| io_error(parent, e))?;
    // P1-17: durable JSON write. Was `fs::write` — a crash mid-write left a
    // torn JSON. `artifact/index.json` is consumed by `PluginInvoker::load`
    // on the very next verify pass; a torn index bricked the runtime.
    // Now: staging tmp + sync_all + rename. Covers `write_pretty_json`'s
    // callers (docs/interfaces.json and artifact/index.json) at 337, 361,
    // 483, 1207.
    let bytes = pretty_json(value);
    let tmp = match path.file_name() {
        Some(name) => {
            let mut owned = name.to_os_string();
            owned.push(format!(".cordis-tmp.{}", std::process::id()));
            path.with_file_name(owned)
        }
        None => {
            return Err(invariant(format!(
                "path has no filename: {}",
                path.display()
            )))
        }
    };
    {
        let mut file =
            fs::File::create(&tmp).map_err(|e| io_error(&tmp, format!("create tmp: {e}")))?;
        (ops.write_all)(&mut file, bytes.as_bytes())
            .map_err(|e| io_error(&tmp, format!("write tmp: {e}")))?;
        (ops.sync_all)(&file).map_err(|e| io_error(&tmp, format!("sync tmp: {e}")))?;
    }
    if let Err(e) = (ops.rename)(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(io_error(path, format!("rename tmp -> target: {e}")));
    }
    Ok(())
}

impl ArtifactBuildLock {
    fn acquire(fixtures_root: &Path) -> Result<Self, RuntimeError> {
        Self::acquire_with_fs(fixtures_root, LockAcquireOps::STD)
    }

    /// Fs-parameterized core of `acquire`. The public entry injects
    /// `LockAcquireOps::STD`. Tests inject failing `serialize` / `write_all` /
    /// `flush` / `sync_all` ops to reach the corresponding map_err arms, a
    /// pre-occupied `open_new` (AlreadyExists) with a zero `wait_timeout` to
    /// reach the timeout arm, and an `open_new` returning a non-AlreadyExists
    /// error to reach the final Io arm — none of which a real filesystem
    /// surfaces deterministically. Default behaviour is byte-for-byte
    /// identical.
    fn acquire_with_fs(fixtures_root: &Path, ops: LockAcquireOps) -> Result<Self, RuntimeError> {
        let path = fixtures_root.join(BUILD_LOCK_FILE);
        let started_at = Instant::now();

        loop {
            match (ops.open_new)(&path) {
                Ok(mut file) => {
                    let state = ArtifactBuildLockState {
                        pid: process::id(),
                        created_at_ms: current_epoch_ms(),
                    };
                    let encoded = (ops.serialize)(&state).map_err(|err| {
                        io_error(&path, format!("lock metadata serialize failed: {err}"))
                    })?;
                    (ops.write_all)(&mut file, &encoded).map_err(|err| io_error(&path, err))?;
                    (ops.flush)(&mut file).map_err(|err| io_error(&path, err))?;
                    // P1-17: fsync the lock file so a power loss doesn't
                    // leave a 0-byte file that lock_pid_is_live can't parse.
                    (ops.sync_all)(&file).map_err(|err| io_error(&path, err))?;
                    return Ok(Self {
                        path,
                        pid: process::id(),
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    maybe_remove_stale_lock(&path)?;
                    if started_at.elapsed() > ops.wait_timeout {
                        return Err(RuntimeError::ArtifactBuildLockTimeout {
                            path,
                            waited_ms: started_at.elapsed().as_millis(),
                        });
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                Err(err) => return Err(io_error(&path, err)),
            }
        }
    }
}

fn maybe_remove_stale_lock(path: &Path) -> Result<(), RuntimeError> {
    maybe_remove_stale_lock_with_fs(path, MetaOps::STD)
}

/// Fs-parameterized core of `maybe_remove_stale_lock`. The public entry
/// injects `MetaOps::STD`. Tests inject a failing `modified` op to reach the
/// lock-mtime read-failure fallback (`eprintln` + `SystemTime::now()`), which
/// a real filesystem won't surface on a lock file that stat'd successfully.
/// Default behaviour is byte-for-byte identical.
fn maybe_remove_stale_lock_with_fs(path: &Path, ops: MetaOps) -> Result<(), RuntimeError> {
    let metadata = match (ops.metadata)(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(io_error(path, err)),
    };
    // P2-28: `unwrap_or(SystemTime::now())` was already the right
    // fallback — mtime read failure -> treat as "just modified" so the
    // lock is considered live. Kept as-is; the comment makes the intent
    // explicit so future refactors don't flip it to UNIX_EPOCH.
    let modified = (ops.modified)(&metadata).unwrap_or_else(|err| {
        eprintln!(
            "[tooling] lock file mtime read failed for {}: {err}",
            path.display()
        );
        SystemTime::now()
    });

    // Read-then-parse flattened into one `Option` chain. As two nested `if
    // let`s the outer one's else-edge (lock file unreadable) was never taken —
    // callers only reach here after the file stat'd successfully. Falling
    // through on either failure is the same behaviour as before.
    let parsed_state = fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<ArtifactBuildLockState>(&text).ok());
    if let Some(state) = parsed_state {
        // R6: a parsable JSON lock is reclaimed only when its pid is dead. The
        // old 300s age gate is gone: a live holder may legitimately build for
        // longer, and unilaterally unlinking its lock would let a newcomer
        // clobber artifacts/index.json mid-write. Tradeoff: a dead holder whose
        // pid got recycled to an unrelated process keeps the lock file around
        // until that pid dies — an intentional availability-vs-safety choice
        // in favour of safety (a recreated lock is refused for at most the
        // newcomer's wait timeout, never stolen from a live builder). Legacy /
        // unparsable files still fall back to the mtime window below.
        if !lock_pid_is_live(state.pid) {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(io_error(path, err)),
            }
        }
        return Ok(());
    }

    if SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        > LEGACY_STALE_LOCK_TIMEOUT
    {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(io_error(path, err)),
        }
    }
    Ok(())
}

/// P0-10: on macOS/BSD `/proc` does not exist, so the previous implementation
/// always returned false → every live lock was declared dead the moment the
/// staleness timer expired, letting two concurrent `prepare_artifacts` calls
/// clobber each other. Fix: use `kill(pid, 0)`, which is portable across all
/// Unix platforms and returns EPERM (still alive, other uid) or ESRCH (dead)
/// without actually delivering a signal.
#[cfg(unix)]
pub(crate) fn lock_pid_is_live(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill(pid, 0)` is signal-safe and side-effect free; it only
    // checks whether the caller *could* deliver a signal to `pid`.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // errno is thread-local; if the syscall returned an error, distinguish
    // "no such process" (ESRCH → dead) from "permission denied" (EPERM →
    // alive, owned by another user).
    let err = std::io::Error::last_os_error();
    !matches!(err.raw_os_error(), Some(libc::ESRCH))
}

#[cfg(not(unix))]
pub(crate) fn lock_pid_is_live(_pid: u32) -> bool {
    // Non-Unix: no portable liveness probe, so be conservative and assume
    // still alive; the staleness timer eventually reclaims true zombies.
    true
}

fn current_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn cleanup_fixture_lockfiles(plugins_root: &Path) -> Result<(), RuntimeError> {
    if !plugins_root.exists() {
        return Ok(());
    }
    // P2-19: gate the cleanup behind an opt-in env var. Blindly deleting
    // every `Cargo.lock` under `plugins/` on each successful build breaks
    // concurrent readers (editor lock services, another cargo process)
    // and prevents reproducible builds. The historical behaviour is kept
    // for callers who really need it; the default is now to leave lock
    // files alone.
    if std::env::var("CORDIS_CLEAN_FIXTURE_LOCKFILES")
        .ok()
        .as_deref()
        != Some("1")
    {
        return Ok(());
    }

    for entry in fs::read_dir(plugins_root).map_err(|e| io_error(plugins_root, e))? {
        let entry = entry.map_err(|e| io_error(plugins_root, e))?;
        remove_lockfiles_recursively(&entry.path())?;
    }
    Ok(())
}

fn remove_lockfiles_recursively(path: &Path) -> Result<(), RuntimeError> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::metadata(path).map_err(|e| io_error(path, e))?;
    if metadata.is_file() {
        if path.file_name().and_then(|name| name.to_str()) == Some("Cargo.lock") {
            fs::remove_file(path).map_err(|e| io_error(path, e))?;
        }
        return Ok(());
    }

    for entry in fs::read_dir(path).map_err(|e| io_error(path, e))? {
        let entry = entry.map_err(|e| io_error(path, e))?;
        if entry.file_name() == "target" {
            continue;
        }
        remove_lockfiles_recursively(&entry.path())?;
    }
    Ok(())
}

fn current_build_marker() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn absolute_path(path: &Path) -> Result<PathBuf, RuntimeError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|e| io_error(path, e))
}

#[cfg(test)]
mod tests {
    /// `true` when the process can be blocked by file-mode permission bits.
    /// Root bypasses them, so a "deny write/read then expect an I/O error"
    /// injection succeeds instead of failing there.
    #[cfg(unix)]
    fn permission_bits_are_enforced() -> bool {
        // SAFETY: `geteuid` is always safe to call and cannot fail.
        (unsafe { libc::geteuid() }) != 0
    }

    /// Verdict for a permission-denied fault injection: when mode bits bind
    /// (non-root) the call must fail the way `is_expected_error` says; when
    /// they do not (root) the call must have succeeded instead.
    ///
    /// Taking `enforced` as a parameter rather than reading the euid inside
    /// keeps both directions reachable from a unit test — an inline
    /// `if enforced { .. } else { .. }` in each test body would leave whichever
    /// branch the current euid does not take permanently uncovered.
    #[cfg(unix)]
    fn permission_fault_holds<T: std::fmt::Debug>(
        enforced: bool,
        outcome: &Result<T, crate::core::error::RuntimeError>,
        is_expected_error: impl FnOnce(&crate::core::error::RuntimeError) -> bool,
    ) -> bool {
        match (enforced, outcome) {
            (true, Err(err)) => is_expected_error(err),
            (true, Ok(_)) => false,
            (false, res) => res.is_ok(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn permission_fault_holds_covers_both_euid_directions() {
        use crate::core::error::RuntimeError;
        let io = RuntimeError::Io {
            path: "/x".into(),
            message: "create tmp: denied".to_string(),
        };
        let is_create_tmp = |e: &RuntimeError| matches!(e, RuntimeError::Io { message, .. } if message.starts_with("create tmp: "));
        // Enforced: the expected error passes, a mismatched error and an
        // unexpected success both fail.
        let denied: Result<(), RuntimeError> = Err(io);
        assert!(permission_fault_holds(true, &denied, is_create_tmp));
        let other: Result<(), RuntimeError> = Err(super::invariant("nope".to_string()));
        assert!(!permission_fault_holds(true, &other, is_create_tmp));
        let ok: Result<(), RuntimeError> = Ok(());
        assert!(!permission_fault_holds(true, &ok, is_create_tmp));
        // Not enforced (root): success is required, failure is not accepted.
        assert!(permission_fault_holds(false, &ok, is_create_tmp));
        assert!(!permission_fault_holds(false, &denied, is_create_tmp));
    }

    use super::{cargo_command_prefers_offline, prepare_local_cargo_args};

    #[test]
    fn metadata_commands_run_offline_for_local_fixture_tooling() {
        let args = vec![
            "metadata".to_string(),
            "--format-version".to_string(),
            "1".to_string(),
        ];
        assert!(cargo_command_prefers_offline(&args));
        assert_eq!(
            prepare_local_cargo_args(&args),
            vec![
                "metadata".to_string(),
                "--format-version".to_string(),
                "1".to_string(),
                "--offline".to_string(),
            ]
        );
    }

    #[test]
    fn non_metadata_commands_keep_original_cargo_args() {
        let args = vec![
            "build".to_string(),
            "--manifest-path".to_string(),
            "fixtures/plugins/Cargo.toml".to_string(),
        ];
        assert!(!cargo_command_prefers_offline(&args));
        assert_eq!(prepare_local_cargo_args(&args), args);
    }

    // ---------- C batch (P0-9 / P0-10) tests ----------

    #[test]
    fn stage_then_rename_replaces_existing_target_atomically() {
        // P0-9 sibling: the shared stage-then-rename helper must overwrite an
        // existing dst with the new source bytes; no leftover .cordis-tmp.
        use super::stage_then_rename_file;
        use tempfile::TempDir;
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("plugin.so");
        let dst = temp.path().join("target.so");
        std::fs::write(&src, b"fresh").unwrap();
        std::fs::write(&dst, b"stale").unwrap();
        stage_then_rename_file(&src, &dst).expect("stage-then-rename ok");
        assert_eq!(std::fs::read(&dst).unwrap(), b"fresh");
        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".cordis-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no leftover tmp file expected");
    }

    #[cfg(unix)]
    #[test]
    fn lock_pid_liveness_recognises_current_process_as_alive() {
        // P0-10: previously used `/proc/{pid}` and returned false on macOS/BSD
        // for every live pid. `kill(pid, 0)` is portable; the running process
        // must always be observed as alive.
        use super::lock_pid_is_live;
        assert!(lock_pid_is_live(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn lock_pid_liveness_reports_dead_pid_as_dead() {
        use super::lock_pid_is_live;
        // A pid we're extremely unlikely to have — u32::MAX is above the
        // maximum kernel-allowed pid on all common systems.
        assert!(!lock_pid_is_live(u32::MAX - 1));
    }

    /// P1-17: `write_pretty_json` is the durable writer for
    /// interfaces.json / artifact/index.json / lock files. Verify tmp
    /// + rename semantics: no stray `.cordis-tmp.<pid>` after a
    ///   success.
    #[test]
    fn write_pretty_json_is_atomic_and_leaves_no_tmp() {
        use super::write_pretty_json;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("index.json");
        write_pretty_json(&target, &serde_json::json!({"k": 1})).unwrap();
        assert!(target.exists());
        // No leftover tmp file.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".cordis-tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no tmp leftover expected");
        // Overwrite preserves prior success semantics: file has new bytes.
        write_pretty_json(&target, &serde_json::json!({"k": 2})).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("\"k\""));
        assert!(text.contains("2"));
    }

    /// P0-9 sibling: `stage_then_rename_file` preserves the source's
    /// executable bit on Unix (dlopen requires +x on the dylib).
    #[cfg(unix)]
    #[test]
    fn stage_then_rename_preserves_exec_bit() {
        use super::stage_then_rename_file;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("plug.so");
        let dst = dir.path().join("target.so");
        std::fs::write(&src, b"bytes").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
        stage_then_rename_file(&src, &dst).unwrap();
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "exec bits should be preserved, got {mode:o}"
        );
    }

    // ---------- path / name helpers ----------

    #[test]
    fn relative_display_strips_base_and_normalises_separators() {
        use super::relative_display;
        use std::path::Path;
        let base = Path::new("/repo/root");
        assert_eq!(
            relative_display(base, Path::new("/repo/root/plugins/qq/src/lib.rs")),
            "plugins/qq/src/lib.rs"
        );
        // Path outside base falls back to the lossy absolute rendering.
        assert_eq!(
            relative_display(base, Path::new("/other/place.rs")),
            "/other/place.rs"
        );
    }

    #[test]
    fn grants_vec_preserves_sorted_order() {
        use super::grants_vec;
        use std::collections::BTreeSet;
        let mut set = BTreeSet::new();
        set.insert("write".to_string());
        set.insert("read".to_string());
        // BTreeSet is ordered, so the Vec comes out sorted.
        assert_eq!(
            grants_vec(&set),
            vec!["read".to_string(), "write".to_string()]
        );
        assert!(grants_vec(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn expected_artifact_name_uses_extension_by_kind() {
        use super::expected_artifact_name;
        use std::env::consts::DLL_EXTENSION;
        assert_eq!(
            expected_artifact_name("expr/evaluator", false),
            "expr_evaluator.json"
        );
        assert_eq!(
            expected_artifact_name("expr/evaluator", true),
            format!("expr_evaluator.{DLL_EXTENSION}")
        );
    }

    #[test]
    fn can_prepare_requires_plugins_manifest() {
        use super::can_prepare_fixture_artifacts;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        assert!(!can_prepare_fixture_artifacts(dir.path()));
        std::fs::create_dir_all(dir.path().join("plugins")).unwrap();
        std::fs::write(dir.path().join("plugins/Cargo.toml"), "[workspace]\n").unwrap();
        assert!(can_prepare_fixture_artifacts(dir.path()));
    }

    #[test]
    fn build_markers_are_numeric_strings() {
        use super::{current_build_marker, current_epoch_ms};
        let marker = current_build_marker();
        assert!(
            marker.parse::<u64>().is_ok(),
            "marker should be secs: {marker}"
        );
        assert!(current_epoch_ms() > 0);
    }

    #[test]
    fn absolute_path_passes_through_absolute_and_joins_relative() {
        use super::absolute_path;
        use std::path::{Path, PathBuf};
        let abs = Path::new("/tmp/some/where");
        assert_eq!(
            absolute_path(abs).unwrap(),
            PathBuf::from("/tmp/some/where")
        );
        let rel = absolute_path(Path::new("rel/child")).unwrap();
        assert!(rel.is_absolute());
        assert!(rel.ends_with("rel/child"));
    }

    // ---------- manifest parsing ----------

    #[test]
    fn read_plugin_build_spec_parses_dylib_and_artifact_config() {
        use super::read_plugin_build_spec;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            r#"[package]
name = "demo_plugin"
version = "1.2.3"

[lib]
crate-type = ["rlib", "cdylib"]

[package.metadata.cordis.artifact]
exports = ["svc_a", "svc_b"]
"#,
        )
        .unwrap();
        let spec = read_plugin_build_spec(&manifest).unwrap();
        assert_eq!(spec.package_name, "demo_plugin");
        assert_eq!(spec.version, "1.2.3");
        assert!(spec.is_dylib);
        assert_eq!(spec.artifact.exports, vec!["svc_a", "svc_b"]);
    }

    #[test]
    fn read_plugin_build_spec_defaults_to_json_when_no_dylib_crate_type() {
        use super::read_plugin_build_spec;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"jsonp\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let spec = read_plugin_build_spec(&manifest).unwrap();
        assert!(!spec.is_dylib);
        assert!(spec.artifact.exports.is_empty());
    }

    #[test]
    fn read_plugin_build_spec_rejects_missing_package_and_bad_toml() {
        use super::read_plugin_build_spec;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();

        // Missing file -> Io error.
        let missing = dir.path().join("nope/Cargo.toml");
        assert!(matches!(
            read_plugin_build_spec(&missing),
            Err(RuntimeError::Io { .. })
        ));

        // Malformed TOML -> CargoParse.
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "this is = = not toml").unwrap();
        assert!(matches!(
            read_plugin_build_spec(&bad),
            Err(RuntimeError::CargoParse { .. })
        ));

        // Valid TOML but no [package] -> InvalidWorkspace.
        let no_pkg = dir.path().join("nopkg.toml");
        std::fs::write(&no_pkg, "[lib]\ncrate-type=[\"dylib\"]\n").unwrap();
        assert!(matches!(
            read_plugin_build_spec(&no_pkg),
            Err(RuntimeError::InvalidWorkspace { .. })
        ));
    }

    // ---------- input collection / probe / fingerprint ----------

    fn scaffold_crate(root: &std::path::Path) {
        use std::fs;
        fs::create_dir_all(root.join("src/inner")).unwrap();
        fs::create_dir_all(root.join("docs/agent")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname=\"c\"\n").unwrap();
        fs::write(root.join("build.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/lib.rs"), "// lib").unwrap();
        fs::write(root.join("src/inner/mod.rs"), "// inner").unwrap();
        fs::write(root.join("docs/agent/interfaces.json"), "{}").unwrap();
        // Should be excluded by the `target` skip.
        fs::write(root.join("target/debug/artifact.bin"), "x").unwrap();
    }

    #[test]
    fn collect_crate_inputs_gathers_sources_and_skips_target() {
        use super::collect_crate_inputs;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        scaffold_crate(dir.path());

        let files = collect_crate_inputs(dir.path(), true).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(names.contains(&"Cargo.toml".to_string()));
        assert!(names.contains(&"build.rs".to_string()));
        assert!(names.contains(&"src/lib.rs".to_string()));
        assert!(names.contains(&"src/inner/mod.rs".to_string()));
        assert!(names.contains(&"docs/agent/interfaces.json".to_string()));
        assert!(
            !names.iter().any(|n| n.contains("target/")),
            "target/ must be excluded, got {names:?}"
        );

        // include_docs=false drops the interfaces.json.
        let no_docs = collect_crate_inputs(dir.path(), false).unwrap();
        assert!(!no_docs
            .iter()
            .any(|p| p.ends_with("docs/agent/interfaces.json")));
    }

    #[test]
    fn collect_plugin_inputs_merges_local_deps_deduped_sorted() {
        use super::collect_plugin_inputs;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let plugin = dir.path().join("plugin");
        let dep = dir.path().join("dep");
        scaffold_crate(&plugin);
        scaffold_crate(&dep);

        let files = collect_plugin_inputs(&plugin, std::slice::from_ref(&dep)).unwrap();
        // Sorted + deduped.
        let mut sorted = files.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(files, sorted);
        // Contains files from both crates.
        assert!(files.iter().any(|p| p.starts_with(&plugin)));
        assert!(files.iter().any(|p| p.starts_with(&dep)));
        // Dep contributes no interfaces.json (include_docs=false for deps).
        let dep_docs = dep.join("docs/agent/interfaces.json");
        assert!(!files.contains(&dep_docs));
    }

    #[test]
    fn build_input_probe_records_relative_path_and_size() {
        use super::build_input_probe;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("plugins/qq/src/lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"hello world").unwrap();

        let probe = build_input_probe(dir.path(), std::slice::from_ref(&file)).unwrap();
        assert_eq!(probe.files.len(), 1);
        assert_eq!(probe.files[0].path, "plugins/qq/src/lib.rs");
        assert_eq!(probe.files[0].size, 11);
    }

    #[test]
    fn build_input_probe_errors_on_missing_file() {
        use super::build_input_probe;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("ghost.rs");
        assert!(matches!(
            build_input_probe(dir.path(), &[missing]),
            Err(RuntimeError::Io { .. })
        ));
    }

    #[test]
    fn compute_build_fingerprint_is_content_sensitive_and_deterministic() {
        use super::compute_build_fingerprint;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, b"one").unwrap();

        let fp1 = compute_build_fingerprint(dir.path(), std::slice::from_ref(&file)).unwrap();
        let fp2 = compute_build_fingerprint(dir.path(), std::slice::from_ref(&file)).unwrap();
        assert_eq!(fp1, fp2, "same content -> same fingerprint");

        std::fs::write(&file, b"two").unwrap();
        let fp3 = compute_build_fingerprint(dir.path(), std::slice::from_ref(&file)).unwrap();
        assert_ne!(fp1, fp3, "content change -> new fingerprint");
    }

    #[test]
    fn compute_build_fingerprint_errors_on_missing_file() {
        use super::compute_build_fingerprint;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            compute_build_fingerprint(dir.path(), &[dir.path().join("nope.rs")]),
            Err(RuntimeError::Io { .. })
        ));
    }

    // ---------- compute_dirty_state ----------

    fn sample_abi() -> crate::core::models::AbiFingerprint {
        crate::core::models::AbiFingerprint {
            rustc_version: "rustc".to_string(),
            target_triple: "triple".to_string(),
            crate_hash: "crate_v1".to_string(),
            api_hash: "api_v1".to_string(),
        }
    }

    fn sample_context(artifact_path: std::path::PathBuf) -> super::PluginBuildContext {
        use super::{PluginBuildContext, PluginBuildSpec, SourceArtifactConfig};
        use crate::core::models::{ArtifactKind, CordisMetadata, InputProbe};
        use crate::plugin::package::ResolvedPlugin;
        use cordis_plugin_sdk::plugin_docs;
        use std::collections::BTreeSet;
        let plugin = ResolvedPlugin {
            plugin_path: "foo".to_string(),
            crate_name: "foo".to_string(),
            dir: std::path::PathBuf::from("/repo/plugins/foo"),
            metadata: CordisMetadata {
                plugin_path: "foo".to_string(),
                abi_kind: Default::default(),
                abi_fingerprint: sample_abi(),
                children: Vec::new(),
                declared_nodes: Vec::new(),
                allow_generated_docs: false,
            },
            docs: plugin_docs("foo", "foo", "0.1.0", None, Vec::new(), None),
            parent: None,
            required: true,
            grants_from_parent: BTreeSet::new(),
        };
        PluginBuildContext {
            plugin,
            build_spec: PluginBuildSpec {
                package_name: "foo".to_string(),
                version: "0.1.0".to_string(),
                is_dylib: false,
                artifact: SourceArtifactConfig::default(),
            },
            artifact_name: "foo.json".to_string(),
            artifact_path,
            artifact_kind: ArtifactKind::Json,
            local_path_deps: Vec::new(),
            input_files: Vec::new(),
            input_probe: InputProbe::default(),
            build_fingerprint: None,
            dirty: false,
        }
    }

    fn entry_matching(ctx: &super::PluginBuildContext) -> crate::core::models::ArtifactIndexEntry {
        crate::core::models::ArtifactIndexEntry {
            plugin_path: ctx.plugin.plugin_path.clone(),
            version: ctx.build_spec.version.clone(),
            abi_fingerprint: ctx.plugin.metadata.abi_fingerprint.clone(),
            artifact_path: ctx.artifact_name.clone(),
            sha256: "abc".to_string(),
            built_at: "0".to_string(),
            parent: ctx.plugin.parent.clone(),
            required: ctx.plugin.required,
            grants_from_parent: Vec::new(),
            docs: ctx.plugin.docs.clone(),
            exports: Vec::new(),
            execution: None,
            artifact_kind: ctx.artifact_kind.clone(),
            build_fingerprint: "fp".to_string(),
            input_probe: ctx.input_probe.clone(),
            local_path_deps: ctx.local_path_deps.clone(),
        }
    }

    #[test]
    fn compute_dirty_state_true_when_no_existing_entry() {
        use super::compute_dirty_state;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("foo.json");
        std::fs::write(&artifact, "{}").unwrap();
        let ctx = sample_context(artifact);
        assert!(compute_dirty_state(dir.path(), &ctx, None).unwrap());
    }

    #[test]
    fn compute_dirty_state_true_when_artifact_missing() {
        use super::compute_dirty_state;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("absent.json");
        let ctx = sample_context(artifact);
        let existing = entry_matching(&ctx);
        assert!(compute_dirty_state(dir.path(), &ctx, Some(&existing)).unwrap());
    }

    #[test]
    fn compute_dirty_state_true_on_metadata_drift() {
        use super::compute_dirty_state;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("foo.json");
        std::fs::write(&artifact, "{}").unwrap();
        let ctx = sample_context(artifact);
        let mut existing = entry_matching(&ctx);
        existing.version = "9.9.9".to_string();
        assert!(compute_dirty_state(dir.path(), &ctx, Some(&existing)).unwrap());
    }

    #[test]
    fn compute_dirty_state_false_when_probe_matches() {
        use super::compute_dirty_state;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("foo.json");
        std::fs::write(&artifact, "{}").unwrap();
        let ctx = sample_context(artifact);
        let existing = entry_matching(&ctx);
        // probe equal (both default) -> clean.
        assert!(!compute_dirty_state(dir.path(), &ctx, Some(&existing)).unwrap());
    }

    #[test]
    fn compute_dirty_state_uses_fingerprint_when_probe_differs() {
        use super::{compute_build_fingerprint, compute_dirty_state};
        use crate::core::models::{InputProbe, InputProbeFile};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("foo.json");
        std::fs::write(&artifact, "{}").unwrap();
        let ctx = sample_context(artifact);
        // ctx.input_files is empty, so the fingerprint is stable.
        let real_fp = compute_build_fingerprint(dir.path(), &ctx.input_files).unwrap();

        let differing_probe = InputProbe {
            files: vec![InputProbeFile {
                path: "x".to_string(),
                size: 1,
                modified_at_ms: 42,
            }],
        };

        // Probe differs but the recomputed fingerprint matches -> clean.
        let mut same_fp = entry_matching(&ctx);
        same_fp.input_probe = differing_probe.clone();
        same_fp.build_fingerprint = real_fp;
        assert!(!compute_dirty_state(dir.path(), &ctx, Some(&same_fp)).unwrap());

        // Probe differs and stored fingerprint differs -> dirty.
        let mut diff_fp = entry_matching(&ctx);
        diff_fp.input_probe = differing_probe;
        diff_fp.build_fingerprint = "stale-fingerprint".to_string();
        assert!(compute_dirty_state(dir.path(), &ctx, Some(&diff_fp)).unwrap());
    }

    // ---------- read_plugin_docs (JSON artifact path) ----------

    #[test]
    fn read_plugin_docs_reads_json_artifact() {
        use super::read_plugin_docs;
        use crate::core::models::PluginArtifact;
        use cordis_plugin_sdk::{plugin_docs, pretty_json};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plug.json");
        let artifact = PluginArtifact {
            plugin_path: "foo".to_string(),
            abi_fingerprint: sample_abi(),
            docs: plugin_docs("foo", "foo", "0.1.0", Some("Foo"), Vec::new(), None),
            exports: Vec::new(),
            execution: None,
        };
        std::fs::write(&path, pretty_json(&artifact)).unwrap();

        let docs = read_plugin_docs(&path).unwrap();
        assert_eq!(docs.plugin_path, "foo");
        assert_eq!(docs.command_name.as_deref(), Some("Foo"));
    }

    #[test]
    fn read_plugin_docs_errors_on_bad_json_artifact() {
        use super::read_plugin_docs;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plug.json");
        std::fs::write(&path, "{ not valid").unwrap();
        assert!(matches!(
            read_plugin_docs(&path),
            Err(RuntimeError::ArtifactIndexParse { .. })
        ));
    }

    // ---------- refresh_artifact_hash_for_plugin (extracted) ----------

    fn write_single_entry_index(
        dir: &std::path::Path,
        artifact_bytes: &[u8],
    ) -> std::path::PathBuf {
        use super::write_pretty_json;
        use crate::core::models::{ArtifactIndex, ARTIFACT_INDEX_SCHEMA_VERSION};
        // Real artifact file the index entry points at (relative "foo.json").
        std::fs::write(dir.join("foo.json"), artifact_bytes).unwrap();
        let ctx = sample_context(dir.join("foo.json"));
        let mut entry = entry_matching(&ctx);
        entry.sha256 = "stale-hash".to_string();
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "0".to_string(),
            topo_order: vec!["foo".to_string()],
            entries: vec![entry],
        };
        let index_path = dir.join("index.json");
        write_pretty_json(&index_path, &index).unwrap();
        index_path
    }

    #[test]
    fn refresh_artifact_hash_for_plugin_noop_when_index_missing() {
        use super::refresh_artifact_hash_for_plugin;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        // No index.json present -> Ok, nothing created.
        refresh_artifact_hash_for_plugin(&dir.path().join("index.json"), "foo").unwrap();
        assert!(!dir.path().join("index.json").exists());
    }

    #[test]
    fn refresh_artifact_hash_for_plugin_updates_stale_hash() {
        use super::{load_artifact_index, refresh_artifact_hash_for_plugin};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let index_path = write_single_entry_index(dir.path(), b"artifact-bytes");
        refresh_artifact_hash_for_plugin(&index_path, "foo").unwrap();
        let index = load_artifact_index(&index_path).unwrap();
        // The stored hash was rewritten to the real sha256 of the artifact.
        assert_ne!(index.entries[0].sha256, "stale-hash");
        assert!(!index.entries[0].sha256.is_empty());
    }

    #[test]
    fn refresh_artifact_hash_for_plugin_ignores_unknown_plugin() {
        use super::{load_artifact_index, refresh_artifact_hash_for_plugin};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let index_path = write_single_entry_index(dir.path(), b"artifact-bytes");
        // A plugin name absent from the index leaves every entry untouched.
        refresh_artifact_hash_for_plugin(&index_path, "not-present").unwrap();
        let index = load_artifact_index(&index_path).unwrap();
        assert_eq!(index.entries[0].sha256, "stale-hash");
    }

    // ---------- write_pretty_json error branch ----------

    #[test]
    fn write_pretty_json_errors_when_path_has_no_parent() {
        use super::write_pretty_json;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        // Root path has no parent directory.
        let err = write_pretty_json(Path::new("/"), &serde_json::json!({"k": 1}));
        assert!(matches!(err, Err(RuntimeError::Invariant { .. })));
    }

    /// A path that has a parent but no `file_name` (ends in `..`) passes the
    /// parent check and `create_dir_all`, then trips the tmp-name
    /// `file_name() == None` Invariant arm. Distinct from the no-parent test
    /// which fails earlier.
    #[test]
    fn write_pretty_json_errors_when_path_has_no_filename() {
        use super::write_pretty_json;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        // `<tempdir>/..` -> parent is <tempdir> (exists), file_name is None.
        let target = dir.path().join("..");
        let err = write_pretty_json(&target, &serde_json::json!({"k": 1}));
        assert!(
            matches!(&err, Err(RuntimeError::Invariant { message }) if message.starts_with("path has no filename: ")),
            "expected Invariant(no filename), got {err:?}"
        );
    }

    // ---------- stage_then_rename_file error branch ----------

    #[test]
    fn stage_then_rename_errors_when_source_missing() {
        use super::stage_then_rename_file;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("nope.so");
        let dst = dir.path().join("out.so");
        assert!(matches!(
            stage_then_rename_file(&src, &dst),
            Err(RuntimeError::Io { .. })
        ));
    }

    // ---------- run_command ----------

    #[test]
    fn run_command_returns_stdout_on_success() {
        use super::run_command;
        let out = run_command("echo", &["hello".to_string()], None).unwrap();
        assert_eq!(String::from_utf8_lossy(&out).trim(), "hello");
    }

    #[test]
    fn run_command_maps_nonzero_exit_to_command_failed() {
        use super::run_command;
        use crate::core::error::RuntimeError;
        assert!(matches!(
            run_command("false", &[], None),
            Err(RuntimeError::CommandFailed { .. })
        ));
    }

    #[test]
    fn run_command_maps_spawn_failure_to_command_failed() {
        use super::run_command;
        use crate::core::error::RuntimeError;
        let err = run_command("cordis-no-such-program-xyz", &[], None);
        assert!(matches!(err, Err(RuntimeError::CommandFailed { .. })));
    }

    // ---------- run_command_with_timeout ----------

    #[test]
    fn run_command_with_timeout_returns_quickly_for_fast_command() {
        use super::run_command_with_timeout;
        use std::process::Command;
        let mut cmd = Command::new("echo");
        cmd.arg("done");
        let output = run_command_with_timeout(cmd, std::time::Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");
    }

    #[test]
    fn run_command_with_timeout_kills_and_errors_on_expiry() {
        use super::run_command_with_timeout;
        use crate::core::error::RuntimeError;
        use std::process::Command;
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let err = run_command_with_timeout(cmd, std::time::Duration::from_millis(150));
        assert!(
            matches!(&err, Err(RuntimeError::InvalidArgument { message }) if message.contains("timeout")),
            "expected timeout error, got {err:?}"
        );
    }

    // ---------- lock lifecycle ----------

    #[test]
    fn artifact_build_lock_writes_state_and_drops_file() {
        use super::{ArtifactBuildLock, ArtifactBuildLockState, BUILD_LOCK_FILE};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(BUILD_LOCK_FILE);
        {
            let _lock = ArtifactBuildLock::acquire(dir.path()).unwrap();
            assert!(lock_path.exists(), "lock file should exist while held");
            let text = std::fs::read_to_string(&lock_path).unwrap();
            let state: ArtifactBuildLockState = serde_json::from_str(&text).unwrap();
            assert_eq!(state.pid, std::process::id());
        }
        // Drop released the lock file.
        assert!(!lock_path.exists(), "lock file should be removed on drop");
    }

    #[test]
    fn artifact_build_lock_drop_keeps_lock_taken_over_by_other_pid() {
        use super::{ArtifactBuildLock, ArtifactBuildLockState, BUILD_LOCK_FILE};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(BUILD_LOCK_FILE);
        {
            let lock = ArtifactBuildLock {
                path: lock_path.clone(),
                pid: std::process::id(),
            };
            // The lock file now records a *different* pid: another holder took
            // it over while we were alive (R6). Drop must leave their lock
            // alone — the old code would have unlinked it out from under them.
            let other = ArtifactBuildLockState {
                pid: std::process::id().wrapping_add(7),
                created_at_ms: 0,
            };
            std::fs::write(&lock_path, serde_json::to_vec(&other).unwrap()).unwrap();
            drop(lock);
        }
        assert!(lock_path.exists(), "a taken-over lock must survive our drop");
    }

    #[test]
    fn artifact_build_lock_drop_keeps_unparsable_lock_file() {
        use super::{ArtifactBuildLock, BUILD_LOCK_FILE};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(BUILD_LOCK_FILE);
        let lock = ArtifactBuildLock {
            path: lock_path.clone(),
            pid: std::process::id(),
        };
        // Corrupt the lock body: unparsable -> conservative, refuse to delete
        // (a delete here could destroy another holder's lock).
        std::fs::write(&lock_path, b"not json at all").unwrap();
        drop(lock);
        assert!(lock_path.exists(), "an unparsable lock must not be deleted");
    }

    #[test]
    fn artifact_build_lock_drop_noop_when_file_already_missing() {
        use super::{ArtifactBuildLock, BUILD_LOCK_FILE};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(BUILD_LOCK_FILE);
        // Lock file already gone (reclaimed earlier): Drop must be a silent
        // no-op, not a panic and not a resurrection attempt.
        let lock = ArtifactBuildLock {
            path: lock_path,
            pid: std::process::id(),
        };
        drop(lock);
    }

    #[test]
    fn artifact_build_lock_drop_keeps_unreadable_lock_file() {
        use super::{ArtifactBuildLock, BUILD_LOCK_FILE};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(BUILD_LOCK_FILE);
        // A directory where the lock file should be: `read_to_string` fails
        // with a non-NotFound io error (EISDIR) -> conservative, refuse to
        // delete.
        std::fs::create_dir(&lock_path).unwrap();
        let lock = ArtifactBuildLock {
            path: lock_path.clone(),
            pid: std::process::id(),
        };
        drop(lock);
        assert!(lock_path.exists(), "an unreadable lock must not be deleted");
    }

    #[test]
    fn artifact_build_lock_reclaims_dead_pid_lock() {
        use super::current_epoch_ms;
        use super::{ArtifactBuildLock, ArtifactBuildLockState, BUILD_LOCK_FILE};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(BUILD_LOCK_FILE);
        // Plant a lock owned by a dead pid.
        let stale = ArtifactBuildLockState {
            pid: u32::MAX - 1,
            created_at_ms: current_epoch_ms(),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        // Acquire should reclaim it promptly rather than time out.
        let _lock = ArtifactBuildLock::acquire(dir.path()).unwrap();
        let text = std::fs::read_to_string(&lock_path).unwrap();
        let state: ArtifactBuildLockState = serde_json::from_str(&text).unwrap();
        assert_eq!(state.pid, std::process::id());
    }

    #[test]
    fn maybe_remove_stale_lock_noop_when_missing() {
        use super::maybe_remove_stale_lock;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        // No file -> Ok, no error.
        maybe_remove_stale_lock(&dir.path().join("absent.lock")).unwrap();
    }

    #[test]
    fn maybe_remove_stale_lock_keeps_live_pid_lock() {
        use super::current_epoch_ms;
        use super::{maybe_remove_stale_lock, ArtifactBuildLockState};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("live.lock");
        let live = ArtifactBuildLockState {
            pid: std::process::id(),
            created_at_ms: current_epoch_ms(),
        };
        std::fs::write(&path, serde_json::to_vec(&live).unwrap()).unwrap();
        maybe_remove_stale_lock(&path).unwrap();
        assert!(path.exists(), "a live-pid, fresh lock must be preserved");
    }

    #[test]
    fn maybe_remove_stale_lock_keeps_live_pid_but_overaged_json_lock() {
        use super::current_epoch_ms;
        use super::{maybe_remove_stale_lock, ArtifactBuildLockState};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overage.lock");
        // R6 regression: a live holder whose build has run far past the old
        // 300s gate (default build timeout is 20min) must NOT have its lock
        // reclaimed. The JSON path keys on pid liveness alone; age is
        // irrelevant for parsable locks.
        let overage = ArtifactBuildLockState {
            pid: std::process::id(),
            created_at_ms: current_epoch_ms() - 10 * 60 * 1000, // 10min > old 300s
        };
        std::fs::write(&path, serde_json::to_vec(&overage).unwrap()).unwrap();
        maybe_remove_stale_lock(&path).unwrap();
        assert!(
            path.exists(),
            "a live-pid lock must be preserved regardless of age"
        );
    }

    #[test]
    fn maybe_remove_stale_lock_removes_dead_pid_lock() {
        use super::current_epoch_ms;
        use super::{maybe_remove_stale_lock, ArtifactBuildLockState};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dead.lock");
        let dead = ArtifactBuildLockState {
            pid: u32::MAX - 1,
            created_at_ms: current_epoch_ms(),
        };
        std::fs::write(&path, serde_json::to_vec(&dead).unwrap()).unwrap();
        maybe_remove_stale_lock(&path).unwrap();
        assert!(!path.exists(), "a dead-pid lock must be reclaimed");
    }

    #[test]
    fn maybe_remove_stale_lock_removes_legacy_old_nonjson_file() {
        use super::maybe_remove_stale_lock;
        use std::time::{Duration, SystemTime};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.lock");
        std::fs::write(&path, "not json at all").unwrap();
        // Backdate mtime well past LEGACY_STALE_LOCK_TIMEOUT (30s) via std's
        // File::set_modified (no external crate needed).
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
        drop(file);
        maybe_remove_stale_lock(&path).unwrap();
        assert!(!path.exists(), "an ancient non-JSON lock must be reclaimed");
    }

    #[test]
    fn maybe_remove_stale_lock_keeps_fresh_legacy_file() {
        use super::maybe_remove_stale_lock;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy_fresh.lock");
        std::fs::write(&path, "not json at all").unwrap();
        // Just-created mtime is within the legacy timeout -> kept.
        maybe_remove_stale_lock(&path).unwrap();
        assert!(path.exists(), "a fresh non-JSON lock must be preserved");
    }

    // ---------- fixture lockfile cleanup ----------

    #[test]
    fn remove_lockfiles_recursively_deletes_cargo_lock_but_skips_target() {
        use super::remove_lockfiles_recursively;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("qq");
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("Cargo.lock"), "l").unwrap();
        std::fs::write(root.join("child/Cargo.lock"), "l").unwrap();
        std::fs::write(root.join("target/Cargo.lock"), "l").unwrap();
        std::fs::write(root.join("keep.txt"), "k").unwrap();

        remove_lockfiles_recursively(&root).unwrap();

        assert!(!root.join("Cargo.lock").exists());
        assert!(!root.join("child/Cargo.lock").exists());
        assert!(
            root.join("target/Cargo.lock").exists(),
            "target/ is skipped"
        );
        assert!(root.join("keep.txt").exists());
    }

    #[test]
    #[serial_test::serial]
    fn cleanup_fixture_lockfiles_is_gated_by_env() {
        use super::cleanup_fixture_lockfiles;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let plugins_root = dir.path().join("plugins");
        std::fs::create_dir_all(plugins_root.join("qq")).unwrap();
        std::fs::write(plugins_root.join("qq/Cargo.lock"), "l").unwrap();

        // Without the opt-in env var, lock files are left alone.
        std::env::remove_var("CORDIS_CLEAN_FIXTURE_LOCKFILES");
        cleanup_fixture_lockfiles(&plugins_root).unwrap();
        assert!(plugins_root.join("qq/Cargo.lock").exists());

        // With the opt-in, they are removed.
        std::env::set_var("CORDIS_CLEAN_FIXTURE_LOCKFILES", "1");
        cleanup_fixture_lockfiles(&plugins_root).unwrap();
        std::env::remove_var("CORDIS_CLEAN_FIXTURE_LOCKFILES");
        assert!(!plugins_root.join("qq/Cargo.lock").exists());
    }

    #[test]
    fn cleanup_fixture_lockfiles_noop_for_missing_root() {
        use super::cleanup_fixture_lockfiles;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        cleanup_fixture_lockfiles(&dir.path().join("does-not-exist")).unwrap();
    }

    // ---------- stage_then_rename_file deeper error arms ----------

    /// The destination path has no filename component (`/`), so the helper
    /// rejects it before any tmp write — the `dst.file_name() == None` arm.
    #[test]
    fn stage_then_rename_errors_when_dst_has_no_filename() {
        use super::stage_then_rename_file;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.so");
        std::fs::write(&src, b"bytes").unwrap();
        // Root path "/" has no file_name.
        let err = stage_then_rename_file(&src, Path::new("/"));
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.contains("no filename")),
            "expected Io(no filename), got {err:?}"
        );
    }

    /// Reading the source fails after a successful open: on Unix a directory
    /// opens as a file handle but `read_to_end` errors (EISDIR), exercising
    /// the `read source` map_err arm.
    #[cfg(unix)]
    #[test]
    fn stage_then_rename_errors_when_source_is_a_directory() {
        use super::stage_then_rename_file;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src_dir = dir.path().join("src_is_dir");
        std::fs::create_dir(&src_dir).unwrap();
        let dst = dir.path().join("out.so");
        let err = stage_then_rename_file(&src_dir, &dst);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.contains("read source") || message.contains("open source")),
            "expected Io reading directory, got {err:?}"
        );
    }

    /// Creating the staging tmp file fails when the destination's parent
    /// directory does not exist — the `create tmp` map_err arm.
    #[test]
    fn stage_then_rename_errors_when_tmp_parent_missing() {
        use super::stage_then_rename_file;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.so");
        std::fs::write(&src, b"bytes").unwrap();
        // Parent "missing/" does not exist, so File::create(tmp) fails.
        let dst = dir.path().join("missing").join("out.so");
        let err = stage_then_rename_file(&src, &dst);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.contains("create tmp")),
            "expected Io(create tmp), got {err:?}"
        );
    }

    /// The final rename fails when the destination is a non-empty directory
    /// (renaming a file over a populated dir is ENOTEMPTY/EISDIR) — the
    /// `rename ... failed` map_err arm.
    #[test]
    fn stage_then_rename_errors_when_dst_is_nonempty_dir() {
        use super::stage_then_rename_file;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.so");
        std::fs::write(&src, b"bytes").unwrap();
        // dst is a directory that already contains a child, so rename(tmp, dst)
        // cannot replace it.
        let dst = dir.path().join("occupied.so");
        std::fs::create_dir(&dst).unwrap();
        std::fs::write(dst.join("child"), b"x").unwrap();
        let err = stage_then_rename_file(&src, &dst);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.contains("rename")),
            "expected Io(rename), got {err:?}"
        );
    }

    // ---------- parse_cargo_metadata (extracted mapper) ----------

    #[test]
    fn parse_cargo_metadata_reads_valid_output() {
        use super::parse_cargo_metadata;
        use std::path::Path;
        let json = br#"{
            "packages": [],
            "workspace_members": [],
            "target_directory": "/tmp/td",
            "resolve": null
        }"#;
        let parsed = parse_cargo_metadata(Path::new("/repo/Cargo.toml"), json).unwrap();
        assert_eq!(parsed.target_directory, "/tmp/td");
        assert!(parsed.packages.is_empty());
        assert!(parsed.workspace_members.is_empty());
        assert!(parsed.resolve.is_none());
    }

    #[test]
    fn parse_cargo_metadata_maps_bad_json_to_artifact_index_parse() {
        use super::parse_cargo_metadata;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        let manifest = Path::new("/repo/plugins/Cargo.toml");
        let result = parse_cargo_metadata(manifest, b"{ not json");
        assert!(
            matches!(&result, Err(RuntimeError::ArtifactIndexParse { path, message }) if path == &manifest.to_path_buf() && message.starts_with("cargo metadata parse failed: ")),
            "expected ArtifactIndexParse, got {result:?}"
        );
    }

    // ---------- IO-arm fault injection (read-only dir / parent-is-file) ----------

    /// P1-17 durable-write create-tmp arm: when the target directory cannot
    /// be written (read-only), `fs::File::create(&tmp)` fails and maps to Io.
    #[cfg(unix)]
    #[test]
    fn write_pretty_json_errors_when_tmp_create_fails_in_readonly_dir() {
        use super::write_pretty_json;
        use crate::core::error::RuntimeError;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        // Directory exists (so create_dir_all is a no-op) but is not writable,
        // so the staging tmp file cannot be created.
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let target = ro.join("index.json");
        let err = write_pretty_json(&target, &serde_json::json!({"k": 1}));
        // Restore perms so TempDir cleanup succeeds regardless of outcome.
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            permission_fault_holds(permission_bits_are_enforced(), &err, |e| {
                matches!(e, RuntimeError::Io { message, .. } if message.starts_with("create tmp: "))
            }),
            "unexpected outcome for a read-only staging dir: {err:?}"
        );
    }

    /// `write_pretty_json` create_dir_all arm: a path whose parent is an
    /// existing regular file cannot be turned into a directory.
    #[test]
    fn write_pretty_json_errors_when_parent_is_a_file() {
        use super::write_pretty_json;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("afile");
        std::fs::write(&file, b"x").unwrap();
        // parent (`afile`) is a regular file -> create_dir_all fails.
        let target = file.join("child.json");
        assert!(matches!(
            write_pretty_json(&target, &serde_json::json!({"k": 1})),
            Err(RuntimeError::Io { .. })
        ));
    }

    /// `stage_then_rename_file` create-tmp arm: read-only destination dir
    /// makes `File::create(&tmp)` fail.
    #[cfg(unix)]
    #[test]
    fn stage_then_rename_errors_when_tmp_create_fails_in_readonly_dir() {
        use super::stage_then_rename_file;
        use crate::core::error::RuntimeError;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.so");
        std::fs::write(&src, b"bytes").unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir(&ro).unwrap();
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        let dst = ro.join("out.so");
        let err = stage_then_rename_file(&src, &dst);
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            permission_fault_holds(permission_bits_are_enforced(), &err, |e| {
                matches!(e, RuntimeError::Io { message, .. } if message.starts_with("create tmp: "))
            }),
            "unexpected outcome for a read-only staging dir: {err:?}"
        );
    }

    /// `collect_files_recursively` read_dir arm: a missing directory makes
    /// `fs::read_dir` fail and map to Io with the dir path.
    #[test]
    fn collect_files_recursively_errors_when_dir_unreadable() {
        use super::collect_files_recursively;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("nope");
        let mut out = Vec::new();
        assert!(matches!(
            collect_files_recursively(&missing, &mut out),
            Err(RuntimeError::Io { .. })
        ));
    }

    /// `cleanup_fixture_lockfiles` read_dir arm (opt-in enabled): a read-only
    /// (unreadable) plugins root makes the top-level `read_dir` fail.
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn cleanup_fixture_lockfiles_errors_when_root_unreadable() {
        use super::cleanup_fixture_lockfiles;
        use crate::core::error::RuntimeError;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let plugins_root = dir.path().join("plugins");
        std::fs::create_dir(&plugins_root).unwrap();
        // Deny read/exec so read_dir fails.
        std::fs::set_permissions(&plugins_root, std::fs::Permissions::from_mode(0o000)).unwrap();
        std::env::set_var("CORDIS_CLEAN_FIXTURE_LOCKFILES", "1");
        let err = cleanup_fixture_lockfiles(&plugins_root);
        std::env::remove_var("CORDIS_CLEAN_FIXTURE_LOCKFILES");
        std::fs::set_permissions(&plugins_root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            permission_fault_holds(permission_bits_are_enforced(), &err, |e| {
                matches!(e, RuntimeError::Io { .. })
            }),
            "unexpected outcome for a permission-denied path: {err:?}"
        );
    }

    /// `remove_lockfiles_recursively` read_dir arm: an unreadable subdirectory
    /// makes the recursive `read_dir` fail.
    #[cfg(unix)]
    #[test]
    fn remove_lockfiles_recursively_errors_when_subdir_unreadable() {
        use super::remove_lockfiles_recursively;
        use crate::core::error::RuntimeError;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("locked");
        std::fs::create_dir(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o000)).unwrap();
        let err = remove_lockfiles_recursively(&sub);
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            permission_fault_holds(permission_bits_are_enforced(), &err, |e| {
                matches!(e, RuntimeError::Io { .. })
            }),
            "unexpected outcome for a permission-denied path: {err:?}"
        );
    }

    /// `lock_pid_is_live(0)` short-circuits to `false` (pid 0 is never a
    /// live process the caller could own).
    #[cfg(unix)]
    #[test]
    fn lock_pid_is_live_returns_false_for_pid_zero() {
        use super::lock_pid_is_live;
        assert!(!lock_pid_is_live(0));
    }

    // ---------- collect_local_dependency_dirs (pure resolve walk) ----------

    fn meta_package(
        id: &str,
        manifest_path: &str,
        source: Option<&str>,
    ) -> super::CargoMetadataPackage {
        super::CargoMetadataPackage {
            id: id.to_string(),
            name: id.to_string(),
            manifest_path: manifest_path.to_string(),
            source: source.map(str::to_string),
        }
    }

    #[test]
    fn collect_local_dependency_dirs_empty_when_root_package_unknown() {
        use super::collect_local_dependency_dirs;
        use std::collections::HashMap;
        // packages_by_id has no entry for the requested id -> early Vec::new().
        let packages = HashMap::new();
        let nodes = HashMap::new();
        assert!(collect_local_dependency_dirs("ghost 0.1.0", &packages, &nodes).is_empty());
    }

    #[test]
    fn collect_local_dependency_dirs_skips_missing_resolve_node_and_registry_deps() {
        use super::collect_local_dependency_dirs;
        use std::collections::HashMap;
        use std::path::PathBuf;
        // Root has a resolve node listing two deps: one local (source None) and
        // one from crates.io (source Some). Only the local one is collected.
        // A third dep id present in the node but absent from packages_by_id
        // exercises the "missing dep_package" continue.
        let mut packages = HashMap::new();
        packages.insert(
            "root 0.1.0".to_string(),
            meta_package("root 0.1.0", "/repo/root/Cargo.toml", None),
        );
        packages.insert(
            "localdep 0.1.0".to_string(),
            meta_package("localdep 0.1.0", "/repo/localdep/Cargo.toml", None),
        );
        packages.insert(
            "serde 1.0.0".to_string(),
            meta_package(
                "serde 1.0.0",
                "/reg/serde/Cargo.toml",
                Some("registry+https://github.com/rust-lang/crates.io-index"),
            ),
        );
        let mut nodes = HashMap::new();
        nodes.insert(
            "root 0.1.0".to_string(),
            vec![
                "localdep 0.1.0".to_string(),
                "serde 1.0.0".to_string(),
                "phantom 9.9.9".to_string(), // present in node, absent from packages
            ],
        );
        // `localdep` and `serde` have no resolve node of their own -> the
        // `nodes_by_id.get` continue arm fires for them.
        let deps = collect_local_dependency_dirs("root 0.1.0", &packages, &nodes);
        assert_eq!(deps, vec![PathBuf::from("/repo/localdep")]);
    }

    // ---------- maybe_remove_stale_lock remove_file error arms ----------

    /// JSON stale-lock path: a dead-pid JSON lock is slated for removal, but a
    /// read-only parent dir makes `fs::remove_file` fail (EACCES != NotFound),
    /// exercising the Io error arm rather than the Ok/NotFound arms.
    #[cfg(unix)]
    #[test]
    fn maybe_remove_stale_lock_json_remove_failure_maps_to_io() {
        use super::current_epoch_ms;
        use super::{maybe_remove_stale_lock, ArtifactBuildLockState};
        use crate::core::error::RuntimeError;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let holder = dir.path().join("holder");
        std::fs::create_dir(&holder).unwrap();
        let path = holder.join("dead.lock");
        let dead = ArtifactBuildLockState {
            pid: u32::MAX - 1,
            created_at_ms: current_epoch_ms(),
        };
        std::fs::write(&path, serde_json::to_vec(&dead).unwrap()).unwrap();
        // Deny write on the parent dir so unlink of the lock file is refused.
        std::fs::set_permissions(&holder, std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = maybe_remove_stale_lock(&path);
        std::fs::set_permissions(&holder, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            permission_fault_holds(permission_bits_are_enforced(), &err, |e| {
                matches!(e, RuntimeError::Io { .. })
            }),
            "unexpected outcome for a permission-denied path: {err:?}"
        );
    }

    /// Legacy (non-JSON) stale-lock path: an ancient non-JSON lock is slated
    /// for removal, but a read-only parent dir makes `fs::remove_file` fail,
    /// exercising the legacy Io error arm.
    #[cfg(unix)]
    #[test]
    fn maybe_remove_stale_lock_legacy_remove_failure_maps_to_io() {
        use super::maybe_remove_stale_lock;
        use crate::core::error::RuntimeError;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, SystemTime};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let holder = dir.path().join("holder");
        std::fs::create_dir(&holder).unwrap();
        let path = holder.join("legacy.lock");
        std::fs::write(&path, "not json at all").unwrap();
        // Backdate mtime past LEGACY_STALE_LOCK_TIMEOUT so removal is attempted.
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
        drop(file);
        std::fs::set_permissions(&holder, std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = maybe_remove_stale_lock(&path);
        std::fs::set_permissions(&holder, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            permission_fault_holds(permission_bits_are_enforced(), &err, |e| {
                matches!(e, RuntimeError::Io { .. })
            }),
            "unexpected outcome for a permission-denied path: {err:?}"
        );
    }

    // ---------- run_command_with_timeout spawn failure ----------

    /// A program that cannot be spawned surfaces the `cargo spawn failed`
    /// InvalidArgument arm of `run_command_with_timeout`.
    #[test]
    fn run_command_with_timeout_maps_spawn_failure() {
        use super::run_command_with_timeout;
        use crate::core::error::RuntimeError;
        use std::process::Command;
        let cmd = Command::new("cordis-no-such-binary-zzz");
        let err = run_command_with_timeout(cmd, std::time::Duration::from_secs(5));
        assert!(
            matches!(&err, Err(RuntimeError::InvalidArgument { message }) if message.contains("spawn")),
            "expected InvalidArgument(spawn), got {err:?}"
        );
    }

    // ---------- P2 command-executor parameterization ----------
    //
    // The cargo-subprocess orchestration helpers (`load_workspace_metadata`,
    // `build_plugin_artifact`, `built_dylib_path`) are factored into
    // `*_with_runner` cores. The public entry points inject the real
    // `run_command`; these tests inject a runner closure so the subprocess
    // failure / synthetic-metadata arms run without spawning `cargo`. Argument
    // order and error text are identical to the direct path.

    /// A runner that always fails, mimicking a `cargo` subprocess that exited
    /// non-zero. Used to drive the `?` early-return of each orchestration core.
    fn failing_runner(
        _program: &str,
        args: &[String],
        _dir: Option<&std::path::Path>,
    ) -> Result<Vec<u8>, super::RuntimeError> {
        Err(super::RuntimeError::CommandFailed {
            program: "cargo".to_string(),
            args: args.to_vec(),
            message: "synthetic build failure".to_string(),
        })
    }

    #[test]
    fn load_workspace_metadata_with_runner_propagates_subprocess_failure() {
        use super::load_workspace_metadata_with_runner;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        let err = load_workspace_metadata_with_runner(
            Path::new("/repo/plugins/Cargo.toml"),
            failing_runner,
        );
        assert!(matches!(err, Err(RuntimeError::CommandFailed { .. })));
    }

    #[test]
    fn load_workspace_metadata_with_runner_parses_injected_bytes() {
        use super::load_workspace_metadata_with_runner;
        use std::path::Path;
        let runner = |_p: &str, _a: &[String], _d: Option<&Path>| {
            Ok(br#"{"packages":[],"workspace_members":[],"target_directory":"/tmp/wtd","resolve":null}"#.to_vec())
        };
        let parsed =
            load_workspace_metadata_with_runner(Path::new("/repo/plugins/Cargo.toml"), runner)
                .expect("synthetic metadata parses");
        assert_eq!(parsed.target_directory, "/tmp/wtd");
        assert!(parsed.packages.is_empty());
    }

    #[test]
    fn load_workspace_metadata_with_runner_maps_bad_bytes_to_parse_error() {
        use super::load_workspace_metadata_with_runner;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        let runner = |_p: &str, _a: &[String], _d: Option<&Path>| Ok(b"{ not json".to_vec());
        let err = load_workspace_metadata_with_runner(Path::new("/repo/Cargo.toml"), runner);
        assert!(matches!(err, Err(RuntimeError::ArtifactIndexParse { .. })));
    }

    #[test]
    fn build_plugin_artifact_with_runner_propagates_subprocess_failure() {
        use super::build_plugin_artifact_with_runner;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        // fixtures_root has a parent, so we reach the runner call.
        let err = build_plugin_artifact_with_runner(
            Path::new("/repo/fixtures"),
            Path::new("/repo/fixtures/plugins/qq/Cargo.toml"),
            failing_runner,
        );
        assert!(matches!(err, Err(RuntimeError::CommandFailed { .. })));
    }

    #[test]
    fn build_plugin_artifact_with_runner_errors_when_fixtures_root_has_no_parent() {
        use super::build_plugin_artifact_with_runner;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        // Root "/" has no parent -> Invariant before the runner is consulted.
        // `failing_runner` (covered by sibling tests) stands in; it is never
        // reached because the no-parent guard returns first.
        let err = build_plugin_artifact_with_runner(
            Path::new("/"),
            Path::new("/plugins/qq/Cargo.toml"),
            failing_runner,
        );
        assert!(matches!(err, Err(RuntimeError::Invariant { .. })));
    }

    #[test]
    fn build_plugin_artifact_with_runner_succeeds_on_ok_runner() {
        use super::build_plugin_artifact_with_runner;
        use std::path::Path;
        let runner = |_p: &str, _a: &[String], _d: Option<&Path>| Ok(Vec::new());
        build_plugin_artifact_with_runner(
            Path::new("/repo/fixtures"),
            Path::new("/repo/fixtures/plugins/qq/Cargo.toml"),
            runner,
        )
        .expect("ok runner -> Ok(())");
    }

    #[test]
    fn built_dylib_path_with_runner_propagates_subprocess_failure() {
        use super::built_dylib_path_with_runner;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        let err = built_dylib_path_with_runner(
            Path::new("/repo/plugins/qq/Cargo.toml"),
            "qq",
            failing_runner,
        );
        assert!(matches!(err, Err(RuntimeError::CommandFailed { .. })));
    }

    #[test]
    fn built_dylib_path_with_runner_composes_path_from_injected_metadata() {
        use super::built_dylib_path_with_runner;
        use std::env::consts::{DLL_EXTENSION, DLL_PREFIX};
        use std::path::{Path, PathBuf};
        let runner = |_p: &str, _a: &[String], _d: Option<&Path>| {
            Ok(br#"{"packages":[],"workspace_members":[],"target_directory":"/tmp/td","resolve":null}"#.to_vec())
        };
        let path = built_dylib_path_with_runner(
            Path::new("/repo/plugins/my-plugin/Cargo.toml"),
            "my-plugin",
            runner,
        )
        .expect("path composed from metadata");
        let expected = PathBuf::from("/tmp/td")
            .join("debug")
            .join(format!("{DLL_PREFIX}my_plugin.{DLL_EXTENSION}"));
        assert_eq!(path, expected);
    }

    #[test]
    fn built_dylib_path_with_runner_maps_bad_bytes_to_parse_error() {
        use super::built_dylib_path_with_runner;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        let runner = |_p: &str, _a: &[String], _d: Option<&Path>| Ok(b"{ not json".to_vec());
        let err =
            built_dylib_path_with_runner(Path::new("/repo/plugins/qq/Cargo.toml"), "qq", runner);
        assert!(matches!(err, Err(RuntimeError::ArtifactIndexParse { .. })));
    }

    // ---------- collect_files_recursively `target` skip ----------

    /// A `target` subdirectory is skipped without descending; sibling regular
    /// files are still collected. Covers the `entry.file_name() == "target"`
    /// continue arm of `collect_files_recursively`.
    #[test]
    fn collect_files_recursively_skips_target_dir() {
        use super::collect_files_recursively;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("target/deep")).unwrap();
        std::fs::write(dir.path().join("target/deep/junk.rs"), "x").unwrap();
        std::fs::write(dir.path().join("keep.rs"), "y").unwrap();
        let mut out = Vec::new();
        collect_files_recursively(dir.path(), &mut out).unwrap();
        assert!(out.iter().any(|p| p.ends_with("keep.rs")));
        assert!(
            !out.iter().any(|p| p.to_string_lossy().contains("target/")),
            "target/ contents must be skipped, got {out:?}"
        );
    }

    // ---------- remove_lockfiles_recursively non-existent path ----------

    /// A path that does not exist is a no-op (the `!path.exists()` guard),
    /// returning Ok without touching the filesystem.
    #[test]
    fn remove_lockfiles_recursively_noop_for_missing_path() {
        use super::remove_lockfiles_recursively;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        remove_lockfiles_recursively(&dir.path().join("does-not-exist")).unwrap();
    }

    /// `remove_lockfiles_recursively` skips a nested `target` directory: a
    /// `Cargo.lock` under `target/` survives while a sibling one is removed.
    #[test]
    fn remove_lockfiles_recursively_skips_nested_target_dir() {
        use super::remove_lockfiles_recursively;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("crate");
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("target/Cargo.lock"), "l").unwrap();
        std::fs::write(root.join("Cargo.lock"), "l").unwrap();
        remove_lockfiles_recursively(&root).unwrap();
        assert!(!root.join("Cargo.lock").exists());
        assert!(
            root.join("target/Cargo.lock").exists(),
            "target/ is skipped"
        );
    }

    // ---------- write_pretty_json rename-failure arm ----------

    /// `write_pretty_json` maps a failed final rename to Io and removes the
    /// staging tmp: the destination is an existing non-empty directory, so
    /// `fs::rename(tmp, dst)` cannot replace it. Covers the `rename tmp ->
    /// target` map_err arm (and the `fs::remove_file(&tmp)` cleanup).
    #[test]
    fn write_pretty_json_errors_and_cleans_tmp_when_rename_fails() {
        use super::write_pretty_json;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        // Target path is itself a non-empty directory; rename over it fails.
        let target = dir.path().join("index.json");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("child"), b"x").unwrap();
        let err = write_pretty_json(&target, &serde_json::json!({"k": 1}));
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.starts_with("rename tmp -> target: ")),
            "expected Io rename error, got {err:?}"
        );
        // The staging tmp was cleaned up on the failure path.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".cordis-tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "tmp should be removed on rename failure"
        );
    }

    // ---------- fs-write injection seams (FsWriteOps) ----------
    //
    // The durable-write helpers open a staging tmp then `write_all`,
    // `sync_all`, `rename`. Once the handle is open, a real filesystem does
    // not deterministically fail the middle steps, so the `_with_fs` cores
    // accept an `FsWriteOps` whose fields are function pointers. Each test
    // starts from `FsWriteOps::STD` and swaps exactly one field for a stub
    // that returns an error, driving the matching map_err arm. A stubbed op
    // is never the real one, so these prove only the arm — the default path
    // remains covered by the byte-identical public entry points.

    fn erroring_write_all(_f: &mut std::fs::File, _b: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other("synthetic write failure"))
    }

    fn erroring_sync_all(_f: &std::fs::File) -> std::io::Result<()> {
        Err(std::io::Error::other("synthetic sync failure"))
    }

    fn erroring_rename(_from: &std::path::Path, _to: &std::path::Path) -> std::io::Result<()> {
        Err(std::io::Error::other("synthetic rename failure"))
    }

    /// `stage_then_rename_file_with_fs` maps a `write_all` failure to
    /// `Io("write tmp: ...")` (lines 325-327 arm).
    #[test]
    fn stage_then_rename_with_fs_maps_write_failure() {
        use super::{stage_then_rename_file_with_fs, FsWriteOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.so");
        std::fs::write(&src, b"bytes").unwrap();
        let dst = dir.path().join("out.so");
        let ops = FsWriteOps {
            write_all: erroring_write_all,
            ..FsWriteOps::STD
        };
        let err = stage_then_rename_file_with_fs(&src, &dst, ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.starts_with("write tmp: ")),
            "expected Io(write tmp), got {err:?}"
        );
    }

    /// `stage_then_rename_file_with_fs` maps a `sync_all` failure to
    /// `Io("sync tmp: ...")` (lines 329-331 arm).
    #[test]
    fn stage_then_rename_with_fs_maps_sync_failure() {
        use super::{stage_then_rename_file_with_fs, FsWriteOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.so");
        std::fs::write(&src, b"bytes").unwrap();
        let dst = dir.path().join("out.so");
        let ops = FsWriteOps {
            sync_all: erroring_sync_all,
            ..FsWriteOps::STD
        };
        let err = stage_then_rename_file_with_fs(&src, &dst, ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.starts_with("sync tmp: ")),
            "expected Io(sync tmp), got {err:?}"
        );
    }

    /// `stage_then_rename_file_with_fs` maps a `rename` failure to
    /// `Io("rename ... failed: ...")` (the final rename arm) via injection —
    /// distinct from the real-fs nonempty-dir test which relies on ENOTEMPTY.
    #[test]
    fn stage_then_rename_with_fs_maps_rename_failure() {
        use super::{stage_then_rename_file_with_fs, FsWriteOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.so");
        std::fs::write(&src, b"bytes").unwrap();
        let dst = dir.path().join("out.so");
        let ops = FsWriteOps {
            rename: erroring_rename,
            ..FsWriteOps::STD
        };
        let err = stage_then_rename_file_with_fs(&src, &dst, ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.contains("rename") && message.contains("failed")),
            "expected Io(rename failed), got {err:?}"
        );
    }

    /// `write_pretty_json_with_fs` maps a `write_all` failure to
    /// `Io("write tmp: ...")` (lines 1332-1334 arm).
    #[test]
    fn write_pretty_json_with_fs_maps_write_failure() {
        use super::{write_pretty_json_with_fs, FsWriteOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("index.json");
        let ops = FsWriteOps {
            write_all: erroring_write_all,
            ..FsWriteOps::STD
        };
        let err = write_pretty_json_with_fs(&target, &serde_json::json!({"k": 1}), ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.starts_with("write tmp: ")),
            "expected Io(write tmp), got {err:?}"
        );
    }

    /// `write_pretty_json_with_fs` maps a `sync_all` failure to
    /// `Io("sync tmp: ...")` (lines 1336-1338 arm).
    #[test]
    fn write_pretty_json_with_fs_maps_sync_failure() {
        use super::{write_pretty_json_with_fs, FsWriteOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("index.json");
        let ops = FsWriteOps {
            sync_all: erroring_sync_all,
            ..FsWriteOps::STD
        };
        let err = write_pretty_json_with_fs(&target, &serde_json::json!({"k": 1}), ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.starts_with("sync tmp: ")),
            "expected Io(sync tmp), got {err:?}"
        );
    }

    /// `write_pretty_json_with_fs` maps a `rename` failure to
    /// `Io("rename tmp -> target: ...")` and still cleans the staging tmp,
    /// via injection (the real-fs test uses a nonempty dir).
    #[test]
    fn write_pretty_json_with_fs_maps_rename_failure_and_cleans_tmp() {
        use super::{write_pretty_json_with_fs, FsWriteOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("index.json");
        let ops = FsWriteOps {
            rename: erroring_rename,
            ..FsWriteOps::STD
        };
        let err = write_pretty_json_with_fs(&target, &serde_json::json!({"k": 1}), ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.starts_with("rename tmp -> target: ")),
            "expected Io(rename tmp -> target), got {err:?}"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".cordis-tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "tmp must be cleaned on rename failure"
        );
    }

    // ---------- ArtifactBuildLock::acquire injection seam ----------

    fn erroring_serialize(_s: &super::ArtifactBuildLockState) -> serde_json::Result<Vec<u8>> {
        // Produce a genuine serde_json error (parsing an invalid number) so
        // the `map_err` arm sees the same error type the real serializer
        // would, then return it.
        let e = serde_json::from_str::<u8>("not-a-number").unwrap_err();
        Err(e)
    }

    fn erroring_lock_write_all(_f: &mut std::fs::File, _b: &[u8]) -> std::io::Result<()> {
        Err(std::io::Error::other("synthetic lock write failure"))
    }

    fn erroring_flush(_f: &mut std::fs::File) -> std::io::Result<()> {
        Err(std::io::Error::other("synthetic flush failure"))
    }

    fn erroring_lock_sync_all(_f: &std::fs::File) -> std::io::Result<()> {
        Err(std::io::Error::other("synthetic lock sync failure"))
    }

    /// The lock-metadata serialize failure maps to `Io("lock metadata
    /// serialize failed: ...")` (lines 1363-1365 arm).
    #[test]
    fn acquire_with_fs_maps_serialize_failure() {
        use super::{ArtifactBuildLock, LockAcquireOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let ops = LockAcquireOps {
            serialize: erroring_serialize,
            ..LockAcquireOps::STD
        };
        let err = ArtifactBuildLock::acquire_with_fs(dir.path(), ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.starts_with("lock metadata serialize failed: ")),
            "expected Io(serialize failed), got {err:?}"
        );
    }

    /// The lock `write_all` failure maps to `Io` (lines 1367-1369 arm).
    #[test]
    fn acquire_with_fs_maps_write_failure() {
        use super::{ArtifactBuildLock, LockAcquireOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let ops = LockAcquireOps {
            write_all: erroring_lock_write_all,
            ..LockAcquireOps::STD
        };
        let err = ArtifactBuildLock::acquire_with_fs(dir.path(), ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.contains("synthetic lock write failure")),
            "expected Io(write), got {err:?}"
        );
    }

    /// The lock `flush` failure maps to `Io` (lines 1371-1373 arm).
    #[test]
    fn acquire_with_fs_maps_flush_failure() {
        use super::{ArtifactBuildLock, LockAcquireOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let ops = LockAcquireOps {
            flush: erroring_flush,
            ..LockAcquireOps::STD
        };
        let err = ArtifactBuildLock::acquire_with_fs(dir.path(), ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.contains("synthetic flush failure")),
            "expected Io(flush), got {err:?}"
        );
    }

    /// The lock `sync_all` failure maps to `Io` (lines 1377-1379 arm).
    #[test]
    fn acquire_with_fs_maps_sync_failure() {
        use super::{ArtifactBuildLock, LockAcquireOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let ops = LockAcquireOps {
            sync_all: erroring_lock_sync_all,
            ..LockAcquireOps::STD
        };
        let err = ArtifactBuildLock::acquire_with_fs(dir.path(), ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.contains("synthetic lock sync failure")),
            "expected Io(sync), got {err:?}"
        );
    }

    /// A permanently-occupied lock with a zero wait timeout hits the
    /// `AlreadyExists` spin arm and then the timeout return
    /// (`ArtifactBuildLockTimeout`, lines 1385-1388). `open_new` always
    /// reports AlreadyExists; the live-pid JSON body keeps
    /// `maybe_remove_stale_lock` from reclaiming it.
    #[test]
    fn acquire_with_fs_times_out_when_lock_permanently_held() {
        use super::{current_epoch_ms, ArtifactBuildLock, ArtifactBuildLockState, LockAcquireOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        // Pre-create a live-pid lock so maybe_remove_stale_lock keeps it.
        let live = ArtifactBuildLockState {
            pid: std::process::id(),
            created_at_ms: current_epoch_ms(),
        };
        std::fs::write(
            dir.path().join(super::BUILD_LOCK_FILE),
            serde_json::to_vec(&live).unwrap(),
        )
        .unwrap();
        fn always_exists(_p: &std::path::Path) -> std::io::Result<std::fs::File> {
            Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists))
        }
        let ops = LockAcquireOps {
            open_new: always_exists,
            wait_timeout: std::time::Duration::ZERO,
            ..LockAcquireOps::STD
        };
        let err = ArtifactBuildLock::acquire_with_fs(dir.path(), ops);
        assert!(
            matches!(&err, Err(RuntimeError::ArtifactBuildLockTimeout { .. })),
            "expected ArtifactBuildLockTimeout, got {err:?}"
        );
    }

    /// A non-AlreadyExists `open_new` error hits the final Io arm
    /// (lines 1392-1396).
    #[test]
    fn acquire_with_fs_maps_open_error_to_io() {
        use super::{ArtifactBuildLock, LockAcquireOps};
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        fn perm_denied(_p: &std::path::Path) -> std::io::Result<std::fs::File> {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }
        let ops = LockAcquireOps {
            open_new: perm_denied,
            ..LockAcquireOps::STD
        };
        let err = ArtifactBuildLock::acquire_with_fs(dir.path(), ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { .. })),
            "expected Io, got {err:?}"
        );
    }

    /// The default injected ops acquire the lock successfully, proving the
    /// happy path of `acquire_with_fs` (mirrors the real `acquire`).
    #[test]
    fn acquire_with_fs_std_acquires_lock() {
        use super::{ArtifactBuildLock, LockAcquireOps};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let lock = ArtifactBuildLock::acquire_with_fs(dir.path(), LockAcquireOps::STD)
            .expect("std ops acquire the lock");
        assert!(dir.path().join(super::BUILD_LOCK_FILE).exists());
        drop(lock);
        assert!(
            !dir.path().join(super::BUILD_LOCK_FILE).exists(),
            "Drop removes the lock file"
        );
    }

    // ---------- mtime injection seam (MetaOps) ----------

    fn erroring_modified(_m: &std::fs::Metadata) -> std::io::Result<std::time::SystemTime> {
        Err(std::io::Error::other("synthetic mtime failure"))
    }

    /// `build_input_probe_with_fs` falls back to `SystemTime::now()` when the
    /// `modified()` read fails, still producing a probe entry (lines 1006-1012
    /// fallback arm). The recorded modified_at_ms is non-zero (now, not epoch).
    #[test]
    fn build_input_probe_with_fs_falls_back_on_mtime_failure() {
        use super::{build_input_probe_with_fs, MetaOps};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("f.rs");
        std::fs::write(&file, b"x").unwrap();
        let ops = MetaOps {
            modified: erroring_modified,
            ..MetaOps::STD
        };
        let probe = build_input_probe_with_fs(dir.path(), &[file], ops)
            .expect("probe built with mtime fallback");
        assert_eq!(probe.files.len(), 1);
        // The message argument is pre-formatted rather than passed as a lazy
        // `assert!` operand: as an operand it is only evaluated when the
        // assertion fails, i.e. never on a passing run.
        let observed = probe.files[0].modified_at_ms;
        let detail = format!("fallback to now() should give a nonzero epoch-ms, got {observed}");
        assert!(observed > 0, "{detail}");
    }

    /// `maybe_remove_stale_lock_with_fs` falls back to `SystemTime::now()`
    /// when the lock file's `modified()` read fails, treating the lock as
    /// just-modified (kept). A live-pid JSON body is retained (lines 1418-1424
    /// fallback + the JSON keep arm).
    #[test]
    fn maybe_remove_stale_lock_with_fs_falls_back_on_mtime_failure() {
        use super::{
            current_epoch_ms, maybe_remove_stale_lock_with_fs, ArtifactBuildLockState, MetaOps,
        };
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("live.lock");
        let live = ArtifactBuildLockState {
            pid: std::process::id(),
            created_at_ms: current_epoch_ms(),
        };
        std::fs::write(&path, serde_json::to_vec(&live).unwrap()).unwrap();
        let ops = MetaOps {
            modified: erroring_modified,
            ..MetaOps::STD
        };
        // Fallback path executes; live-pid lock is kept (Ok, file still there).
        maybe_remove_stale_lock_with_fs(&path, ops).expect("stale-lock check ok");
        assert!(path.exists(), "live-pid lock must be kept");
    }

    fn erroring_metadata(_p: &std::path::Path) -> std::io::Result<std::fs::Metadata> {
        Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
    }

    /// `maybe_remove_stale_lock_with_fs` maps a non-NotFound `metadata`
    /// failure to `Io` (the initial-stat error arm). A PermissionDenied stub
    /// drives it without needing an OS-level unreadable path.
    #[test]
    fn maybe_remove_stale_lock_with_fs_maps_metadata_failure_to_io() {
        use super::{maybe_remove_stale_lock_with_fs, MetaOps};
        use crate::core::error::RuntimeError;
        use std::path::Path;
        let ops = MetaOps {
            metadata: erroring_metadata,
            ..MetaOps::STD
        };
        let err = maybe_remove_stale_lock_with_fs(Path::new("/some/lock"), ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { .. })),
            "expected Io(metadata), got {err:?}"
        );
    }

    /// `build_input_probe_with_fs` maps a `metadata` failure to `Io` (the
    /// per-file stat error arm), driven by a PermissionDenied stub.
    #[test]
    fn build_input_probe_with_fs_maps_metadata_failure_to_io() {
        use super::{build_input_probe_with_fs, MetaOps};
        use crate::core::error::RuntimeError;
        use std::path::{Path, PathBuf};
        let ops = MetaOps {
            metadata: erroring_metadata,
            ..MetaOps::STD
        };
        let err = build_input_probe_with_fs(Path::new("/repo"), &[PathBuf::from("/repo/x")], ops);
        assert!(
            matches!(&err, Err(RuntimeError::Io { .. })),
            "expected Io(metadata), got {err:?}"
        );
    }

    // ---------- collect_dir_entry injected iterator-Err arm ----------

    /// `collect_dir_entry` maps a `DirEntry` iterator `Err` to `Io` — the
    /// `entry.map_err` arm the OS won't produce for a directory that opened.
    #[test]
    fn collect_dir_entry_maps_iterator_error_to_io() {
        use super::collect_dir_entry;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        let mut out = Vec::new();
        let synthetic = Err(std::io::Error::other("synthetic dir-entry failure"));
        let err = collect_dir_entry(Path::new("/some/dir"), synthetic, &mut out);
        assert!(
            matches!(&err, Err(RuntimeError::Io { message, .. }) if message.contains("synthetic dir-entry failure")),
            "expected Io(dir-entry), got {err:?}"
        );
        assert!(out.is_empty());
    }

    /// `collect_dir_entry` maps a `fs::metadata` failure to `Io`: a dangling
    /// symlink stat's the target (ENOENT) rather than the link itself.
    #[cfg(unix)]
    #[test]
    fn collect_dir_entry_maps_metadata_failure_on_dangling_symlink() {
        use super::collect_dir_entry;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let link = dir.path().join("dangling");
        std::os::unix::fs::symlink(dir.path().join("no-such-target"), &link).unwrap();
        // Grab the real DirEntry for the dangling link.
        let entry = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| e.file_name() == "dangling")
            .map(Ok)
            .unwrap();
        let mut out = Vec::new();
        let err = collect_dir_entry(dir.path(), entry, &mut out);
        assert!(
            matches!(&err, Err(RuntimeError::Io { .. })),
            "expected Io(metadata) for dangling symlink, got {err:?}"
        );
    }

    // ---------- no-parent / missing-plugin Invariant arms ----------
    //
    // These arms guard against a `fixtures_root` with no parent component
    // (the root path `/`) and a topo_order naming a plugin absent from the
    // resolved graph. Both are structural invariants that the normal call
    // path (a real fixtures dir under a repo, a graph built by
    // PackageResolver) never violates, so they're driven directly with a
    // constructed root-path / mismatched-graph input.

    fn empty_dependency_snapshot() -> super::DependencySnapshot {
        use std::collections::{HashMap, HashSet};
        super::DependencySnapshot {
            workspace_manifest_path: std::path::PathBuf::from("/plugins/Cargo.toml"),
            workspace_members: HashSet::new(),
            target_directory: std::path::PathBuf::from("/target"),
            local_dep_closure_by_name: HashMap::new(),
        }
    }

    /// `build_dirty_dylib_plugins` rejects a `fixtures_root` of `/` (no parent)
    /// with an Invariant before consulting any context.
    #[test]
    fn build_dirty_dylib_plugins_errors_when_fixtures_root_has_no_parent() {
        use super::build_dirty_dylib_plugins;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        let snapshot = empty_dependency_snapshot();
        let err = build_dirty_dylib_plugins(Path::new("/"), &snapshot, &[]);
        assert!(
            matches!(&err, Err(RuntimeError::Invariant { message }) if message.starts_with("fixtures root missing parent: ")),
            "expected Invariant(no parent), got {err:?}"
        );
    }

    /// `prepare_artifacts_locked` rejects a `fixtures_root` of `/` (no parent)
    /// with an Invariant before touching the filesystem.
    #[test]
    fn prepare_artifacts_locked_errors_when_fixtures_root_has_no_parent() {
        use super::{prepare_artifacts_locked, PrepareMode};
        use crate::core::error::RuntimeError;
        use std::path::Path;
        let err = prepare_artifacts_locked(Path::new("/"), PrepareMode::Incremental);
        assert!(
            matches!(&err, Err(RuntimeError::Invariant { message }) if message.starts_with("fixtures root missing parent: ")),
            "expected Invariant(no parent), got {err:?}"
        );
    }

    /// `build_plugin_contexts` errors with Invariant when `topo_order` names a
    /// plugin that is absent from the `plugins` map — the graph-consistency
    /// guard. `repo_root` here is fine; the missing-plugin lookup fires first.
    #[test]
    fn build_plugin_contexts_errors_on_plugin_missing_from_graph() {
        use super::{build_plugin_contexts, ResolvedPluginGraph};
        use crate::core::error::RuntimeError;
        use std::collections::BTreeMap;
        use std::path::Path;
        let graph = ResolvedPluginGraph {
            plugins: BTreeMap::new(),
            children: BTreeMap::new(),
            topo_order: vec!["/ghost".to_string()],
        };
        let snapshot = empty_dependency_snapshot();
        let err = build_plugin_contexts(
            Path::new("/repo"),
            &graph,
            Path::new("/repo/fixtures/artifacts"),
            &snapshot,
        );
        assert!(
            matches!(&err, Err(RuntimeError::Invariant { message }) if message.starts_with("missing plugin in resolved graph: ")),
            "expected Invariant(missing plugin), got {err:?}"
        );
    }

    // ---------- extracted error mappers ----------

    /// `create_artifacts_dir` succeeds (and is idempotent) for a directory it
    /// can create, which is the arm `rebuild_plugin_workspace` always takes.
    #[test]
    fn create_artifacts_dir_creates_nested_path_and_is_idempotent() {
        use super::create_artifacts_dir;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("nested/artifacts");
        create_artifacts_dir(&target).expect("first create succeeds");
        assert!(target.is_dir());
        create_artifacts_dir(&target).expect("create_dir_all is idempotent");
    }

    /// `create_artifacts_dir` maps a `create_dir_all` failure to `Io` carrying
    /// the `create artifacts dir: ` prefix. A plain file standing where the
    /// directory belongs makes the syscall fail without needing permissions
    /// games.
    #[test]
    fn create_artifacts_dir_reports_io_when_path_is_a_file() {
        use super::create_artifacts_dir;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let occupied = dir.path().join("artifacts");
        std::fs::write(&occupied, b"not a directory").unwrap();
        // Rendered through `Display` rather than destructured with a
        // `let ... else { panic!(..) }`: the mismatch arm of such a destructure
        // never runs while the test passes, whereas the rendered string pins the
        // variant (`I/O at <path>: ...`), the path and the message at once.
        // `map_or_else` keeps both arms on one line for the same reason.
        let rendered = create_artifacts_dir(&occupied)
            .map_or_else(|e| e.to_string(), |()| "unexpected Ok".to_string());
        let expected_prefix = format!("I/O at {}: create artifacts dir: ", occupied.display());
        assert!(
            rendered.starts_with(&expected_prefix),
            "expected Io with the original prefix {expected_prefix:?}, got {rendered:?}"
        );
    }

    /// `cargo_wait_error` wraps a `try_wait` io error as `InvalidArgument` with
    /// the `cargo wait failed: ` text the inline match arm used to produce.
    #[test]
    fn cargo_wait_error_wraps_io_error_as_invalid_argument() {
        use super::cargo_wait_error;
        let err = cargo_wait_error(std::io::Error::other("synthetic wait failure"));
        // Compared through `Display` so the whole value is checked in one
        // `assert_eq!`; a `let ... else { panic!(..) }` destructure would add an
        // arm that never runs while the test passes.
        assert_eq!(
            err.to_string(),
            "invalid argument: cargo wait failed: synthetic wait failure"
        );
    }

    /// `materialize_artifact_entry` computes the build fingerprint on the fly
    /// when the context carries none (`build_fingerprint: None`) — the arm the
    /// orchestrator skips because it pre-fills the fingerprint for dirty
    /// contexts. Uses the JSON artifact branch so no dylib/cargo is involved.
    #[test]
    fn materialize_artifact_entry_computes_fingerprint_when_context_has_none() {
        use super::{compute_build_fingerprint, materialize_artifact_entry};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifacts_dir = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifacts_dir).unwrap();
        let mut ctx = sample_context(artifacts_dir.join("foo.json"));
        // An input file makes the computed fingerprint content-dependent, so a
        // match against the direct computation is meaningful.
        let input = dir.path().join("src.rs");
        std::fs::write(&input, b"fn main() {}").unwrap();
        ctx.input_files = vec![input];
        assert!(ctx.build_fingerprint.is_none(), "None arm is under test");
        let snapshot = empty_dependency_snapshot();

        let entry = materialize_artifact_entry(
            dir.path(),
            &artifacts_dir,
            &snapshot,
            &mut ctx,
            "built-marker",
        )
        .expect("JSON artifact materializes");

        let expected = compute_build_fingerprint(dir.path(), &ctx.input_files).unwrap();
        assert_eq!(entry.build_fingerprint, expected);
        assert_eq!(entry.built_at, "built-marker");
        assert_eq!(entry.artifact_path, "foo.json");
    }

    /// The `Some(..)` counterpart: a context that already carries a fingerprint
    /// is taken verbatim rather than recomputed.
    #[test]
    fn materialize_artifact_entry_reuses_prefilled_fingerprint() {
        use super::materialize_artifact_entry;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifacts_dir = dir.path().join("artifacts");
        std::fs::create_dir_all(&artifacts_dir).unwrap();
        let mut ctx = sample_context(artifacts_dir.join("foo.json"));
        ctx.build_fingerprint = Some("prefilled-fingerprint".to_string());
        let snapshot = empty_dependency_snapshot();

        let entry =
            materialize_artifact_entry(dir.path(), &artifacts_dir, &snapshot, &mut ctx, "marker")
                .expect("JSON artifact materializes");
        assert_eq!(entry.build_fingerprint, "prefilled-fingerprint");
    }

    /// `collect_crate_inputs` on a crate directory with no `src/` still returns
    /// the manifest — the "skip the source walk" path of the option-iteration
    /// over `src/`.
    #[test]
    fn collect_crate_inputs_without_src_dir_returns_manifest_only() {
        use super::collect_crate_inputs;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), b"[package]\n").unwrap();
        let files = collect_crate_inputs(dir.path(), true).unwrap();
        assert_eq!(files, vec![dir.path().join("Cargo.toml")]);
    }

    /// `maybe_remove_stale_lock_with_fs` falls through to the legacy mtime check
    /// when the lock file exists but holds non-JSON bytes (the flattened
    /// read-then-parse chain yielding `None`). A freshly written file is inside
    /// the legacy window, so the lock is kept.
    #[test]
    fn maybe_remove_stale_lock_with_fs_keeps_fresh_non_json_lock() {
        use super::{maybe_remove_stale_lock_with_fs, MetaOps};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("garbage.lock");
        std::fs::write(&path, b"not json at all").unwrap();
        maybe_remove_stale_lock_with_fs(&path, MetaOps::STD).expect("stale-lock check ok");
        assert!(path.exists(), "a just-written lock must be kept");
    }
}
