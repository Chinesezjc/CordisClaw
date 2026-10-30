use super::{
    cleanup_orphaned_snapshot_roots, cleanup_stale_snapshot_dirs, collect_plugin_context_paths,
    dir_is_older_than, dir_size_bytes, ensure_scaffold_integration_edits, extract_warning_blocks,
    render_child_plugin_core, render_child_plugin_test, sanitize_child_plugin_segment,
    snapshot_staging_lock, sort_plugin_context_paths, staged_artifact_root_is_removable,
    warning_diagnostics_for_changed_paths, AgentBackend, AgentSessionKind, AgentStartOptions,
    ContextFilesScope, PluginIterationAgentBackend, PluginIterationAgentState, RuntimeHost,
    ScaffoldedChildRegistration, SnapshotGcReport, PLUGIN_AGENT_TOOL_CREATE_FILE,
    PLUGIN_AGENT_TOOL_DELETE_FILE, PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG,
    PLUGIN_AGENT_TOOL_JSON_SET, PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES,
    PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES, PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT,
    PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT, PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN,
    PLUGIN_AGENT_TOOL_TOML_SET,
};
use crate::core::error::RuntimeError;
use crate::core::models::NodeOutcome;
use crate::kernel::plugin_iteration::{
    KernelPluginIterationRequest, PluginEditOpKind, PluginEditOperation,
};
use serial_test::serial;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

fn repo_fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .canonicalize()
        .expect("fixtures root")
}

#[test]
fn stale_snapshot_cleanup_keeps_live_pid_dirs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();

    // 本进程的 in-flight snapshot：必须保留。
    let live = root.join(format!("snapshot-{}-1", std::process::id()));
    // 已死进程的残留：起一个立即退出的子进程并 wait 掉，用它的 pid。
    // （wait 之后 pid 已回收；紧接着复用到新进程的概率可忽略。）
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn short-lived child");
    let dead_pid = child.id();
    child.wait().expect("reap child");
    let dead = root.join(format!("snapshot-{dead_pid}-1"));
    // 历史 `snapshot-{nanos}` 旧格式：首段纳秒时间戳 parse u32 失败，
    // 按 stale 清理。
    let legacy = root.join("snapshot-1784782891597667568");
    // 前缀不匹配的目录与普通文件：不受清理影响。
    let unrelated = root.join("candidate-1");
    let file = root.join("snapshot-not-a-dir");
    for dir in [&live, &dead, &legacy, &unrelated] {
        fs::create_dir(dir).expect("create test dir");
    }
    fs::write(&file, b"x").expect("write file");

    cleanup_stale_snapshot_dirs(root);

    assert!(live.exists(), "live-pid snapshot must survive cleanup");
    assert!(!dead.exists(), "dead-pid snapshot must be removed");
    assert!(!legacy.exists(), "legacy-format snapshot must be removed");
    assert!(unrelated.exists(), "non-snapshot dirs are untouched");
    assert!(file.exists(), "plain files are untouched");
}

/// `Some(())` 当且仅当当前 euid 不是 root。
///
/// root 持 `CAP_DAC_OVERRIDE`，内核不对它强制 mode 位，`chmod 000` 之后仍
/// 能 read_dir。返回 `Option` 让调用方用 `for`-over-`Option` 门控——不同于
/// `if` 或提前 `return`，它没有"未走到的臂"会留下永久无覆盖的行。
#[cfg(unix)]
fn probe_not_root() -> Option<()> {
    // SAFETY: `geteuid` 无参数、不写调用方内存、不设 errno。
    (unsafe { libc::geteuid() } != 0).then_some(())
}

/// 起一个立即退出的子进程并 wait 掉，拿一个确定已死的 pid。
fn reaped_dead_pid() -> u32 {
    let mut child = std::process::Command::new("true")
        .spawn()
        .expect("spawn short-lived child");
    let pid = child.id();
    child.wait().expect("reap child");
    pid
}

/// 把目录 mtime 往前拨 `secs` 秒，模拟"老目录"。
fn backdate_dir(path: &Path, secs: u64) {
    let past = SystemTime::now() - Duration::from_secs(secs);
    let times = fs::FileTimes::new().set_modified(past);
    let dir = fs::File::open(path).expect("open dir for utimes");
    dir.set_times(times).expect("backdate dir mtime");
}

/// 造一个 hash 目录：内含一个属于 `pid` 的 snapshot 子目录 + 一个占位文件。
fn make_hash_dir(root: &Path, name: &str, pid: u32) -> PathBuf {
    let hash = root.join(name);
    let snapshot = hash.join(format!("snapshot-{pid}-1"));
    fs::create_dir_all(&snapshot).expect("create hash/snapshot dir");
    fs::write(snapshot.join("plugin.so"), b"0123456789").expect("write artifact");
    hash
}

#[test]
fn orphaned_snapshot_root_gc_removes_dead_pid_dirs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let dead_pid = reaped_dead_pid();

    // 死 pid + 老 mtime → 删。
    let orphan = make_hash_dir(root, "aaa", dead_pid);
    backdate_dir(&orphan, 48 * 3600);
    // 活 pid（本进程）→ 留，无论多老。
    let live = make_hash_dir(root, "bbb", std::process::id());
    backdate_dir(&live, 48 * 3600);
    // 死 pid 但 mtime 新 → 留（未到保留期）。
    let recent = make_hash_dir(root, "ccc", dead_pid);

    let report = cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);

    assert!(!orphan.exists(), "dead-pid aged hash dir must be removed");
    assert!(live.exists(), "live-pid hash dir must survive");
    assert!(recent.exists(), "hash dir within retention must survive");
    assert_eq!(report.removed, 1);
    assert_eq!(report.skipped_live, 1);
    assert_eq!(report.skipped_recent, 1);
    assert_eq!(report.scanned, 3);
    // 回收字节数覆盖到 snapshot 子目录里的文件。
    assert_eq!(report.bytes_reclaimed, 10);
}

