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
    pub verdict: ChangeVerdict,
    pub quality_score: u32,
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
    pub fn record(
        &mut self,
        issue_id: impl Into<String>,
        patch_id: impl Into<String>,
        verdict: ChangeVerdict,
        quality_score: u32,
    ) {
        let observed_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        self.records.push_back(ChangeRecord {
            issue_id: issue_id.into(),
            patch_id: patch_id.into(),
            verdict,
            quality_score,
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
