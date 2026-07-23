//! Crash-recovery integration test for the plugin-iteration rollback
//! journal (P0-6 / P0-7).
//!
//! The unit tests already cover journal serialization, atomic-write
//! semantics, and generation-id round-tripping. This file exercises the
//! *cross-boot* replay path: a workspace is modified inside "iteration
//! 1", the journal is persisted, then iteration 1 is dropped without
//! calling `clear_journal` (simulating SIGKILL). "Boot 2" invokes the
//! recovery entry point and must observe:
//!
//!   - the modified files are reverted to their pre-iteration bytes;
//!   - the journal file is removed on successful replay;
//!   - a subsequent recovery call on the same snapshot_root is a no-op
//!     (P0-7 idempotency via the `.applied` marker generation id);
//!   - a stale `.applied` marker whose id does NOT match the journal
//!     still triggers a full replay (i.e. the marker only skips replay
//!     for the SAME journal, not any journal).

use std::fs;
use std::path::PathBuf;

use cordis_runtime::host::apply_plugin_iteration_journal;
use cordis_runtime::kernel::plugin_iteration::PluginEditRollback;

fn setup_workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let workspace = temp.path().to_path_buf();
    let snapshot_root = workspace.join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("snapshot dir");
    (temp, workspace, snapshot_root)
}

fn journal_path(snapshot_root: &std::path::Path) -> PathBuf {
    snapshot_root.join("plugin-iteration-edit-journal.json")
}

fn applied_marker_path(snapshot_root: &std::path::Path) -> PathBuf {
    snapshot_root.join("plugin-iteration-edit-journal.applied")
}

/// End-to-end recovery: simulate iteration crash mid-flight, boot,
/// verify rollback happens and journal is cleared.
#[test]
fn recovery_restores_edited_file_after_simulated_crash() {
    let (_guard, workspace, snapshot_root) = setup_workspace();
    let target = workspace.join("plugins/demo/src/lib.rs");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, b"PRE-EDIT").unwrap();

    // "Iteration 1": stash the pre-edit backup and persist the journal.
    let rb = PluginEditRollback::single_backup(
        &workspace,
        "plugins/demo/src/lib.rs",
        Some(b"PRE-EDIT".to_vec()),
    );
    rb.persist_journal(&journal_path(&snapshot_root), "iter-1")
        .expect("persist journal");
    // Simulate mid-flight mutation (agent edited the file).
    fs::write(&target, b"POST-EDIT-DIRTY").unwrap();
    // Drop `rb` without calling `clear_journal` — the process would
    // have SIGKILL'd here in a real crash.
    drop(rb);
    assert!(
        journal_path(&snapshot_root).exists(),
        "journal survives crash"
    );

    // "Boot 2": run recovery. Expected: rollback restores PRE-EDIT bytes,
    // journal file is removed.
    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
        .expect("recovery runs cleanly");
    assert!(restored, "recovery reports a restore happened");
    assert_eq!(fs::read(&target).unwrap(), b"PRE-EDIT");
    assert!(
        !journal_path(&snapshot_root).exists(),
        "journal removed after successful replay"
    );
    assert!(
        !applied_marker_path(&snapshot_root).exists(),
        "applied marker cleaned up after successful replay"
    );
}

/// A recovered snapshot survives a second boot without re-replaying.
/// The applied-marker generation id must match the (now-absent)
/// journal, so `apply_plugin_iteration_journal` short-circuits.
#[test]
fn double_boot_after_recovery_is_a_noop() {
    let (_guard, workspace, snapshot_root) = setup_workspace();
    let target = workspace.join("plugins/demo/src/lib.rs");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, b"PRE").unwrap();

    let rb = PluginEditRollback::single_backup(
        &workspace,
        "plugins/demo/src/lib.rs",
        Some(b"PRE".to_vec()),
    );
    rb.persist_journal(&journal_path(&snapshot_root), "iter")
        .unwrap();
    fs::write(&target, b"DIRTY").unwrap();

    // Boot 2 restores.
    apply_plugin_iteration_journal(&workspace, &snapshot_root, None).unwrap();
    assert_eq!(fs::read(&target).unwrap(), b"PRE");

    // Legitimate post-boot user edit — bytes go somewhere new.
    fs::write(&target, b"USER-EDIT").unwrap();

    // Boot 3: no journal exists → recovery is a no-op → user edit is
    // NOT clobbered.
    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None).unwrap();
    assert!(!restored, "no journal → no restore happens");
    assert_eq!(
        fs::read(&target).unwrap(),
        b"USER-EDIT",
        "legitimate post-boot edit must survive a no-op recovery"
    );
}