#[test]
fn orphaned_snapshot_root_gc_preserves_journal_dirs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let dead_pid = reaped_dead_pid();

    // 死 pid + 老 mtime，但含未重放的 rollback journal → 必须保留。
    // journal 是崩溃恢复状态，sha256 单向无法反推 fixtures root 还在不在，
    // 盲删会摧毁回滚记录。
    let with_journal = make_hash_dir(root, "aaa", dead_pid);
    fs::write(
        with_journal.join("plugin-iteration-edit-journal.json"),
        b"{}",
    )
    .expect("write journal");
    backdate_dir(&with_journal, 48 * 3600);

    let report = cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);

    assert!(
        with_journal.exists(),
        "hash dir holding an unreplayed journal must never be auto-removed"
    );
    assert_eq!(report.skipped_journal, 1);
    assert_eq!(report.removed, 0);
}

/// boot GC 的日志行：有回收才输出。coverage 跑在全新临时目录里，boot GC
/// 永远没有可回收的孤儿，`removed > 0` 那支在原地不可达，故直接测纯函数。
#[test]
fn snapshot_gc_summary_line_only_when_something_was_reclaimed() {
    let mut report = SnapshotGcReport::default();
    assert!(super::snapshot_gc_summary_line(&report).is_none());

    report.removed = 3;
    report.bytes_reclaimed = 2 * 1024 * 1024;
    let line = super::snapshot_gc_summary_line(&report).expect("a reclaim must be logged");
    assert!(line.contains("回收 3 个"), "got: {line}");
    assert!(line.contains("2.0 MB"), "got: {line}");
}

/// `flock` 返回码判定的两个方向。原地只会看到 rc==0（新建锁文件上的
/// `flock` 不会失败），失败支只能这样驱动。
#[cfg(unix)]
#[test]
fn flock_succeeded_maps_zero_to_true_and_nonzero_to_false() {
    assert!(snapshot_staging_lock::flock_succeeded(0));
    assert!(!snapshot_staging_lock::flock_succeeded(-1));
}

/// 同一进程内重复 `hold` 同一目录：flock 是**按 fd** 而非按进程的，第二次
/// 用新 fd 试 `LOCK_EX|LOCK_NB` 会拿不到，于是退化成无锁 Guard。这既覆盖了
/// `filter` 的 false 支，也固定了"锁不可重入"这个行为。
#[cfg(unix)]
#[test]
fn staging_lock_is_not_reentrant_across_fds() {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = temp.path().join("hash");
    fs::create_dir(&dir).expect("create dir");

    let _first = snapshot_staging_lock::hold(&dir);
    // 第一把锁仍在，GC 试锁必须失败（拿不到 Guard）。
    assert!(snapshot_staging_lock::try_acquire_for_gc(&dir).is_none());
    drop(_first);
    // 释放后可拿到（Guard 随即 drop 释放）。
    assert!(snapshot_staging_lock::try_acquire_for_gc(&dir).is_some());
}

/// R8: GC Guard 持有 flock 直到 drop。持锁期间用锁文件路径直接 flock
/// 验证并发 hold 无法取锁；Guard drop 后同一 fd 立刻可取。这正是
/// "试锁→解锁→删除"窗口被关闭的机制——旧实现试锁即解锁，删除前并发
/// boot 可以插进 staging。
#[cfg(unix)]
#[test]
fn gc_guard_holds_lock_until_dropped() {
    use std::os::unix::io::AsRawFd;
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = temp.path().join("hash");
    fs::create_dir(&dir).expect("create dir");

    let gc_guard = snapshot_staging_lock::try_acquire_for_gc(&dir).expect("gc acquires lock");
    let lock_path = snapshot_staging_lock::lock_path(&dir);
    let probe = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open lock file");
    // SAFETY: fd 由 `probe` 拥有，跨 syscall 存活。
    let held_rc = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_ne!(
        held_rc, 0,
        "a concurrent flock must fail while the GC guard holds the lock"
    );
    drop(gc_guard);
    // SAFETY: 同上。
    let released_rc = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(released_rc, 0, "once the guard drops, the lock is released");
    // SAFETY: 释放探测 fd 上的锁，避免影响后续测试。
    let _ = unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_UN) };
}

/// staging 锁挡住 GC：即便判据全中（无活 pid、mtime 已过期、无 journal），
/// 只要有进程持锁就跳过。这是"目录刚 mkdir、还没 stage 进文件"那个窗口的
/// 确定性排除——CI 上实测过 GC 删掉并发 boot 的 staging 目录导致对方
/// rename 报 ENOENT。
#[test]
fn orphaned_snapshot_root_gc_skips_dirs_held_by_staging_lock() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let dead_pid = reaped_dead_pid();
    let held = make_hash_dir(root, "aaa", dead_pid);
    backdate_dir(&held, 48 * 3600);

    // 持锁期间：GC 必须跳过。
    let guard = snapshot_staging_lock::hold(&held);
    let report = cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);
    assert!(held.exists(), "a lock-held dir must survive GC");
    assert_eq!(report.skipped_live, 1);
    assert_eq!(report.removed, 0);

    // 释放后：同一个目录立刻可回收，且同级锁文件一并清掉。
    drop(guard);
    let report = cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);
    assert!(!held.exists(), "once unlocked the dir is reclaimed");
    assert_eq!(report.removed, 1);
    assert!(
        !snapshot_staging_lock::lock_path(&held).exists(),
        "the sibling lock file must not accumulate"
    );
}

/// provider 插件回复的解析：成功与三类失败。
///
/// 抽成自由函数就是为了这个——每条失败路径直接喂字符串即可覆盖，不必真起
/// 一个会返回畸形数据的插件。
#[test]
fn parse_llm_reply_accepts_a_well_formed_completion() {
    let parts = super::parse_llm_reply(
        "llm_openai",
        r#"{"ok":true,"message":{"content":"hi","tool_calls":[]},
                "response_id":"r1","finish_reason":"stop"}"#,
    )
    .expect("well-formed reply");
    assert_eq!(parts.message.content.as_deref(), Some("hi"));
    assert_eq!(parts.response_id.as_deref(), Some("r1"));
    assert_eq!(parts.finish_reason.as_deref(), Some("stop"));
}

