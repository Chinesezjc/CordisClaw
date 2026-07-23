//! Iteration policy used by kernel self-iteration loop.
//! This is the safety boundary for "what can be changed automatically".

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
    pub fn paths_allowed(&self, changed_paths: &[String]) -> bool {
        changed_paths.iter().all(|path| {
            self.path_allowlist
                .iter()
                .any(|prefix| path.starts_with(prefix))
        })
    }

    /// Returns true when changed size is within the configured budget.
    pub fn diff_allowed(&self, diff_lines: usize) -> bool {
        diff_lines <= self.max_diff_lines
    }

    /// Returns true when the changed set touches at least one sensitive path prefix.
    pub fn touches_sensitive_paths(&self, changed_paths: &[String]) -> bool {
        changed_paths.iter().any(|path| {
            self.sensitive_path_prefixes
                .iter()
                .any(|prefix| path.starts_with(prefix))
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
    use super::IterationPolicy;

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
