//! Iteration policy used by kernel self-iteration loop.
//! This is the safety boundary for "what can be changed automatically".

/// Lexically normalize a path without touching the filesystem.
///
/// Rules:
/// - components are split on `/`;
/// - empty components (repeated or leading/trailing `/`) and `.` are dropped;
/// - a `..` component pops the previous regular component; when there is no
///   regular component to pop (the stack is empty or only `..` remain) the
///   `..` is **kept** in the output — this records that the path escapes above
///   the root, and callers must refuse any normalized path that still contains
///   a `..` component (see [`IterationPolicy::paths_allowed`]);
/// - the result is joined with single `/` and has no leading or trailing `/`.
///
/// Examples: `crates/x/../y.rs` → `crates/y.rs`, `crates/../config/x.toml` →
/// `config/x.toml`, `../etc/passwd` → `../etc/passwd` (escape kept).
pub fn normalize_path_lexically(path: &str) -> String {
    let mut components: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                let can_pop = matches!(components.last(), Some(c) if *c != "..");
                if can_pop {
                    components.pop();
                } else {
                    components.push("..");
                }
            }
            other => components.push(other),
        }
    }
    components.join("/")
}

/// Returns true when a lexically normalized path still contains a `..`
/// component, i.e. it escapes above the root and must not be admitted.
fn escapes_above_root(normalized: &str) -> bool {
    normalized.split('/').any(|component| component == "..")
}