#[test]
fn parse_llm_reply_surfaces_provider_reported_failure() {
    let err = super::parse_llm_reply("llm_openai", r#"{"ok":false,"error":"upstream 500"}"#)
        .expect_err("ok=false must fail");
    let text = err.to_string();
    assert!(text.contains("upstream 500"), "{text}");
    assert!(
        text.contains("llm_openai"),
        "should name the plugin: {text}"
    );

    // ok=false 但没给 error：仍要失败，且给出可读的兜底说明。
    let err = super::parse_llm_reply("p", r#"{"ok":false}"#).expect_err("must fail");
    assert!(
        err.to_string().contains("without an error message"),
        "{err}"
    );
}

#[test]
fn parse_llm_reply_rejects_malformed_payloads() {
    // 非 JSON。
    let err = super::parse_llm_reply("p", "not json").expect_err("must fail");
    assert!(err.to_string().contains("is not JSON"), "{err}");
    // ok=true 但缺 message。
    let err = super::parse_llm_reply("p", r#"{"ok":true}"#).expect_err("must fail");
    assert!(err.to_string().contains("no message field"), "{err}");
    // message 结构不对。
    let err =
        super::parse_llm_reply("p", r#"{"ok":true,"message":"a string"}"#).expect_err("must fail");
    assert!(err.to_string().contains("malformed message"), "{err}");
}

/// 带 sink 时两个字段出现；不带时整个不出现——插件据此判定走非流式。
#[test]
fn attach_sink_fields_is_all_or_nothing() {
    let mut with = serde_json::json!({"node_id": "llm_complete"});
    super::attach_sink_fields(
        &mut with,
        Some(("127.0.0.1:5300".to_string(), "k1".to_string())),
    );
    assert_eq!(with["sink_addr"], serde_json::json!("127.0.0.1:5300"));
    assert_eq!(with["sink_key"], serde_json::json!("k1"));

    let mut without = serde_json::json!({"node_id": "llm_complete"});
    super::attach_sink_fields(&mut without, None);
    assert!(without.get("sink_addr").is_none());
    assert!(without.get("sink_key").is_none());
}

/// 摘要截断：超长加省略号，不超长原样返回。
#[test]
fn truncate_agent_excerpt_text_adds_ellipsis_only_when_cut() {
    // 空白被压平成单空格。
    assert_eq!(super::truncate_agent_excerpt_text("a   b\n c", 32), "a b c");
    // 恰好等于上限：不加省略号。
    assert_eq!(super::truncate_agent_excerpt_text("abcde", 5), "abcde");
    // 超出：截断并加省略号。
    assert_eq!(super::truncate_agent_excerpt_text("abcdef", 5), "abcde...");
}

/// sink key 每次调用都不同——端口本就一调用一换，key 是并发下拒绝串台
/// 连接的第二道闸，重复就失去意义。
#[test]
fn sink_keys_are_unique_per_call() {
    let a = super::new_sink_key();
    let b = super::new_sink_key();
    assert_ne!(a, b);
    assert!(a.starts_with("sink-"), "{a}");
}

/// journal **同名目录**不算 journal，必须照常回收。
///
/// `clear_journal_remove_failure_when_path_is_nonempty_dir` 之类的测试会
/// 故意把 journal 路径造成非空目录以迫使 `remove_file` 失败；早期只按
/// 文件名判断的实现把这类残渣误当成崩溃恢复状态而永久保留（实测 338 个
/// 同名条目里 229 个是目录，占住 GC 该回收的空间）。
#[test]
fn orphaned_snapshot_root_gc_ignores_journal_named_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let dead_pid = reaped_dead_pid();

    let orphan = make_hash_dir(root, "aaa", dead_pid);
    let journal_as_dir = orphan.join("plugin-iteration-edit-journal.json");
    fs::create_dir_all(&journal_as_dir).expect("create journal-named dir");
    fs::write(journal_as_dir.join("blocker.txt"), b"x").expect("write blocker");
    backdate_dir(&orphan, 48 * 3600);

    let report = cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);

    assert!(
        !orphan.exists(),
        "a journal-named directory is test debris, not recovery state"
    );
    assert_eq!(report.skipped_journal, 0);
    assert_eq!(report.removed, 1);
}

/// 空 hash 目录**不**能无视保留期删除：它可能是另一个正在 boot 的进程刚
/// mkdir、还没 stage 进文件的 staging 目录，此刻既无 `snapshot-{pid}-*`
/// 可探活又是空的，立刻删会让那个进程随后 rename 时报 ENOENT。
/// CI 上实测过这个竞态（命中 `dispatch_unknown_command_is_reply` 的 boot），
/// mtime 闸门是唯一能挡住它的东西。
#[test]
fn orphaned_snapshot_root_gc_keeps_fresh_empty_dirs_but_reaps_aged_ones() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();

    // 刚建的空目录：可能是并发 boot 的 in-flight staging，必须保留。
    let fresh = root.join("aaa");
    fs::create_dir(&fresh).expect("create fresh empty dir");
    let report = cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);
    assert!(
        fresh.exists(),
        "a just-created empty dir may be another process's in-flight staging"
    );
    assert_eq!(report.skipped_recent, 1);
    assert_eq!(report.removed, 0);

    // 过了保留期的空目录：确定是残渣，回收。
    backdate_dir(&fresh, 48 * 3600);
    let report = cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);
    assert!(!fresh.exists(), "an aged empty dir is reclaimed");
    assert_eq!(report.removed, 1);
    assert_eq!(report.bytes_reclaimed, 0);
}

#[test]
fn orphaned_snapshot_root_gc_skips_own_root() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let dead_pid = reaped_dead_pid();
    // 即便判据全中（死 pid + 老 mtime），当前进程正在用的 root 也不能自删。
    let own = make_hash_dir(root, "aaa", dead_pid);
    backdate_dir(&own, 48 * 3600);

    let report =
        cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), Some(&own), false);

    assert!(
        own.exists(),
        "the live process's own snapshot root is never removed"
    );
    assert_eq!(report.removed, 0);
    // skip_root 在计数前就被跳过，不计入 scanned。
    assert_eq!(report.scanned, 0);
}

