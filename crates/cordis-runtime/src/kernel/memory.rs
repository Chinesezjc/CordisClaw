//! Persistent-like in-memory change history used by iteration loop.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ChangeVerdict {
    Promote,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChangeRecord {
    pub issue_id: String,
    pub patch_id: String,
    pub patch_kind: String,
    pub verification_profile: Option<String>,
    pub verdict: ChangeVerdict,
    pub quality_score: u32,
    pub reasons: Vec<String>,
    pub observed_at_ms: u128,
}

#[derive(Debug, Clone)]
pub struct ChangeMemory {
    records: VecDeque<ChangeRecord>,
    limit: usize,
}

impl Default for ChangeMemory {
    fn default() -> Self {
        Self {
            records: VecDeque::new(),
            limit: 1_024,
        }
    }
}

impl ChangeMemory {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            records: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    /// Append one iteration result; oldest item is evicted when over limit.
    // 记录一次修复事件的全部维度；拆参数结构体会让唯一调用点更啰嗦，收益为负。
    #[expect(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        issue_id: impl Into<String>,
        patch_id: impl Into<String>,
        patch_kind: impl Into<String>,
        verification_profile: Option<String>,
        verdict: ChangeVerdict,
        quality_score: u32,
        reasons: Vec<String>,
    ) {
        let observed_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        self.records.push_back(ChangeRecord {
            issue_id: issue_id.into(),
            patch_id: patch_id.into(),
            patch_kind: patch_kind.into(),
            verification_profile,
            verdict,
            quality_score,
            reasons,
            observed_at_ms,
        });
        while self.records.len() > self.limit {
            self.records.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn recent(&self, limit: usize) -> Vec<ChangeRecord> {
        self.records
            .iter()
            .rev()
            .take(limit)
            .cloned()
            .collect::<Vec<_>>()
    }
}

#[cfg(test)]
mod tests {
    use super::{ChangeMemory, ChangeVerdict};

    fn push(memory: &mut ChangeMemory, issue: &str, verdict: ChangeVerdict) {
        memory.record(
            issue,
            format!("{issue}-patch"),
            "replace_exact",
            Some("rust_workspace".to_string()),
            verdict,
            88,
            vec!["ok".to_string()],
        );
    }

    #[test]
    fn default_starts_empty() {
        let memory = ChangeMemory::default();
        assert_eq!(memory.len(), 0);
        assert!(memory.is_empty());
        assert!(memory.recent(10).is_empty());
    }

    #[test]
    fn record_stores_all_dimensions() {
        let mut memory = ChangeMemory::default();
        memory.record(
            "issue-1",
            "patch-1",
            "create_file",
            None,
            ChangeVerdict::Promote,
            95,
            vec!["tests_passed".to_string(), "safety_ok".to_string()],
        );
        assert_eq!(memory.len(), 1);
        assert!(!memory.is_empty());
        let recent = memory.recent(1);
        let record = &recent[0];
        assert_eq!(record.issue_id, "issue-1");
        assert_eq!(record.patch_id, "patch-1");
        assert_eq!(record.patch_kind, "create_file");
        assert_eq!(record.verification_profile, None);
        assert_eq!(record.verdict, ChangeVerdict::Promote);
        assert_eq!(record.quality_score, 95);
        assert_eq!(record.reasons, vec!["tests_passed", "safety_ok"]);
        // observed_at_ms is populated from the system clock (non-zero on any
        // real host past the epoch).
        assert!(record.observed_at_ms > 0);
    }

    #[test]
    fn with_limit_floors_at_one() {
        // limit.max(1): a requested limit of 0 must still keep at least one.
        let mut memory = ChangeMemory::with_limit(0);
        push(&mut memory, "a", ChangeVerdict::Promote);
        push(&mut memory, "b", ChangeVerdict::Rollback);
        assert_eq!(memory.len(), 1);
        // Only the newest survives eviction.
        assert_eq!(memory.recent(1)[0].issue_id, "b");
    }

    #[test]
    fn record_evicts_oldest_when_over_limit() {
        let mut memory = ChangeMemory::with_limit(2);
        push(&mut memory, "first", ChangeVerdict::Promote);
        push(&mut memory, "second", ChangeVerdict::Promote);
        push(&mut memory, "third", ChangeVerdict::Rollback);
        assert_eq!(memory.len(), 2);
        let recent = memory.recent(5);
        // recent() is newest-first; "first" was evicted.
        assert_eq!(recent[0].issue_id, "third");
        assert_eq!(recent[1].issue_id, "second");
    }

    #[test]
    fn recent_limits_and_reverses_order() {
        let mut memory = ChangeMemory::with_limit(10);
        for name in ["x", "y", "z"] {
            push(&mut memory, name, ChangeVerdict::Promote);
        }
        let recent = memory.recent(2);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].issue_id, "z");
        assert_eq!(recent[1].issue_id, "y");
    }

    #[test]
    fn verdict_serde_round_trips() {
        for verdict in [ChangeVerdict::Promote, ChangeVerdict::Rollback] {
            let json = serde_json::to_string(&verdict).unwrap();
            let back: ChangeVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(verdict, back);
        }
    }
}
