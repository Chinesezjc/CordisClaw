use super::{
    apply_plugin_iteration_journal, plugin_iteration_applied_marker_path,
    plugin_iteration_journal_path, scaffolded_child_order, validated_verification_command,
    with_partial_batch_rollback_failure, workspace_manifest_lock, ScaffoldedChildRegistration,
};
use crate::core::error::RuntimeError;
use crate::kernel::plugin_iteration::PluginEditRollback;
use std::cmp::Ordering;
use std::fs;
use tempfile::TempDir;

fn invalid(message: &str) -> RuntimeError {
    RuntimeError::InvalidArgument {
        message: message.to_string(),
    }
}

#[track_caller]
fn invariant_message(err: &RuntimeError) -> String {
    match err {
        RuntimeError::Invariant { message } => message.clone(),
        other => format!("not Invariant: {other}"),
    }
}

#[test]
fn invariant_message_labels_other_variants() {
    let other = RuntimeError::InvalidArgument {
        message: "nope".to_string(),
    };
    assert_eq!(invariant_message(&other), format!("not Invariant: {other}"));
}

fn registration(child_root: &str, parent_manifest: &str) -> ScaffoldedChildRegistration {
    ScaffoldedChildRegistration {
        parent_manifest_path: parent_manifest.to_string(),
        child_root_path: child_root.to_string(),
    }
}

// ── apply_operations double-fault arm ────────────────────────────────
//
// `with_partial_batch_rollback_failure` is the wrapper `apply_operations`
// applies when a batch operation failed AND unwinding the already-applied
// prefix of the same batch also failed. Reaching that state in situ needs
// the rollback's own `fs::write` / `remove_file` to fail on a path the
// executor just successfully wrote inside the same call, which cannot be
// arranged without interleaving the loop, so the wrapper is asserted
// directly — including the byte-exact message.

#[test]
fn partial_batch_rollback_failure_wraps_both_messages_byte_exactly() {
    let edit_err = RuntimeError::LlmResponseInvalid {
        message: "auto update patch pattern not found in /w/a.rs: OLD".to_string(),
    };
    let rollback_err = RuntimeError::Io {
        path: std::path::PathBuf::from("/w/b.rs"),
        message: "Is a directory (os error 21)".to_string(),
    };
    // Bind the Display forms first so the expectation is built from the
    // same `Display` impls the production `format!` uses.
    let edit_text = edit_err.to_string();
    let rollback_text = rollback_err.to_string();
    let out = with_partial_batch_rollback_failure(edit_err, Some(rollback_err));
    assert_eq!(
        invariant_message(&out),
        format!("{edit_text}; additionally, partial-batch rollback failed: {rollback_text}")
    );
}

#[test]
fn partial_batch_rollback_failure_passes_error_through_when_rollback_succeeded() {
    // The common case: the batch operation failed but unwinding worked, so
    // the caller sees the enriched edit error unchanged (same variant, same
    // message) rather than an `Invariant` wrapper.
    let out = with_partial_batch_rollback_failure(invalid("stale snippet"), None);
    assert!(
        matches!(&out, RuntimeError::InvalidArgument { message } if message == "stale snippet"),
        "expected the edit error verbatim, got {out:?}"
    );
}

#[test]
fn partial_batch_rollback_failure_nests_an_already_invariant_edit_error() {
    // The edit error can itself be an `Invariant` (e.g. a rollback-workspace
    // mismatch surfaced by `absorb`); the wrapper still produces a single
    // flat `Invariant` whose message embeds both `Display` forms.
    let edit_err = RuntimeError::Invariant {
        message: "plugin edit rollback workspace mismatch: /a vs /b".to_string(),
    };
    let edit_text = edit_err.to_string();
    let out = with_partial_batch_rollback_failure(edit_err, Some(invalid("rollback kaput")));
    assert_eq!(
            invariant_message(&out),
            format!(
                "{edit_text}; additionally, partial-batch rollback failed: invalid argument: rollback kaput"
            )
        );
}

// ── scaffold_child_plugin: scaffolded-children ordering ──────────────

#[test]
fn scaffolded_child_order_sorts_by_child_root_then_parent_manifest() {
    // Distinct child roots: the child root decides, the parent manifest is
    // never consulted.
    assert_eq!(
        scaffolded_child_order(
            &registration("plugins/expr/evaluator/abs", "plugins/expr/zzz/Cargo.toml"),
            &registration("plugins/expr/evaluator/dist", "plugins/expr/aaa/Cargo.toml"),
        ),
        Ordering::Less
    );
    // Equal child roots: the parent manifest breaks the tie.
    assert_eq!(
        scaffolded_child_order(
            &registration("plugins/expr/evaluator/dist", "plugins/expr/b/Cargo.toml"),
            &registration("plugins/expr/evaluator/dist", "plugins/expr/a/Cargo.toml"),
        ),
        Ordering::Greater
    );
    // Fully equal registrations compare Equal, which is what makes the
    // following `dedup()` in `scaffold_child_plugin` collapse repeats.
    assert_eq!(
        scaffolded_child_order(
            &registration("plugins/expr/evaluator/dist", "plugins/expr/a/Cargo.toml"),
            &registration("plugins/expr/evaluator/dist", "plugins/expr/a/Cargo.toml"),
        ),
        Ordering::Equal
    );
}