#[test]
fn orphaned_snapshot_root_gc_dry_run_reports_without_deleting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let dead_pid = reaped_dead_pid();
    let orphan = make_hash_dir(root, "aaa", dead_pid);
    backdate_dir(&orphan, 48 * 3600);

    let report = cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, true);

    assert!(orphan.exists(), "dry-run must not delete anything");
    assert_eq!(
        report.removed, 1,
        "dry-run still reports what would be removed"
    );
    assert_eq!(report.bytes_reclaimed, 10);
}

#[test]
fn orphaned_snapshot_root_gc_zero_max_age_expires_everything() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let dead_pid = reaped_dead_pid();
    // 刚建的目录，配合 retention=0（config 的 Some(0) 语义）应立即可回收。
    let fresh = make_hash_dir(root, "aaa", dead_pid);

    let report = cleanup_orphaned_snapshot_roots(root, Duration::ZERO, None, false);

    assert!(
        !fresh.exists(),
        "retention=0 expires even a just-created dir"
    );
    assert_eq!(report.removed, 1);
}

/// GC 的三处防御性早退臂：`host_root` 不可 read_dir、hash 目录本身不可
/// read_dir、以及目录项不是目录。都不是"不可能发生"——权限变化、并发删除
/// 都会走到，且 100% 行覆盖门槛要求它们被执行到。
#[cfg(unix)]
#[test]
fn orphaned_snapshot_root_gc_tolerates_unreadable_and_non_dir_entries() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();

    // 目录项不是目录：普通文件必须被跳过而不是当 hash 目录处理。
    fs::write(root.join("stray-file"), b"x").expect("write stray file");
    let report = cleanup_orphaned_snapshot_roots(root, Duration::ZERO, None, false);
    assert_eq!(report.scanned, 0, "plain files are not hash dirs");
    assert!(
        root.join("stray-file").exists(),
        "plain files are untouched"
    );

    // hash 目录不可 read_dir：跳过该目录，不 panic、不误删。root 绕过 mode
    // 位，故只在非特权用户下有意义；用 for-over-Option 门控而非 `if`——
    // `if` 会留下当前 euid 不走的那个臂永久无覆盖。
    for () in probe_not_root().into_iter() {
        let locked = root.join("locked");
        fs::create_dir(&locked).expect("create locked dir");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod 000");
        let report = cleanup_orphaned_snapshot_roots(root, Duration::ZERO, None, false);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("restore mode");
        assert!(locked.exists(), "an unreadable hash dir is left alone");
        assert_eq!(report.removed, 0);
    }

    // host_root 整体不可 read_dir：返回空报告。
    let missing = root.join("does-not-exist");
    let report = cleanup_orphaned_snapshot_roots(&missing, Duration::ZERO, None, false);
    assert_eq!(report, SnapshotGcReport::default());
}

/// `dir_is_older_than` 取不到 mtime 时保守返回 false（宁可留着不误删），
/// `dir_size_bytes` 对不可 read_dir 的目录返回 0，且不跟随符号链接。
#[cfg(unix)]
#[test]
fn dir_age_and_size_helpers_handle_unreadable_and_symlinks() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();

    // 不存在的路径拿不到 metadata → 判"不老"，避免误删。
    assert!(
        !dir_is_older_than(&root.join("nope"), Duration::ZERO),
        "missing mtime must not be treated as expired"
    );
    // 不存在的路径 size 为 0。
    assert_eq!(dir_size_bytes(&root.join("nope")), 0);

    // 符号链接不跟随：只算真实文件，不把链接目标的字节数计进来。
    let real = root.join("real.bin");
    fs::write(&real, b"0123456789").expect("write real file");
    let inner = root.join("inner");
    fs::create_dir(&inner).expect("create inner dir");
    std::os::unix::fs::symlink(&real, inner.join("link.bin")).expect("symlink");
    assert_eq!(
        dir_size_bytes(&inner),
        0,
        "symlinks are not followed, so the target's bytes are not counted"
    );
    // 真实文件被递归计入。
    assert_eq!(dir_size_bytes(root), 10);
}

/// `staged_artifact_root_is_removable` 的三条拒绝理由与一条放行。
///
/// 空路径是 `reload_subtree` 的产物（它不建 staging 目录），当路径删会打到
/// CWD；等于或不在 snapshot_root 之下则会连带删掉
/// `plugin-iteration-edit-journal.json` 这类崩溃恢复状态。
#[test]
fn staged_root_removable_rejects_empty_and_out_of_tree_paths() {
    let snapshot_root = Path::new("/tmp/snaps");
    // 正常情况：snapshot_root 下的子目录可删。
    assert!(staged_artifact_root_is_removable(
        &snapshot_root.join("snapshot-1-2"),
        snapshot_root
    ));
    // 空路径（reload_subtree）。
    assert!(!staged_artifact_root_is_removable(
        Path::new(""),
        snapshot_root
    ));
    // 等于 snapshot_root 本身。
    assert!(!staged_artifact_root_is_removable(
        snapshot_root,
        snapshot_root
    ));
    // 完全在别处。
    assert!(!staged_artifact_root_is_removable(
        Path::new("/tmp/elsewhere/snapshot-1-2"),
        snapshot_root
    ));
}

// Shared read-only host against the real fixtures tree. The fixture
// dylibs under `fixtures/artifacts/` are rebuilt natively for the current
// host target (arm64 macOS included), so a real `RuntimeHost::boot`
// succeeds here without the x86_64-linux-only gate that used to guard
// these tests. Booting is cheap (the loader only reads `artifacts/`); we
// still boot once and share it, gating the mutating tests with `serial`
// so they don't race on the shared temp snapshot root.
//
// Flaky-guard (mirrors `tests/host_coverage.rs::shared_host`): a full
// suite run may rebuild the fixture dylibs mid-suite, leaving
// `artifacts/index.json`'s recorded sha256 stale relative to the on-disk
// dylib and failing boot with `PluginUnavailable { HashMismatch }`.
// Re-hash every staged artifact and rewrite the index right before
// booting so the shared host always sees a consistent index.
static SHARED_HOST: OnceLock<RuntimeHost> = OnceLock::new();

