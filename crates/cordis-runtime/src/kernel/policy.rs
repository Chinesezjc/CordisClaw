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
            path_allowlist: vec!["crates/".to_string(), "docs/".to_string(), "tests/".to_string()],
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