#[test]
fn scaffolded_child_order_drives_sort_and_dedup_like_scaffold_child_plugin() {
    // Two scaffolds in one iteration plus a duplicate: the exact shape
    // `scaffold_child_plugin` feeds into `sort_by` + `dedup`.
    let mut children = vec![
        registration(
            "plugins/expr/evaluator/dist",
            "plugins/expr/evaluator/Cargo.toml",
        ),
        registration(
            "plugins/expr/evaluator/abs",
            "plugins/expr/evaluator/Cargo.toml",
        ),
        registration(
            "plugins/expr/evaluator/dist",
            "plugins/expr/evaluator/Cargo.toml",
        ),
    ];
    children.sort_by(scaffolded_child_order);
    children.dedup();
    let roots = children
        .iter()
        .map(|entry| entry.child_root_path.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        roots,
        vec!["plugins/expr/evaluator/abs", "plugins/expr/evaluator/dist"]
    );
}

// ── validated_verification_command: bare alias without a fallback ────

#[test]
fn validated_verification_command_bare_alias_without_fallback_is_rejected() {
    // `check` is the bare alias, but no default command is available to
    // substitute → the alias falls through to the prefix guard and is
    // rejected with the byte-exact prefix message.
    let out = validated_verification_command(Some("check".to_string()), None, "cargo check");
    assert!(
        matches!(&out, Err(RuntimeError::InvalidArgument { message })
                if *message == "verification tool only allows commands starting with `cargo check`, got `check`"),
        "expected the prefix-guard rejection, got {out:?}"
    );
}

#[test]
fn validated_verification_command_alias_of_the_other_tool_is_not_substituted() {
    // `test` is only an alias for the `cargo test` tool; supplied to the
    // `cargo check` tool it is an ordinary (invalid) command even though a
    // fallback exists.
    let out = validated_verification_command(
        Some("test".to_string()),
        Some("cargo check --quiet".to_string()),
        "cargo check",
    );
    assert!(
        matches!(&out, Err(RuntimeError::InvalidArgument { message })
                if *message == "verification tool only allows commands starting with `cargo check`, got `test`"),
        "expected cross-tool alias to be rejected, got {out:?}"
    );
}

// ── workspace_manifest_lock::acquire: flock failure branch ───────────

#[cfg(unix)]
#[test]
fn workspace_manifest_lock_acquire_survives_flock_failure_on_a_fifo() {
    // `flock(2)` reports ENOTSUP for a FIFO on macOS/BSD, while opening one
    // O_RDWR succeeds and does not block. That is the one portable way to
    // reach the `rc != 0` branch, which only logs and still hands back a
    // guard holding the open file.
    let temp = TempDir::new().expect("tempdir");
    let fifo = temp.path().join("nested/manifest.lock");
    fs::create_dir_all(fifo.parent().expect("fifo parent")).expect("mkdir");
    let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes())
        .expect("lock path has no interior NUL");
    // SAFETY: `c_path` is a NUL-terminated path owned for the call's whole
    // duration; `mkfifo` only reads it.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o666) };
    assert_eq!(rc, 0, "mkfifo must succeed inside a fresh tempdir");

    let guard = workspace_manifest_lock::acquire(&fifo);
    // The guard still owns an open descriptor even though locking failed;
    // dropping it exercises the unlock path on that same descriptor.
    drop(guard);
    assert!(fifo.exists(), "acquire must not remove the lock path");
}

// ── apply_plugin_iteration_journal: replay and skip gates ────────────

fn seed_journal(
    workspace: &std::path::Path,
    snapshot_root: &std::path::Path,
    rel: &str,
    pre_edit: &[u8],
) {
    let target = workspace.join(rel);
    fs::create_dir_all(target.parent().expect("target parent")).expect("mkdir target parent");
    let rollback = PluginEditRollback::single_backup(workspace, rel, Some(pre_edit.to_vec()));
    rollback
        .persist_journal(&plugin_iteration_journal_path(snapshot_root), "iter-ops")
        .expect("persist journal");
}

#[test]
fn apply_plugin_iteration_journal_replays_and_cleans_up_marker_and_journal() {
    let temp = TempDir::new().expect("tempdir");
    let workspace = temp.path().join("workspace");
    let snapshot_root = temp.path().join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
    let rel = "plugins/mini/src/lib.rs";
    seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT");
    // Simulate the post-edit workspace state the journal must undo.
    fs::write(workspace.join(rel), b"POST-EDIT").expect("write post-edit body");

    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
        .expect("journal replay succeeds");
    assert!(restored, "a journal on disk means a restore happened");
    assert_eq!(
        fs::read(workspace.join(rel)).expect("read restored"),
        b"PRE-EDIT"
    );
    // Both the journal and the applied marker are cleaned up on success.
    assert!(!plugin_iteration_journal_path(&snapshot_root).exists());
    assert!(!plugin_iteration_applied_marker_path(&snapshot_root).exists());
}