fn shared_host() -> &'static RuntimeHost {
    SHARED_HOST.get_or_init(|| {
        let root = repo_fixtures_root();
        cordis_tooling::refresh_artifact_index(&root)
            .expect("refresh artifact index before shared boot");
        RuntimeHost::boot(&root).expect("host should boot")
    })
}

fn collect_plugin_paths(plugin_root: &Path, subtree: &Path, paths: &mut Vec<String>) {
    if subtree.join("Cargo.toml").exists() {
        let relative = subtree
            .strip_prefix(plugin_root)
            .expect("subtree should stay inside plugin root");
        paths.push(relative.to_string_lossy().replace('\\', "/"));
    }

    let mut children = fs::read_dir(subtree)
        .expect("read subtree")
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("");
            if matches!(name, "src" | "tests" | "docs" | "target") {
                return None;
            }
            entry
                .file_type()
                .ok()
                .filter(|ty| ty.is_dir())
                .map(|_| path)
        })
        .collect::<Vec<_>>();
    children.sort();
    for child in children {
        collect_plugin_paths(plugin_root, &child, paths);
    }
}

#[test]
fn sanitize_child_plugin_segment_keeps_mod_path_component() {
    assert_eq!(sanitize_child_plugin_segment("mod"), "mod");
}

#[test]
fn child_plugin_test_template_is_smoke_only() {
    let rendered = render_child_plugin_test("expr_evaluator_mod");
    assert!(rendered.contains("scaffold_exports_apply"));
    assert!(rendered.contains("let _ = apply(5.0, 2.0);"));
    assert!(!rendered.contains("is_err"));
}

#[test]
fn child_plugin_core_template_matches_shared_wrapper_pattern() {
    let rendered = render_child_plugin_core("mod");
    assert!(rendered.contains("pub struct ModPlugin;"));
    assert!(rendered.contains("#[allow(dead_code)]"));
    assert!(rendered.contains("pub fn apply(lhs: f64, rhs: f64)"));
}

#[test]
fn extract_warning_blocks_keeps_separate_diagnostics() {
    let warnings = extract_warning_blocks(
            "warning: function `apply` is never used\n  --> plugins/expr/evaluator/mod/src/core.rs:23:8\n\nwarning: field `modulo` is never read\n  --> plugins/expr/evaluator/src/core.rs:15:5\n",
        );
    assert_eq!(warnings.len(), 2);
    assert!(warnings[0].contains("core.rs:23:8"));
    assert!(warnings[1].contains("field `modulo`"));
}

#[test]
fn warning_detection_ignores_environment_noise_outside_changed_paths() {
    let operations = vec![PluginEditOperation {
        path: "plugins/expr/evaluator/mod/src/core.rs".to_string(),
        kind: PluginEditOpKind::ReplaceExact,
        expected_old_string: Some("old".to_string()),
        expected_sha256: None,
        new_content: Some("new".to_string()),
        pointer: None,
        dotted_key: None,
        value: None,
    }];
    let diagnostics = warning_diagnostics_for_changed_paths(
            "",
            "sh: 6: /etc/profile.d/clab-notify.sh: [[: not found\nwarning: function `apply` is never used\n  --> plugins/expr/evaluator/mod/src/core.rs:23:8\n   |\n23 | pub fn apply(lhs: f64, rhs: f64) -> Result<f64, ModError> {\n   |        ^^^^^\n",
            &operations,
            Path::new("/tmp/fixtures"),
        );
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].contains("function `apply` is never used"));
    assert!(!diagnostics[0].contains("clab-notify"));
}

#[test]
fn strip_ansi_sequences_removes_csi_and_keeps_plain_text() {
    use super::strip_ansi_sequences;
    assert_eq!(
        strip_ansi_sequences(
            "\u{1b}[1m\u{1b}[33mwarning\u{1b}[0m\u{1b}[1m: unused import\u{1b}[0m"
        ),
        "warning: unused import"
    );
    assert_eq!(
        strip_ansi_sequences("no escapes at all"),
        "no escapes at all"
    );
    // 非 CSI 的 ESC 双字符序列同样被剥掉，不残留 ESC。
    assert_eq!(strip_ansi_sequences("a\u{1b}cb"), "ab");
    // 文本截断在未终结的 CSI 序列中间时不 panic。
    assert_eq!(strip_ansi_sequences("tail\u{1b}[1;3"), "tail");
}

#[test]
fn warning_detection_survives_ansi_colored_output() {
    let operations = vec![PluginEditOperation {
        path: "plugins/expr/evaluator/dist/src/core.rs".to_string(),
        kind: PluginEditOpKind::ReplaceExact,
        expected_old_string: Some("old".to_string()),
        expected_sha256: None,
        new_content: Some("new".to_string()),
        pointer: None,
        dotted_key: None,
        value: None,
    }];
    // CI 实测字节序列：CARGO_TERM_COLOR=always 下嵌套 cargo 的 stderr。
    let colored_stderr = "\u{1b}[1m\u{1b}[33mwarning\u{1b}[0m\u{1b}[1m: unused import: `std::fmt`\u{1b}[0m\n \u{1b}[1m\u{1b}[94m--> \u{1b}[0mexpr/src/../evaluator/src/../dist/src/core.rs:2:5\n  \u{1b}[1m\u{1b}[94m|\u{1b}[0m\n\u{1b}[1m\u{1b}[94m2\u{1b}[0m \u{1b}[1m\u{1b}[94m|\u{1b}[0m use std::fmt;\n  \u{1b}[1m\u{1b}[94m|\u{1b}[0m     \u{1b}[1m\u{1b}[33m^^^^^^^^\u{1b}[0m\n";
    let diagnostics = warning_diagnostics_for_changed_paths(
        "",
        colored_stderr,
        &operations,
        Path::new("/tmp/fixtures"),
    );
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].contains("unused import: `std::fmt`"));
    assert!(
        !diagnostics[0].contains('\u{1b}'),
        "excerpt sent to the LLM must be free of escape sequences"
    );
}

