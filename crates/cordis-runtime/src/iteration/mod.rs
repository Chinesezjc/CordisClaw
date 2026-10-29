//! Plugin-iteration policy.
//!
//! The pipeline used to answer "may this change land?" from three places: the
//! `record_iteration_summary` tool checked for test edits, `finalize` consulted
//! a `requires_approval` flag, and the approval gate lived in the agent tool
//! layer. This module is the single place that turns an iteration's facts into
//! a verdict, so a policy change is one edit here instead of six spread across
//! `host.rs`.

pub(crate) mod policy;
pub(crate) mod report;