#[test]
fn apply_plugin_iteration_journal_skips_when_marker_matches_journal_generation() {
    let temp = TempDir::new().expect("tempdir");
    let workspace = temp.path().join("workspace");
    let snapshot_root = temp.path().join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
    let rel = "plugins/mini/src/lib.rs";
    seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT");
    let journal_path = plugin_iteration_journal_path(&snapshot_root);
    let generation_id = PluginEditRollback::journal_generation_id(&journal_path)
        .expect("read generation id")
        .expect("a freshly persisted journal carries a generation id");
    // A marker recording the SAME generation means the replay already ran.
    let marker = plugin_iteration_applied_marker_path(&snapshot_root);
    fs::write(&marker, generation_id.as_bytes()).expect("write applied marker");
    fs::write(workspace.join(rel), b"LEGITIMATE-LATER-EDIT").expect("write later body");

    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
        .expect("already-applied journal short-circuits");
    assert!(!restored, "an already-applied journal reports no restore");
    // The later, legitimate edit must survive untouched.
    assert_eq!(
        fs::read(workspace.join(rel)).expect("read after skip"),
        b"LEGITIMATE-LATER-EDIT"
    );
    assert!(!journal_path.exists(), "journal cleared on the skip path");
    assert!(!marker.exists(), "marker cleared on the skip path");
}

#[test]
fn apply_plugin_iteration_journal_replays_when_marker_generation_differs() {
    let temp = TempDir::new().expect("tempdir");
    let workspace = temp.path().join("workspace");
    let snapshot_root = temp.path().join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
    let rel = "plugins/mini/src/lib.rs";
    seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT");
    // A stale marker from an EARLIER journal must not suppress this replay.
    fs::write(
        plugin_iteration_applied_marker_path(&snapshot_root),
        b"some-older-generation",
    )
    .expect("write stale marker");
    fs::write(workspace.join(rel), b"POST-EDIT").expect("write post-edit body");

    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
        .expect("mismatched marker still replays");
    assert!(restored, "a different generation id means replay proceeds");
    assert_eq!(
        fs::read(workspace.join(rel)).expect("read restored"),
        b"PRE-EDIT"
    );
}

#[test]
fn apply_plugin_iteration_journal_falls_back_to_the_in_memory_rollback() {
    let temp = TempDir::new().expect("tempdir");
    let workspace = temp.path().join("workspace");
    let snapshot_root = temp.path().join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
    let rel = "plugins/mini/src/lib.rs";
    let target = workspace.join(rel);
    fs::create_dir_all(target.parent().expect("target parent")).expect("mkdir");
    fs::write(&target, b"POST-EDIT").expect("write post-edit body");
    // No journal on disk → the in-memory rollback is used instead.
    let rollback = PluginEditRollback::single_backup(&workspace, rel, Some(b"PRE-EDIT".to_vec()));

    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, Some(&rollback))
        .expect("in-memory rollback replays");
    assert!(restored);
    assert_eq!(fs::read(&target).expect("read restored"), b"PRE-EDIT");
}

#[test]
fn apply_plugin_iteration_journal_reports_nothing_to_restore_when_both_are_absent() {
    let temp = TempDir::new().expect("tempdir");
    let snapshot_root = temp.path().join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
    let restored = apply_plugin_iteration_journal(temp.path(), &snapshot_root, None)
        .expect("no journal and no rollback is not an error");
    assert!(!restored);
}

#[test]
fn apply_plugin_iteration_journal_surfaces_a_corrupt_journal_as_invariant() {
    // `load_journal`'s parse error propagates through the `?` on the
    // `load_journal` call, which is the arm the replay path guards.
    let temp = TempDir::new().expect("tempdir");
    let snapshot_root = temp.path().join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
    fs::write(
        plugin_iteration_journal_path(&snapshot_root),
        b"{ not valid json",
    )
    .expect("write corrupt journal");
    let err = apply_plugin_iteration_journal(temp.path(), &snapshot_root, None)
        .expect_err("a corrupt journal must not be silently ignored");
    assert!(
        invariant_message(&err).starts_with("plugin edit rollback journal parse failed: "),
        "got {err:?}"
    );
}

#[test]
fn apply_plugin_iteration_journal_propagates_a_failing_rollback_write() {
    // The `rollback.rollback()?` arm: the journal wants to restore bytes to
    // a path that is now a non-empty directory, so `fs::write` fails.
    let temp = TempDir::new().expect("tempdir");
    let workspace = temp.path().join("workspace");
    let snapshot_root = temp.path().join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
    let rel = "plugins/mini/src/lib.rs";
    seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT");
    // Replace the target file with a non-empty directory.
    let target = workspace.join(rel);
    fs::create_dir_all(target.join("occupied")).expect("mkdir target-as-dir");

    let err = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
        .expect_err("restoring onto a directory must fail");
    assert!(
        matches!(&err, RuntimeError::Io { path, .. } if path == &target),
        "expected an Io error naming the restore target, got {err:?}"
    );
}