#[test]
fn warning_detection_matches_folded_cargo_source_paths() {
    let operations = vec![PluginEditOperation {
        path: "plugins/expr/evaluator/mod/src/core.rs".to_string(),
        kind: PluginEditOpKind::ReplaceExact,
        expected_old_string: Some("old".to_string()),
        expected_sha256: None,
        new_content: Some("new".to_string()),
        pointer: None,
        dotted_key: None,
        value: None,
    }];
    let diagnostics = warning_diagnostics_for_changed_paths(
            "",
            "warning: unused import: `std::fmt`\n --> expr/src/../evaluator/src/../mod/src/core.rs:2:5\n  |\n2 | use std::fmt;\n  |     ^^^^^^^^\n",
            &operations,
            Path::new("/tmp/fixtures"),
        );
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].contains("unused import: `std::fmt`"));
}

#[test]
fn sort_plugin_context_paths_uses_structural_order() {
    let mut paths = vec![
        "plugins/expr/docs/human/overview.md".to_string(),
        "plugins/expr/tests/eval.rs".to_string(),
        "plugins/expr/src/lib.rs".to_string(),
        "plugins/expr/Cargo.toml".to_string(),
        "plugins/expr/evaluator/src/core.rs".to_string(),
    ];
    sort_plugin_context_paths(&mut paths);
    assert_eq!(
        paths,
        vec![
            "plugins/expr/Cargo.toml".to_string(),
            "plugins/expr/evaluator/src/core.rs".to_string(),
            "plugins/expr/src/lib.rs".to_string(),
            "plugins/expr/tests/eval.rs".to_string(),
            "plugins/expr/docs/human/overview.md".to_string(),
        ]
    );
}

#[test]
fn collect_plugin_context_paths_focuses_structural_source_anchors_through_depth_two() {
    let fixtures_root = repo_fixtures_root();
    let plugin_root = fixtures_root.join("plugins");
    let expr_root = plugin_root.join("expr");
    let mut target_plugin_paths = Vec::new();
    collect_plugin_paths(&plugin_root, &expr_root, &mut target_plugin_paths);

    let context_paths = collect_plugin_context_paths(&fixtures_root, "expr", &target_plugin_paths)
        .expect("context paths");

    assert!(context_paths
        .focus_paths
        .contains(&"plugins/expr/Cargo.toml".to_string()));
    assert!(context_paths
        .focus_paths
        .contains(&"plugins/expr/src/lib.rs".to_string()));
    assert!(context_paths
        .focus_paths
        .contains(&"plugins/expr/tests/eval.rs".to_string()));
    assert!(context_paths
        .focus_paths
        .contains(&"plugins/expr/evaluator/Cargo.toml".to_string()));
    assert!(context_paths
        .focus_paths
        .contains(&"plugins/expr/evaluator/src/core.rs".to_string()));
    assert!(context_paths
        .focus_paths
        .contains(&"plugins/expr/lexer/src/core.rs".to_string()));
    assert!(context_paths
        .focus_paths
        .contains(&"plugins/expr/evaluator/add/src/core.rs".to_string()));
    assert!(context_paths
        .focus_paths
        .contains(&"plugins/expr/evaluator/add/src/lib.rs".to_string()));
    assert!(context_paths
        .focus_paths
        .contains(&"plugins/expr/evaluator/div/src/core.rs".to_string()));
    assert!(!context_paths
        .focus_paths
        .contains(&"plugins/expr/evaluator/add/tests/add.rs".to_string()));
    assert!(!context_paths
        .focus_paths
        .contains(&"plugins/expr/evaluator/div/docs/human/overview.md".to_string()));
    assert!(context_paths
        .all_paths
        .contains(&"plugins/expr/evaluator/add/src/core.rs".to_string()));
    assert!(context_paths
        .all_paths
        .contains(&"plugins/expr/evaluator/div/src/core.rs".to_string()));
}

