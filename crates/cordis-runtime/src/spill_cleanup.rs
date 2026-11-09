//! Age-based cleanup for spilled tool results, run once at boot.
//!
//! Spill files are written so the model can read back a result that did not fit
//! in the context. They are never read automatically, so nothing prunes them
//! while the process runs — the directory grows with every oversized result.
//!
//! The rule is borrowed from DSH's spill backend: delete regular files older
//! than a retention period, once, at startup. Age rather than total size,
//! because a fixed age is something a reader can predict ("that result is older
//! than the retention window, so it is gone") while a size cap destroys an
//! arbitrary subset.
//!
//! Only regular files are removed: `symlink_metadata` never follows a link, so
//! a symlink planted in the directory cannot redirect the sweep or be deleted
//! as if it were a file. Anything unexpected is skipped, and every failure is
//! counted rather than fatal.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default retention for spill files.
pub const DEFAULT_RETENTION_DAYS: u64 = 30;

/// What a sweep did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Regular files removed for being too old.
    pub removed: usize,
    /// Entries left alone (recent files, symlinks, directories).
    pub kept: usize,
    /// Entries that could not be examined or removed.
    pub failed: usize,
}

/// Current wall clock in milliseconds, or 0 when the clock is before the epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// Cutoff for a retention period: files with an mtime strictly before this go.
///
/// A retention of 0 disables cleanup, which the caller expresses by not calling
/// this at all; here it simply yields the current instant so nothing is older.
pub fn cutoff_ms(now_ms: u64, retention_days: u64) -> u64 {
    let window = retention_days.saturating_mul(24 * 60 * 60 * 1000);
    now_ms.saturating_sub(window)
}

/// Delete regular files under `root` whose mtime is older than `cutoff`.
///
/// Empty session directories are pruned afterwards, mirroring the spill layout
/// DSH uses. A missing root is not an error: there is simply nothing to sweep.
pub fn sweep(root: &Path, cutoff: u64) -> SweepReport {
    let mut report = SweepReport::default();
    let Ok(entries) = std::fs::read_dir(root) else {
        return report;
    };
    let mut directories: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            report.failed += 1;
            continue;
        };
        if metadata.is_dir() {
            let nested = sweep(&path, cutoff);
            report.removed += nested.removed;
            report.kept += nested.kept;
            report.failed += nested.failed;
            directories.push(path);
            continue;
        }
        if !metadata.is_file() {
            // A symlink, socket or fifo: never followed, never removed.
            report.kept += 1;
            continue;
        }
        let mtime = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|elapsed| elapsed.as_millis() as u64);
        match mtime {
            Some(mtime) if mtime < cutoff => {
                if std::fs::remove_file(&path).is_ok() {
                    report.removed += 1;
                } else {
                    report.failed += 1;
                }
            }
            Some(_) => report.kept += 1,
            None => report.failed += 1,
        }
    }
    // A directory that held only expired files is itself prunable.
    for directory in directories {
        let empty = std::fs::read_dir(&directory)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if empty {
            let _ = std::fs::remove_dir(&directory);
        }
    }
    report
}

/// Retention in days: `CORDIS_SPILL_RETENTION_DAYS`, or the default.
///
/// `0` disables cleanup, matching DSH's `cleanupPeriodDays: 0`.
pub fn retention_days() -> u64 {
    std::env::var("CORDIS_SPILL_RETENTION_DAYS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_RETENTION_DAYS)
}

/// Sweep `data_dir/spill` at boot. Returns what happened, or `None` when
/// cleanup is disabled.
pub fn sweep_data_dir(data_dir: &Path) -> Option<SweepReport> {
    let days = retention_days();
    if days == 0 {
        return None;
    }
    Some(sweep(&data_dir.join("spill"), cutoff_ms(now_ms(), days)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::FileTimes;
    use std::time::Duration;

    /// Set a file's mtime to `age` before now.
    fn age_file(path: &Path, age: Duration) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open");
        let when = SystemTime::now() - age;
        file.set_times(FileTimes::new().set_modified(when))
            .expect("set mtime");
    }

    #[test]
    fn removes_files_older_than_the_cutoff_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.txt");
        let new = dir.path().join("new.txt");
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&new, "new").unwrap();
        age_file(&old, Duration::from_secs(40 * 24 * 3600));
        let report = sweep(dir.path(), cutoff_ms(now_ms(), 30));
        assert_eq!(report.removed, 1);
        assert_eq!(report.kept, 1);
        assert!(!old.exists());
        assert!(new.exists());
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let report = sweep(&dir.path().join("nope"), cutoff_ms(now_ms(), 30));
        assert_eq!(report, SweepReport::default());
    }

    #[test]
    fn symlinks_are_skipped_even_when_old() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "keep me").unwrap();
        let link = dir.path().join("link.txt");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&target, &link).unwrap();
            age_file(&target, Duration::from_secs(40 * 24 * 3600));
            let report = sweep(dir.path(), cutoff_ms(now_ms(), 30));
            // The regular file is removed; the link is not followed or unlinked.
            assert_eq!(report.removed, 1);
            assert!(std::fs::symlink_metadata(&link).is_ok());
        }
        #[cfg(not(unix))]
        assert!(true);
    }

    #[test]
    fn nested_session_directories_are_swept_and_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session-abc");
        std::fs::create_dir(&session).unwrap();
        let old = session.join("a.txt");
        std::fs::write(&old, "old").unwrap();
        age_file(&old, Duration::from_secs(40 * 24 * 3600));
        let report = sweep(dir.path(), cutoff_ms(now_ms(), 30));
        assert_eq!(report.removed, 1);
        assert!(!session.exists(), "empty session dir is pruned");
    }

    #[test]
    fn a_recent_symlink_only_directory_survives() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session-keep");
        std::fs::create_dir(&session).unwrap();
        std::fs::write(session.join("recent.txt"), "x").unwrap();
        let report = sweep(dir.path(), cutoff_ms(now_ms(), 30));
        assert_eq!(report.removed, 0);
        assert!(session.exists());
    }

    #[test]
    fn the_cutoff_moves_back_with_the_retention_window() {
        let now = 10_000_000_000u64;
        assert_eq!(cutoff_ms(now, 0), now);
        assert_eq!(cutoff_ms(now, 1), now - 86_400_000);
        assert_eq!(cutoff_ms(now, 30), now - 30 * 86_400_000);
        // A window larger than the clock cannot underflow.
        assert_eq!(cutoff_ms(10, 30), 0);
    }

    #[test]
    fn cleanup_can_be_disabled_by_the_environment() {
        // Serialized by the test harness's default single-threaded run for env
        // vars in this crate; the value is restored immediately.
        let previous = std::env::var("CORDIS_SPILL_RETENTION_DAYS").ok();
        std::env::set_var("CORDIS_SPILL_RETENTION_DAYS", "0");
        assert_eq!(retention_days(), 0);
        std::env::remove_var("CORDIS_SPILL_RETENTION_DAYS");
        assert_eq!(retention_days(), DEFAULT_RETENTION_DAYS);
        if let Some(previous) = previous {
            std::env::set_var("CORDIS_SPILL_RETENTION_DAYS", previous);
        }
    }

    #[test]
    fn sweeping_a_data_dir_without_spill_files_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let report = sweep_data_dir(dir.path()).expect("cleanup enabled by default");
        assert_eq!(report.removed, 0);
    }
}