/// Component-level prefix match: `normalized_path` starts with
/// `normalized_prefix` at a component boundary. Both inputs must already be
/// lexically normalized. An empty prefix (e.g. `/` or ``) denotes the root
/// and matches every path.
fn path_starts_with_component(normalized_path: &str, normalized_prefix: &str) -> bool {
    if normalized_prefix.is_empty() {
        return true;
    }
    let path_components: Vec<&str> = normalized_path.split('/').collect();
    let prefix_components: Vec<&str> = normalized_prefix.split('/').collect();
    path_components.len() >= prefix_components.len()
        && path_components[..prefix_components.len()] == prefix_components[..]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IterationPolicy {
    /// Allowed path prefixes for auto-changes.
    pub path_allowlist: Vec<String>,
    /// Sensitive path prefixes that require manual approval when touched.
    pub sensitive_path_prefixes: Vec<String>,
    /// If true, touching sensitive paths requires manual approval.
    pub require_manual_approval_for_sensitive: bool,
    /// Max total changed lines allowed for one iteration.
    pub max_diff_lines: usize,
    /// Max wall-clock budget for one iteration.
    pub time_budget_ms: u64,
}

impl Default for IterationPolicy {
    fn default() -> Self {
        Self {
            path_allowlist: vec![
                "crates/".to_string(),
                "docs/".to_string(),
                "tests/".to_string(),
            ],
            sensitive_path_prefixes: vec![
                "crates/cordis-runtime/src/core/".to_string(),
                "crates/cordis-runtime/src/plugin/".to_string(),
                "crates/cordis-runtime/src/kernel/".to_string(),
            ],
            require_manual_approval_for_sensitive: true,
            max_diff_lines: 500,
            time_budget_ms: 60_000,
        }
    }
}

impl IterationPolicy {
    /// Returns true when all changed paths are explicitly allowed.
    ///
    /// Paths are matched **lexically normalized** (no filesystem access): `..`
    /// and `.` segments are resolved first, so `crates/../config/secret.toml`
    /// is judged as `config/secret.toml` instead of slipping past the
    /// `crates/` allowlist entry. Paths that escape above the root (their
    /// normalized form still contains a `..` component) are never allowed.
    pub fn paths_allowed(&self, changed_paths: &[String]) -> bool {
        changed_paths.iter().all(|path| {
            let normalized = normalize_path_lexically(path);
            // ".." left over after normalization means the path escapes above
            // the root; such paths are refused outright.
            if escapes_above_root(&normalized) {
                return false;
            }
            self.path_allowlist
                .iter()
                .any(|prefix| path_starts_with_component(&normalized, &normalize_path_lexically(prefix)))
        })
    }

    /// Returns true when changed size is within the configured budget.
    pub fn diff_allowed(&self, diff_lines: usize) -> bool {
        diff_lines <= self.max_diff_lines
    }

    /// Returns true when the changed set touches at least one sensitive path prefix.
    ///
    /// Uses the same lexical normalization as [`IterationPolicy::paths_allowed`],
    /// so a `..` path is judged by its resolved location
    /// (e.g. `crates/cordis-runtime/src/plugin/../core/models.rs` is detected as
    /// touching `crates/cordis-runtime/src/core/`). Paths escaping above the
    /// root are not under any sensitive prefix (and are refused by
    /// `paths_allowed` anyway), so they are reported as non-sensitive.
    pub fn touches_sensitive_paths(&self, changed_paths: &[String]) -> bool {
        changed_paths.iter().any(|path| {
            let normalized = normalize_path_lexically(path);
            if escapes_above_root(&normalized) {
                return false;
            }
            self.sensitive_path_prefixes
                .iter()
                .any(|prefix| path_starts_with_component(&normalized, &normalize_path_lexically(prefix)))
        })
    }

    /// Returns true when the manual safety gate condition is satisfied.
    pub fn manual_gate_passed(&self, changed_paths: &[String], manual_approved: bool) -> bool {
        if !self.require_manual_approval_for_sensitive {
            return true;
        }
        if !self.touches_sensitive_paths(changed_paths) {
            return true;
        }
        manual_approved
    }

    /// Returns true when elapsed time is still inside policy budget.
    pub fn time_allowed(&self, elapsed_ms: u128) -> bool {
        elapsed_ms <= u128::from(self.time_budget_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_path_lexically, IterationPolicy};

    fn paths(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn default_has_expected_bounds() {
        let policy = IterationPolicy::default();
        assert_eq!(policy.max_diff_lines, 500);
        assert_eq!(policy.time_budget_ms, 60_000);
        assert!(policy.require_manual_approval_for_sensitive);
        assert!(policy.path_allowlist.contains(&"crates/".to_string()));
    }

    #[test]
    fn paths_allowed_requires_every_path_in_allowlist() {
        let policy = IterationPolicy::default();
        assert!(policy.paths_allowed(&paths(&["crates/a.rs", "docs/b.md", "tests/c.rs"])));
        // Any single out-of-list path fails the whole set.
        assert!(!policy.paths_allowed(&paths(&["crates/a.rs", "config/secret.toml"])));
        // Empty set is vacuously allowed.
        assert!(policy.paths_allowed(&[]));
    }

    #[test]
    fn normalize_path_lexically_resolves_dot_segments() {
        assert_eq!(normalize_path_lexically("crates/x/../y.rs"), "crates/y.rs");
        assert_eq!(normalize_path_lexically("crates//x/./y.rs"), "crates/x/y.rs");
        assert_eq!(normalize_path_lexically("crates/"), "crates");
        assert_eq!(normalize_path_lexically("/crates/../docs//x.md"), "docs/x.md");
        assert_eq!(normalize_path_lexically("."), "");
        assert_eq!(normalize_path_lexically(""), "");
        // ".." with nothing regular to pop is kept, marking an escape.
        assert_eq!(normalize_path_lexically(".."), "..");
        assert_eq!(normalize_path_lexically("../.."), "../..");
        assert_eq!(normalize_path_lexically("crates/../.."), "..");
        assert_eq!(normalize_path_lexically("crates/../../etc/x"), "../etc/x");
    }

    #[test]
    fn paths_allowed_normalizes_parent_traversal() {
        let policy = IterationPolicy::default();
        // ".." resolved within the allowlist is judged by its normalized form.
        assert!(policy.paths_allowed(&paths(&["crates/x/../y.rs"])));
        assert!(policy.paths_allowed(&paths(&["crates/../docs/x.md"])));
        // ".." that resolves the path out of the allowlist is rejected.
        assert!(!policy.paths_allowed(&paths(&["crates/../config/secret.toml"])));
        // ".." that escapes above the root is rejected outright.
        assert!(!policy.paths_allowed(&paths(&["../etc/passwd"])));
        assert!(!policy.paths_allowed(&paths(&["crates/../../etc/passwd"])));
        // Normal paths keep working.
        assert!(policy.paths_allowed(&paths(&["crates/plugin/src/lib.rs"])));
    }

    #[test]
    fn paths_allowed_matches_at_component_boundary() {
        let policy = IterationPolicy {
            path_allowlist: vec!["crates".to_string()],
            ..IterationPolicy::default()
        };
        assert!(policy.paths_allowed(&paths(&["crates/a.rs"])));
        // Shares the string prefix "crates" but not a component boundary.
        assert!(!policy.paths_allowed(&paths(&["crates_evil/a.rs"])));
    }

    #[test]
    fn paths_allowed_root_prefix_matches_everything() {
        let policy = IterationPolicy {
            path_allowlist: vec!["/".to_string()],
            ..IterationPolicy::default()
        };
        assert!(policy.paths_allowed(&paths(&["anything/at/all.rs"])));
    }

    #[test]
    fn touches_sensitive_paths_normalizes_parent_traversal() {
        let policy = IterationPolicy::default();
        // A ".." path that resolves into a sensitive prefix is detected.
        assert!(policy.touches_sensitive_paths(&paths(&[
            "crates/cordis-runtime/src/plugin/../core/models.rs"
        ])));
        // A ".." path that resolves outside every sensitive prefix is not.
        assert!(!policy.touches_sensitive_paths(&paths(&[
            "crates/cordis-runtime/src/plugin/../other/x.rs"
        ])));
        // Escape-above-root paths are never sensitive.
        assert!(!policy.touches_sensitive_paths(&paths(&["../../etc/passwd"])));
        // Paths shorter than the prefix cannot match (component count check).
        assert!(!policy.touches_sensitive_paths(&paths(&["crates/a.rs"])));
    }

    #[test]
    fn diff_allowed_boundary() {
        let policy = IterationPolicy::default();
        assert!(policy.diff_allowed(0));
        assert!(policy.diff_allowed(500));
        assert!(!policy.diff_allowed(501));
    }

    #[test]
    fn touches_sensitive_paths_detects_prefix() {
        let policy = IterationPolicy::default();
        assert!(policy.touches_sensitive_paths(&paths(&[
            "crates/other/a.rs",
            "crates/cordis-runtime/src/kernel/policy.rs",
        ])));
        assert!(!policy.touches_sensitive_paths(&paths(&["crates/other/a.rs", "docs/x.md"])));
        assert!(!policy.touches_sensitive_paths(&[]));
    }

    #[test]
    fn manual_gate_requires_approval_only_for_sensitive() {
        let policy = IterationPolicy::default();
        let sensitive = paths(&["crates/cordis-runtime/src/core/models.rs"]);
        let benign = paths(&["docs/readme.md"]);
        // Non-sensitive paths pass regardless of approval.
        assert!(policy.manual_gate_passed(&benign, false));
        // Sensitive paths need explicit approval.
        assert!(!policy.manual_gate_passed(&sensitive, false));
        assert!(policy.manual_gate_passed(&sensitive, true));
    }

    #[test]
    fn manual_gate_bypassed_when_flag_disabled() {
        let policy = IterationPolicy {
            require_manual_approval_for_sensitive: false,
            ..IterationPolicy::default()
        };
        let sensitive = paths(&["crates/cordis-runtime/src/kernel/x.rs"]);
        assert!(policy.manual_gate_passed(&sensitive, false));
    }

    #[test]
    fn time_allowed_boundary() {
        let policy = IterationPolicy::default();
        assert!(policy.time_allowed(0));
        assert!(policy.time_allowed(60_000));
        assert!(!policy.time_allowed(60_001));
    }
}