#[test]
fn scaffold_integration_edits_require_host_source_or_tests() {
    let scaffolded_children = vec![ScaffoldedChildRegistration {
        parent_manifest_path: "plugins/expr/evaluator/Cargo.toml".to_string(),
        child_root_path: "plugins/expr/evaluator/mod".to_string(),
    }];
    let scaffold_only = vec![
        PluginEditOperation {
            path: "plugins/expr/evaluator/Cargo.toml".to_string(),
            kind: PluginEditOpKind::TomlSet,
            expected_old_string: None,
            expected_sha256: Some("sha".to_string()),
            new_content: None,
            pointer: None,
            dotted_key: Some("package.metadata.cordis.children".to_string()),
            value: None,
        },
        PluginEditOperation {
            path: "plugins/expr/evaluator/mod/src/core.rs".to_string(),
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some("old".to_string()),
            expected_sha256: None,
            new_content: Some("new".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        },
    ];
    assert!(ensure_scaffold_integration_edits(&scaffolded_children, &scaffold_only).is_err());

    let mut integrated = scaffold_only.clone();
    integrated.push(PluginEditOperation {
        path: "plugins/expr/evaluator/src/core.rs".to_string(),
        kind: PluginEditOpKind::ReplaceExact,
        expected_old_string: Some("old".to_string()),
        expected_sha256: None,
        new_content: Some("new".to_string()),
        pointer: None,
        dotted_key: None,
        value: None,
    });
    assert!(ensure_scaffold_integration_edits(&scaffolded_children, &integrated).is_ok());
}

#[test]
#[serial]
fn plugin_iteration_tool_surface_and_context_reads_expand_from_focus_to_all() {
    cordis_tooling::refresh_artifact_index(&repo_fixtures_root())
        .expect("refresh artifact index before boot");
    let fixtures_root = repo_fixtures_root();
    let host = RuntimeHost::boot(&fixtures_root).expect("host should boot");
    let snapshot = host.current_snapshot();
    let prepared = host
        .kernel
        .begin_plugin_iteration(
            snapshot.as_ref(),
            &KernelPluginIterationRequest {
                issue_id: None,
                target_plugin_paths: vec!["expr".to_string()],
                instruction: Some("inspect expr subtree".to_string()),
                edit_plan: None,
                manual_approved: false,
                tests_command: None,
                safety_command: None,
                verify_profile: None,
                quality_score: None,
            },
        )
        .expect("prepare iteration");
    let iteration_id = prepared.iteration_id.clone();
    let context_paths = collect_plugin_context_paths(
        &fixtures_root,
        &prepared.root_plugin_path,
        &prepared.target_plugin_paths,
    )
    .expect("collect context paths");
    let mut state = PluginIterationAgentState::new(prepared, context_paths, &fixtures_root);
    let mut backend = PluginIterationAgentBackend {
        host: &host,
        state: &mut state,
    };

    let initial_tools = backend.tool_specs();
    assert!(initial_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES));
    assert!(initial_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES));
    assert!(initial_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG));
    assert!(initial_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN));
    assert!(initial_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT));
    assert!(initial_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT));
    assert!(initial_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_CREATE_FILE));
    assert!(initial_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_DELETE_FILE));
    assert!(initial_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_TOML_SET));
    assert!(initial_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_JSON_SET));

    let focus = backend.list_context_files(ContextFilesScope::Focus);
    let focus_paths = focus
        .get("focus_paths")
        .and_then(|value| value.as_array())
        .expect("focus paths array")
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<Vec<_>>();
    assert!(focus_paths.contains(&"plugins/expr/lexer/src/core.rs"));
    assert!(focus_paths.contains(&"plugins/expr/evaluator/src/core.rs"));
    assert!(focus_paths.contains(&"plugins/expr/evaluator/div/src/core.rs"));
    assert!(!focus_paths.contains(&"plugins/expr/evaluator/div/tests/div.rs"));

    let hidden_err = backend
        .read_context_files(&["plugins/expr/evaluator/div/tests/div.rs".to_string()])
        .expect_err("deep non-source file should require explicit expansion");
    assert!(hidden_err
        .to_string()
        .contains("hidden behind the structural focus shortlist"));

    let expanded = backend.list_context_files(ContextFilesScope::All);
    let expanded_paths = expanded
        .get("paths")
        .and_then(|value| value.as_array())
        .expect("expanded paths array")
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<Vec<_>>();
    assert!(expanded_paths.contains(&"plugins/expr/evaluator/div/src/core.rs"));
    assert!(expanded_paths.contains(&"plugins/expr/evaluator/div/tests/div.rs"));

    let expanded_tools = backend.tool_specs();
    assert!(expanded_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG));
    assert!(expanded_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_CREATE_FILE));
    assert!(expanded_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_DELETE_FILE));
    assert!(expanded_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_TOML_SET));
    assert!(expanded_tools
        .iter()
        .any(|tool| tool.name == PLUGIN_AGENT_TOOL_JSON_SET));

    let deep_read = backend
        .read_context_files(&["plugins/expr/evaluator/div/tests/div.rs".to_string()])
        .expect("expanded context should allow reading deep test file");
    assert!(deep_read.to_string().contains("DivisionByZero"));

    host.kernel.finish_plugin_iteration(&iteration_id);
}

#[test]
fn validated_verification_command_accepts_short_check_and_test_aliases() {
    let check = super::validated_verification_command(
        Some("check".to_string()),
        Some("cargo check --quiet --manifest-path plugins/Cargo.toml".to_string()),
        "cargo check",
    )
    .expect("check alias should use default command");
    assert_eq!(
        check,
        "cargo check --quiet --manifest-path plugins/Cargo.toml"
    );

    let test = super::validated_verification_command(
        Some("test".to_string()),
        Some("cargo test --quiet --manifest-path plugins/Cargo.toml".to_string()),
        "cargo test",
    )
    .expect("test alias should use default command");
    assert_eq!(
        test,
        "cargo test --quiet --manifest-path plugins/Cargo.toml"
    );

    let empty = super::validated_verification_command(
        super::normalize_optional_command(Some(String::new())),
        Some("cargo check --quiet --manifest-path plugins/Cargo.toml".to_string()),
        "cargo check",
    )
    .expect("empty command should fall back to default");
    assert_eq!(
        empty,
        "cargo check --quiet --manifest-path plugins/Cargo.toml"
    );
}

#[test]
#[serial]
fn drop_session_clears_all_maps() {
    let host = shared_host();
    // Boot may hydrate leftover session snapshots from data/sessions
    // (crash recovery), so assert deltas against the post-boot baseline
    // instead of absolute sizes.
    let (a0, p0, f0) = host.debug_session_map_sizes();
    let handle = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("agent should start");
    let session_id = handle.session_id;
    // agent_start populates agent_sessions + profile_fallback; the
    // pending_session_actions entry is created lazily.
    assert_eq!(host.debug_session_map_sizes(), (a0 + 1, p0, f0 + 1));
    host.drop_session(&session_id);
    assert_eq!(host.debug_session_map_sizes(), (a0, p0, f0));
}

#[test]
#[serial]
fn drop_session_idempotent_on_missing() {
    let host = shared_host();
    // Boot may hydrate leftover snapshots (crash recovery); baseline
    // against whatever is there rather than assuming empty maps.
    let baseline = host.debug_session_map_sizes();
    // Dropping an unknown session id twice must not panic or error, and
    // must leave the maps untouched.
    host.drop_session("no-such-session");
    host.drop_session("no-such-session");
    assert_eq!(host.debug_session_map_sizes(), baseline);
}

#[test]
#[serial]
fn refresh_session_soul_unknown_sid_errors() {
    let host = shared_host();
    let err = host
        .refresh_session_soul("no-such-session", "user:42")
        .expect_err("unknown session should error");
    assert!(matches!(err, RuntimeError::AgentSessionNotFound { .. }));
}