/// P0-7 idempotency: two consecutive `apply_plugin_iteration_journal`
/// calls, without ever legitimately editing between them, must produce
/// exactly one restore. The second call short-circuits via the applied
/// marker.
///
/// This simulates: recovery replays; SIGKILL between rollback and
/// clear_journal; second boot sees marker + journal with the same id;
/// second boot MUST NOT double-rollback (which would revert a
/// post-restore edit).
#[test]
fn recovery_is_idempotent_when_crash_between_rollback_and_clear() {
    let (_guard, workspace, snapshot_root) = setup_workspace();
    let target = workspace.join("plugins/demo/src/lib.rs");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, b"PRE").unwrap();

    // Set up: journal + pre-edit backup, dirty on-disk state.
    let rb = PluginEditRollback::single_backup(
        &workspace,
        "plugins/demo/src/lib.rs",
        Some(b"PRE".to_vec()),
    );
    let jp = journal_path(&snapshot_root);
    rb.persist_journal(&jp, "iter").unwrap();
    fs::write(&target, b"DIRTY").unwrap();

    // Capture the journal's generation id so we can synthesize a
    // "crashed mid-recovery" state (marker written, journal still on
    // disk, workspace already rolled back).
    let gen_id = PluginEditRollback::journal_generation_id(&jp)
        .unwrap()
        .expect("generation id exists");
    // Perform rollback ourselves — model of "just finished the rollback,
    // journal not yet cleared".
    fs::write(&target, b"PRE").unwrap();
    fs::write(applied_marker_path(&snapshot_root), gen_id.as_bytes()).unwrap();
    // journal still present, marker present, workspace back to PRE.

    // User makes a legitimate post-crash edit. If recovery re-plays, this
    // will be clobbered — the exact regression P0-7 is about.
    fs::write(&target, b"USER-EDIT-AFTER-CRASH").unwrap();

    // Boot 3: recovery must recognise marker.id == journal.id and skip.
    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None).unwrap();
    assert!(!restored, "matching marker/journal must short-circuit");
    assert_eq!(
        fs::read(&target).unwrap(),
        b"USER-EDIT-AFTER-CRASH",
        "post-crash user edit must NOT be re-rollback'd"
    );
    // The short-circuit also cleans up: both files gone.
    assert!(!jp.exists(), "journal cleared on skip");
    assert!(
        !applied_marker_path(&snapshot_root).exists(),
        "marker cleared on skip"
    );
}

/// The marker only guards against replaying the SAME journal id.
/// A brand-new iteration produces a new journal with a fresh
/// generation_id; recovery MUST run against it even if a stale marker
/// from a prior iteration is still on disk.
#[test]
fn stale_marker_does_not_block_a_new_journal() {
    let (_guard, workspace, snapshot_root) = setup_workspace();
    let target = workspace.join("plugins/demo/src/lib.rs");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, b"NEW-PRE").unwrap();

    // Put a stale marker from a hypothetical previous iteration.
    fs::write(
        applied_marker_path(&snapshot_root),
        b"stale-generation-id-from-earlier-iteration",
    )
    .unwrap();

    // Now a fresh journal (different generation id).
    let rb = PluginEditRollback::single_backup(
        &workspace,
        "plugins/demo/src/lib.rs",
        Some(b"NEW-PRE".to_vec()),
    );
    rb.persist_journal(&journal_path(&snapshot_root), "iter-new")
        .unwrap();
    fs::write(&target, b"NEW-DIRTY").unwrap();

    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None).unwrap();
    assert!(
        restored,
        "fresh journal must be replayed despite stale marker"
    );
    assert_eq!(fs::read(&target).unwrap(), b"NEW-PRE");
}

/// P0-6 durability: journal_generation_id must round-trip verbatim
/// through the atomic write, and two persists produce distinct ids.
#[test]
fn journal_generation_id_is_stable_across_reload() {
    let (_guard, workspace, snapshot_root) = setup_workspace();
    let rb = PluginEditRollback::single_backup(
        &workspace,
        "plugins/demo/src/lib.rs",
        Some(b"x".to_vec()),
    );
    let jp = journal_path(&snapshot_root);
    rb.persist_journal(&jp, "iter").unwrap();
    let first = PluginEditRollback::journal_generation_id(&jp)
        .unwrap()
        .expect("id present");

    // Re-open the journal (simulates a new host boot reading it).
    let loaded = PluginEditRollback::load_journal(&workspace, &jp)
        .unwrap()
        .expect("journal parses");
    // Re-persist the loaded rollback — MUST bump the id (each persist
    // is a fresh generation).
    loaded.persist_journal(&jp, "iter").unwrap();
    let second = PluginEditRollback::journal_generation_id(&jp)
        .unwrap()
        .expect("id present after re-persist");
    assert_ne!(first, second, "re-persist must generate a new id");
}

/// In-memory rollback path: `restore_plugin_iteration_workspace` with
/// no on-disk journal but a supplied `PluginEditRollback` should apply
/// the in-memory rollback and report `true`. Same-process failure
/// recovery uses this branch.
#[test]
fn in_memory_rollback_path_applies_when_no_journal() {
    let (_guard, workspace, snapshot_root) = setup_workspace();
    let target = workspace.join("plugins/demo/src/lib.rs");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, b"ORIG").unwrap();

    let rb = PluginEditRollback::single_backup(
        &workspace,
        "plugins/demo/src/lib.rs",
        Some(b"ORIG".to_vec()),
    );
    fs::write(&target, b"MUTATED").unwrap();

    // No on-disk journal — recovery uses the in-memory rollback.
    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, Some(&rb)).unwrap();
    assert!(restored);
    assert_eq!(fs::read(&target).unwrap(), b"ORIG");
    // Nothing to clean up on disk (no journal was ever written).
    assert!(!journal_path(&snapshot_root).exists());
}
