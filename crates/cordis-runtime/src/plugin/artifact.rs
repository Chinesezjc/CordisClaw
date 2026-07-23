use crate::core::error::RuntimeError;
use crate::core::models::{
    ArtifactIndex, ArtifactIndexEntry, PluginArtifact, PluginExecution,
    ARTIFACT_INDEX_SCHEMA_VERSION,
};
use crate::plugin::dynamic::{is_dylib_path, sidecar_json_path};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn load_artifact_index(path: &Path) -> Result<ArtifactIndex, RuntimeError> {
    let text = fs::read_to_string(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let index = serde_json::from_str::<ArtifactIndex>(&text).map_err(|e| {
        RuntimeError::ArtifactIndexParse {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    })?;
    if index.schema_version != ARTIFACT_INDEX_SCHEMA_VERSION {
        return Err(RuntimeError::ArtifactIndexParse {
            path: path.to_path_buf(),
            message: format!(
                "unsupported schema_version {}, expected {}",
                index.schema_version, ARTIFACT_INDEX_SCHEMA_VERSION
            ),
        });
    }
    Ok(index)
}

pub fn artifact_index_map(index: &ArtifactIndex) -> BTreeMap<String, ArtifactIndexEntry> {
    index
        .entries
        .iter()
        .cloned()
        .map(|entry| (entry.plugin_path.clone(), entry))
        .collect()
}

pub fn resolve_artifact_path(index_path: &Path, artifact_path: &str) -> PathBuf {
    let candidate = Path::new(artifact_path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        index_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(candidate)
    }
}

pub fn sha256_file(path: &Path) -> Result<String, RuntimeError> {
    let mut file = fs::File::open(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(|e| RuntimeError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn load_plugin_artifact(path: &Path) -> Result<PluginArtifact, RuntimeError> {
    let text = fs::read_to_string(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    serde_json::from_str::<PluginArtifact>(&text).map_err(|e| RuntimeError::ArtifactIndexParse {
        path: path.to_path_buf(),
        message: format!("artifact parse failed: {e}"),
    })
}

pub fn stage_artifact_bundle(
    plugin_path: &str,
    artifact_reference: &str,
    artifact_path: &Path,
    staged_root: &Path,
) -> Result<PathBuf, RuntimeError> {
    let staged_artifact_path =
        staged_artifact_path(plugin_path, artifact_reference, artifact_path, staged_root)?;
    stage_file(artifact_path, &staged_artifact_path)?;

    if is_dylib_path(artifact_path) {
        let original_sidecar = sidecar_json_path(artifact_path);
        if original_sidecar.exists() {
            let staged_sidecar = sidecar_json_path(&staged_artifact_path);
            stage_file(&original_sidecar, &staged_sidecar)?;
        }
        return Ok(staged_artifact_path);
    }

    let artifact = load_plugin_artifact(artifact_path)?;
    if let Some(PluginExecution::Process { command, .. }) = artifact.execution {
        stage_process_command(artifact_path, &staged_artifact_path, &command, staged_root)?;
    }

    Ok(staged_artifact_path)
}

fn staged_artifact_path(
    plugin_path: &str,
    artifact_reference: &str,
    artifact_path: &Path,
    staged_root: &Path,
) -> Result<PathBuf, RuntimeError> {
    let artifact_ref = Path::new(artifact_reference);
    let relative = if artifact_ref.is_absolute() {
        let file_name = artifact_path
            .file_name()
            .ok_or_else(|| RuntimeError::Invariant {
                message: format!(
                    "artifact path missing file name for plugin {plugin_path}: {}",
                    artifact_path.display()
                ),
            })?;
        PathBuf::from(plugin_path.replace('/', std::path::MAIN_SEPARATOR_STR)).join(file_name)
    } else {
        artifact_ref.to_path_buf()
    };

    Ok(staged_root.join(relative))
}

fn stage_process_command(
    original_artifact_path: &Path,
    staged_artifact_path: &Path,
    command: &str,
    staged_root: &Path,
) -> Result<(), RuntimeError> {
    let command_path = Path::new(command);
    if command_path.is_absolute() {
        return Ok(());
    }

    let source_path = original_artifact_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(command_path);
    let target_path = staged_artifact_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(command_path);

    let target_parent = target_path
        .parent()
        .ok_or_else(|| RuntimeError::Invariant {
            message: format!(
                "staged process command missing parent: {}",
                target_path.display()
            ),
        })?;
    fs::create_dir_all(target_parent).map_err(|e| RuntimeError::Io {
        path: target_parent.to_path_buf(),
        message: e.to_string(),
    })?;

    // P2-22: canonicalise both sides symmetrically. If one side succeeds
    // canonicalising (resolves symlinks) and the other doesn't, the
    // `starts_with` check would false-positive (e.g. after symlink
    // resolution `normalized_target` might drop a `..` segment that
    // still exists in the un-resolved `staged_root`). If either side
    // fails to canonicalise, fall through to the original path on
    // *both* sides so the check compares like-with-like.
    let normalized_target = target_parent.canonicalize();
    let normalized_root = staged_root.canonicalize();
    let (normalized_target, normalized_root) = match (normalized_target, normalized_root) {
        (Ok(t), Ok(r)) => (t, r),
        _ => (target_parent.to_path_buf(), staged_root.to_path_buf()),
    };
    if !normalized_target.starts_with(&normalized_root) {
        return Err(RuntimeError::Invariant {
            message: format!(
                "staged process command escapes snapshot root: {}",
                target_path.display()
            ),
        });
    }

    stage_file(&source_path, &target_path)
}

fn stage_file(source: &Path, target: &Path) -> Result<(), RuntimeError> {
    if source == target {
        return Ok(());
    }

    let target_parent = target.parent().ok_or_else(|| RuntimeError::Invariant {
        message: format!("staged artifact missing parent: {}", target.display()),
    })?;
    fs::create_dir_all(target_parent).map_err(|e| RuntimeError::Io {
        path: target_parent.to_path_buf(),
        message: e.to_string(),
    })?;

    // P0-15: previously did `remove_file(target)` then `hard_link/copy` —
    // two concurrent stagers race on that window and one may find the file
    // absent when they expected it, breaking a third invoker. Stage into a
    // sibling `<target>.cordis-staging.<pid>` first, then atomically rename
    // over `target`. The rename is the only writer visible from outside.
    let staging_name = match target.file_name() {
        Some(name) => {
            let mut owned = name.to_os_string();
            owned.push(format!(".cordis-staging.{}", std::process::id()));
            target.with_file_name(owned)
        }
        None => {
            return Err(RuntimeError::Invariant {
                message: format!("stage_file target has no filename: {}", target.display()),
            });
        }
    };
    // Clean up any stale staging file from a prior crash of this same pid.
    let _ = fs::remove_file(&staging_name);

    // Always COPY, never hard-link. A hard link shares the inode with the
    // source: any writer that later mutates the source IN PLACE (fs::write,
    // truncate — e.g. an external tool or a test fixture) silently rewrites
    // the staged snapshot copy too, breaking the "old snapshot stays
    // immutable" guarantee. Our own writers use tmp+rename (new inode) so
    // they happened not to trigger this, but snapshot isolation must not
    // depend on every writer being well-behaved.
    fs::copy(source, &staging_name)
        .map(|_| ())
        .map_err(|e| RuntimeError::Io {
            path: staging_name.clone(),
            message: e.to_string(),
        })?;
    fs::rename(&staging_name, target).map_err(|e| {
        // Clean up staging on rename failure.
        let _ = fs::remove_file(&staging_name);
        RuntimeError::Io {
            path: target.to_path_buf(),
            message: format!("rename staging -> target failed: {e}"),
        }
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::{
        AbiFingerprint, ArtifactIndex, ArtifactIndexEntry, ArtifactKind, PluginArtifact,
        PluginDocs, PluginExecution, ARTIFACT_INDEX_SCHEMA_VERSION,
    };
    use std::fs;
    use tempfile::TempDir;

    fn abi() -> AbiFingerprint {
        AbiFingerprint {
            rustc_version: "test".to_string(),
            target_triple: "x86_64-unknown-linux-gnu".to_string(),
            crate_hash: "deadbeef".to_string(),
            api_hash: "cafebabe".to_string(),
        }
    }

    fn docs(plugin_path: &str) -> PluginDocs {
        PluginDocs {
            plugin_id: plugin_path.replace('/', "_"),
            plugin_path: plugin_path.to_string(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 2,
            command_name: None,
            nodes: Vec::new(),
            system_hint: None,
        }
    }

    fn entry(plugin_path: &str, artifact_path: &str) -> ArtifactIndexEntry {
        ArtifactIndexEntry {
            plugin_path: plugin_path.to_string(),
            version: "0.1.0".to_string(),
            abi_fingerprint: abi(),
            artifact_path: artifact_path.to_string(),
            sha256: "0".repeat(64),
            built_at: "0".to_string(),
            parent: None,
            required: true,
            grants_from_parent: Vec::new(),
            docs: docs(plugin_path),
            exports: Vec::new(),
            execution: None,
            artifact_kind: ArtifactKind::Json,
            build_fingerprint: "bf".to_string(),
            input_probe: Default::default(),
            local_path_deps: Vec::new(),
        }
    }

    fn write_index(dir: &Path, index: &ArtifactIndex) -> PathBuf {
        let path = dir.join("index.json");
        fs::write(&path, serde_json::to_string_pretty(index).unwrap()).unwrap();
        path
    }

    // --- load_artifact_index -------------------------------------------------

    #[test]
    fn load_artifact_index_ok() {
        let tmp = TempDir::new().unwrap();
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["a".to_string()],
            entries: vec![entry("a", "a.json")],
        };
        let path = write_index(tmp.path(), &index);
        let loaded = load_artifact_index(&path).unwrap();
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.topo_order, vec!["a".to_string()]);
    }

    #[test]
    fn load_artifact_index_io_error() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.json");
        let err = load_artifact_index(&missing).unwrap_err();
        assert!(matches!(err, RuntimeError::Io { .. }));
    }

    #[test]
    fn load_artifact_index_parse_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.json");
        fs::write(&path, "{ not json").unwrap();
        let err = load_artifact_index(&path).unwrap_err();
        assert!(matches!(err, RuntimeError::ArtifactIndexParse { .. }));
    }

    #[test]
    fn load_artifact_index_schema_version_mismatch() {
        let tmp = TempDir::new().unwrap();
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION + 99,
            generated_at: "now".to_string(),
            topo_order: Vec::new(),
            entries: Vec::new(),
        };
        let path = write_index(tmp.path(), &index);
        let err = load_artifact_index(&path).unwrap_err();
        match err {
            RuntimeError::ArtifactIndexParse { message, .. } => {
                assert!(message.contains("unsupported schema_version"), "{message}");
            }
            other => panic!("expected ArtifactIndexParse, got {other:?}"),
        }
    }

    // --- artifact_index_map --------------------------------------------------

    #[test]
    fn artifact_index_map_keys_by_plugin_path() {
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: Vec::new(),
            entries: vec![entry("a", "a.json"), entry("b/c", "bc.json")],
        };
        let map = artifact_index_map(&index);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("a").unwrap().artifact_path, "a.json");
        assert_eq!(map.get("b/c").unwrap().artifact_path, "bc.json");
    }

    // --- resolve_artifact_path -----------------------------------------------

    #[test]
    fn resolve_artifact_path_relative_joins_index_parent() {
        let index_path = Path::new("/root/artifacts/index.json");
        let resolved = resolve_artifact_path(index_path, "sub/a.so");
        assert_eq!(resolved, PathBuf::from("/root/artifacts/sub/a.so"));
    }

    #[test]
    fn resolve_artifact_path_absolute_passthrough() {
        let index_path = Path::new("/root/artifacts/index.json");
        let resolved = resolve_artifact_path(index_path, "/abs/a.so");
        assert_eq!(resolved, PathBuf::from("/abs/a.so"));
    }

    // --- sha256_file ---------------------------------------------------------

    #[test]
    fn sha256_file_matches_known_digest() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("data.bin");
        fs::write(&path, b"abc").unwrap();
        // Known SHA-256 of "abc".
        assert_eq!(
            sha256_file(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sha256_file_empty_input() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.bin");
        fs::write(&path, b"").unwrap();
        assert_eq!(
            sha256_file(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn sha256_file_io_error() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope.bin");
        assert!(matches!(
            sha256_file(&missing),
            Err(RuntimeError::Io { .. })
        ));
    }

    #[test]
    fn sha256_file_read_error_on_directory() {
        // On Unix `File::open` succeeds for a directory but the subsequent
        // `read` fails (EISDIR), exercising the read-loop error branch rather
        // than the open error branch above.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("adir");
        fs::create_dir_all(&dir).unwrap();
        assert!(matches!(sha256_file(&dir), Err(RuntimeError::Io { .. })));
    }

    // --- load_plugin_artifact ------------------------------------------------

    fn write_json_artifact(dir: &Path, name: &str, art: &PluginArtifact) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, serde_json::to_string_pretty(art).unwrap()).unwrap();
        path
    }

    #[test]
    fn load_plugin_artifact_ok() {
        let tmp = TempDir::new().unwrap();
        let art = PluginArtifact {
            plugin_path: "a".to_string(),
            abi_fingerprint: abi(),
            docs: docs("a"),
            exports: Vec::new(),
            execution: None,
        };
        let path = write_json_artifact(tmp.path(), "a.json", &art);
        let loaded = load_plugin_artifact(&path).unwrap();
        assert_eq!(loaded.plugin_path, "a");
    }

    #[test]
    fn load_plugin_artifact_io_error() {
        let tmp = TempDir::new().unwrap();
        let err = load_plugin_artifact(&tmp.path().join("nope.json")).unwrap_err();
        assert!(matches!(err, RuntimeError::Io { .. }));
    }

    #[test]
    fn load_plugin_artifact_parse_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("bad.json");
        fs::write(&path, "{}").unwrap();
        let err = load_plugin_artifact(&path).unwrap_err();
        match err {
            RuntimeError::ArtifactIndexParse { message, .. } => {
                assert!(message.contains("artifact parse failed"), "{message}");
            }
            other => panic!("expected ArtifactIndexParse, got {other:?}"),
        }
    }

    // --- stage_artifact_bundle: JSON artifact, no execution ------------------

    #[test]
    fn stage_bundle_json_relative_reference() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        let art = PluginArtifact {
            plugin_path: "a".to_string(),
            abi_fingerprint: abi(),
            docs: docs("a"),
            exports: Vec::new(),
            execution: None,
        };
        let artifact_path = write_json_artifact(&src_dir, "a.json", &art);
        let staged_root = tmp.path().join("staged");
        let staged = stage_artifact_bundle("a", "a.json", &artifact_path, &staged_root).unwrap();
        assert_eq!(staged, staged_root.join("a.json"));
        assert!(staged.exists());
    }

    // --- stage_artifact_bundle: JSON artifact with process command -----------

    #[test]
    fn stage_bundle_json_with_relative_process_command() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("artifacts");
        fs::create_dir_all(src_dir.join("bin")).unwrap();
        // Create the process command binary next to the artifact.
        fs::write(src_dir.join("bin/run.sh"), b"#!/bin/sh\n").unwrap();
        let art = PluginArtifact {
            plugin_path: "a".to_string(),
            abi_fingerprint: abi(),
            docs: docs("a"),
            exports: Vec::new(),
            execution: Some(PluginExecution::Process {
                command: "bin/run.sh".to_string(),
                args: Vec::new(),
            }),
        };
        let artifact_path = write_json_artifact(&src_dir, "a.json", &art);
        let staged_root = tmp.path().join("staged");
        let staged = stage_artifact_bundle("a", "a.json", &artifact_path, &staged_root).unwrap();
        assert!(staged.exists());
        // The relative process command must be staged alongside the artifact.
        assert!(staged_root.join("bin/run.sh").exists());
    }

    #[test]
    fn stage_bundle_json_with_absolute_process_command_skips_copy() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        let art = PluginArtifact {
            plugin_path: "a".to_string(),
            abi_fingerprint: abi(),
            docs: docs("a"),
            exports: Vec::new(),
            execution: Some(PluginExecution::Process {
                command: "/usr/bin/true".to_string(),
                args: Vec::new(),
            }),
        };
        let artifact_path = write_json_artifact(&src_dir, "a.json", &art);
        let staged_root = tmp.path().join("staged");
        // Absolute command → stage_process_command returns Ok early, only the
        // artifact is staged.
        let staged = stage_artifact_bundle("a", "a.json", &artifact_path, &staged_root).unwrap();
        assert!(staged.exists());
    }

    // --- stage_artifact_bundle: dylib path (sidecar) -------------------------

    #[test]
    fn stage_bundle_dylib_with_sidecar() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        // A fake .so + its sidecar json (staging only copies bytes, no dlopen).
        let so_path = src_dir.join("a.so");
        fs::write(&so_path, b"\x7fELF-fake").unwrap();
        let sidecar = sidecar_json_path(&so_path);
        fs::write(&sidecar, b"{}").unwrap();
        let staged_root = tmp.path().join("staged");
        let staged = stage_artifact_bundle("a", "a.so", &so_path, &staged_root).unwrap();
        assert_eq!(staged, staged_root.join("a.so"));
        assert!(staged.exists());
        assert!(sidecar_json_path(&staged).exists());
    }

    #[test]
    fn stage_bundle_dylib_without_sidecar() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        let so_path = src_dir.join("b.so");
        fs::write(&so_path, b"\x7fELF-fake").unwrap();
        let staged_root = tmp.path().join("staged");
        let staged = stage_artifact_bundle("b", "b.so", &so_path, &staged_root).unwrap();
        assert!(staged.exists());
        assert!(!sidecar_json_path(&staged).exists());
    }

    // --- staged_artifact_path: absolute reference ----------------------------

    #[test]
    fn staged_artifact_path_absolute_reference_uses_plugin_path_dir() {
        let tmp = TempDir::new().unwrap();
        let artifact_path = tmp.path().join("build/libfoo.so");
        fs::create_dir_all(artifact_path.parent().unwrap()).unwrap();
        fs::write(&artifact_path, b"x").unwrap();
        let staged_root = tmp.path().join("staged");
        let abs_ref = artifact_path.to_string_lossy().to_string();
        // Absolute artifact_reference → relative path becomes
        // <plugin_path>/<file_name>.
        let staged =
            stage_artifact_bundle("foo/bar", &abs_ref, &artifact_path, &staged_root).unwrap();
        assert_eq!(staged, staged_root.join("foo/bar/libfoo.so"));
        assert!(staged.exists());
    }

    // --- stage_process_command escape guard ----------------------------------

    #[test]
    fn stage_process_command_escaping_root_is_rejected() {
        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&src_dir).unwrap();
        // Command escapes upward out of staged_root via `../`.
        let art = PluginArtifact {
            plugin_path: "a".to_string(),
            abi_fingerprint: abi(),
            docs: docs("a"),
            exports: Vec::new(),
            execution: Some(PluginExecution::Process {
                command: "../../escape/run.sh".to_string(),
                args: Vec::new(),
            }),
        };
        let artifact_path = write_json_artifact(&src_dir, "a.json", &art);
        // Use a nested staged root so `../..` escapes it.
        let staged_root = tmp.path().join("staged/deep");
        let err =
            stage_artifact_bundle("a", "nested/a.json", &artifact_path, &staged_root).unwrap_err();
        assert!(
            matches!(err, RuntimeError::Invariant { .. }),
            "expected escape rejection, got {err:?}"
        );
    }

    // --- stage_file: source == target early return ---------------------------

    #[test]
    fn stage_file_source_equals_target_is_noop() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("same.bin");
        fs::write(&path, b"hello").unwrap();
        // source == target → early Ok without touching the file.
        stage_file(&path, &path).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
    }
}