// P4: 未知 sid 不能写 caller 角色位。
#[test]
#[serial]
fn refresh_session_caller_unknown_sid_errors() {
    use crate::agent::AgentToolHost;

    let host = shared_host();
    let err = host
        .refresh_session_caller("no-such-session", true, true, false)
        .expect_err("unknown session should error");
    assert!(matches!(err, RuntimeError::AgentSessionNotFound { .. }));
    // 读侧同样无事实可报：None 表示"宿主判断不了"，门控不生效。
    assert_eq!(host.agent_session_caller("no-such-session"), None);
}

// P4: 已知 sid 写入后能原样读回；默认是"非群聊、非管理员"。
#[test]
#[serial]
fn refresh_session_caller_roundtrips_policy() {
    use crate::agent_api::AgentToolHost;
    use crate::agent_tools::SessionCaller;

    let host = shared_host();
    let handle = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("agent should start");
    let session_id = handle.session_id;
    // 新会话未绑定时既不是群聊也不是管理员（安全默认）。
    assert_eq!(
        host.agent_session_caller(&session_id),
        Some(SessionCaller {
            is_group: false,
            is_admin: false,
            can_approve_iteration: false,
        })
    );
    // The approval capability is a separate channel fact: it can be
    // granted and revoked independently of the room role.
    host.refresh_session_caller(&session_id, true, true, false)
        .expect("known session should accept the policy");
    assert_eq!(
        host.agent_session_caller(&session_id),
        Some(SessionCaller {
            is_group: true,
            is_admin: true,
            can_approve_iteration: false,
        })
    );
    host.refresh_session_caller(&session_id, true, false, true)
        .expect("capability refresh");
    let caller = host.agent_session_caller(&session_id).expect("caller");
    assert!(!caller.is_admin);
    assert!(caller.can_approve_iteration);
    // 会话自己也能读回同一组旗标。
    let guard = host.agent_sessions_mut();
    let managed = guard.get(&session_id).expect("session should exist");
    assert_eq!(managed.session.caller_policy(), (true, false));
    assert!(managed.session.can_approve_iteration());
}

// Build a one-node `RuntimeSnapshot` whose single plugin is registered as
// `Loaded` with valid docs (so its node enters the registered net as a
// Task transition), but whose `artifact_path` points at a file that does
// not exist. `invoke_registered_plugin` then fails when it tries to open
// the dylib, so `execute_registered_target` takes the invoke-`Err` trace
// arm (host.rs:204-218): outcome=Failure with the error string recorded.
fn snapshot_with_broken_artifact_node() -> super::RuntimeSnapshot {
    use crate::core::models::{ArtifactKind, PluginDocs};
    use cordis_plugin_sdk::{AbiFingerprint, NodeDoc, NodeType};

    let plugin_path = "brokenplug".to_string();
    let node_id = "broken_entry".to_string();
    let docs = PluginDocs {
        plugin_id: plugin_path.clone(),
        plugin_path: plugin_path.clone(),
        plugin_version: "0.1.0".to_string(),
        abi_version: 2,
        command_name: None,
        nodes: vec![NodeDoc {
            id: node_id.clone(),
            summary: "always-failing node backed by a missing dylib".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "value": { "type": "number" } }
            }),
            output_schema: serde_json::json!({ "type": "object" }),
            side_effects: Vec::new(),
            failure_modes: vec!["artifact missing".to_string()],
            node_type: NodeType::Task,
            agent_accessible: true,
        }],
        system_hint: None,
    };

    let plugin_registry = crate::plugin::registry::PluginRegistry::default();
    plugin_registry.insert_loaded(
        plugin_path.clone(),
        None,
        true,
        std::collections::BTreeSet::new(),
        docs.clone(),
        // A dylib path that does not exist on disk: `LoadedDylibApi::open`
        // fails and `invoke_registered_plugin` returns an `Err`.
        PathBuf::from("/nonexistent/cordis-broken-artifact.dylib"),
        ArtifactKind::Dylib,
        AbiFingerprint::current_build("crate_broken_v1", "api_v2"),
        None,
    );

    let mut node_registry = crate::plugin::registry::NodeRegistry::default();
    node_registry
        .register_from_docs(&plugin_path, &docs)
        .expect("register broken node");

    let doc_registry =
        crate::service::doc_registry::DocRegistry::from_plugin_registry(&plugin_registry);
    let graph_registry = crate::service::graph_registry::GraphRegistry::from_registries(
        &plugin_registry,
        &node_registry,
    );

    super::runtime_snapshot_from_output(
        crate::plugin::loader::LoadOutput {
            execution_id: "snapshot-broken-artifact".to_string(),
            plugin_registry,
            node_registry,
            doc_registry,
            graph_registry,
            context: crate::context::RuntimeContext::default(),
            metrics: crate::plugin::loader::LoaderMetrics::default(),
        },
        PathBuf::from("/tmp/cordis-broken-artifact-staged"),
        BTreeMap::new(),
    )
}

#[test]
fn execute_registered_target_records_failure_trace_when_invoke_errs() {
    let snapshot = snapshot_with_broken_artifact_node();
    let result = snapshot
        .execute_registered_target(
            "brokenplug::broken_entry",
            serde_json::json!({ "value": 1 }),
        )
        .expect("execute returns a result even when the node invoke fails");

    assert_eq!(result.target_node_fqn, "brokenplug::broken_entry");
    let trace = result
        .traces
        .get("brokenplug::broken_entry")
        .expect("failing node should have a trace entry");
    // The invoke-Err arm records plugin/node identity, a Failure outcome,
    // the request payload, no response payload, and the error string.
    assert_eq!(trace.outcome, Some(NodeOutcome::Failure));
    assert_eq!(trace.plugin_path, "brokenplug");
    assert_eq!(trace.node_id, "broken_entry");
    assert!(trace.response_payload.is_none());
    assert!(
        trace.request_payload.is_some(),
        "request payload should be captured before the failed invoke"
    );
    let err = trace
        .error
        .as_deref()
        .expect("failure trace should carry the invoke error");
    assert!(!err.is_empty(), "invoke error string should be non-empty");
    // The overall net outcome for the target is Failure.
    assert_eq!(
        result.output.outcomes.get("brokenplug::broken_entry"),
        Some(&NodeOutcome::Failure)
    );
}
