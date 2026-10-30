//! The agent-loop seam.
//!
//! The kernel owns the *session slot* — the map from session id to a running
//! conversation — but not the loop that fills it. A loop is installed at
//! startup the same way the plugin builder is installed: the kernel ships a
//! default implementation, and a separate crate may replace it.
//!
//! Why a seam at all: the loop decides prompt content, when a turn ends and how
//! failures retry. Those are policy, and today they live in the one file the
//! coverage gate excludes. Behind a trait they can move to their own crate,
//! which keeps the kernel to scheduling + loading and lets sessions become
//! interchangeable objects — the prerequisite for running several of them and
//! letting them talk to each other.
//!
//! Dependency direction stays acyclic: `cordis-agent` depends on
//! `cordis-runtime` for these traits, never the other way round.

use std::sync::{Arc, OnceLock};

use serde_json::Value;

use crate::agent::{AgentSession, AgentSessionSnapshot, LlmProvider};
use crate::agent_api::{
    AgentReply, AgentSessionStatus, AgentToolExecutionSummary, AgentToolHost, AgentTranscriptEntry,
};
use crate::config::LlmApiConfig;
use crate::core::error::RuntimeError;
use crate::llm_sink::TokenSink;

/// One running conversation, as the kernel sees it.
///
/// Narrow on purpose: everything the host does to a session, and nothing about
/// how a turn is produced. The loop's internals (history, protocol, prompt
/// assembly) stay behind this line.
pub trait AgentSessionApi: Send {
    fn kind(&self) -> &str;
    fn status(&self) -> AgentSessionStatus;
    fn history_len(&self) -> usize;
    fn compact_history(&mut self) -> (usize, usize);
    fn swap_config(&mut self, config: LlmApiConfig) -> Result<(), RuntimeError>;
    fn set_soul_key(&mut self, key: String);
    fn set_require_end_tool(&mut self, require: bool);
    fn set_caller_policy(&mut self, is_group: bool, is_admin: bool);
    fn set_approval_capability(&mut self, can_approve: bool);
    fn caller_policy(&self) -> (bool, bool);
    fn can_approve_iteration(&self) -> bool;
    fn tool_execution_summary(&self) -> AgentToolExecutionSummary;
    fn transcript(&self) -> Vec<AgentTranscriptEntry>;
    fn replace_last_action_with_reply(&mut self, reply: &str) -> bool;
    fn to_snapshot(&self) -> AgentSessionSnapshot;
    /// Run one turn against `host`, using `provider` for completions.
    // Eight parameters is the honest shape of a turn: host, identity, input,
    // images, one-shot context, transport, sink.
    #[expect(
        clippy::too_many_arguments,
        reason = "a turn genuinely takes this many inputs; grouping them into a struct would only hide them"
    )]
    fn respond(
        &mut self,
        host: &dyn AgentToolHost,
        session_id: &str,
        input: &str,
        images: &[Value],
        notes: &str,
        provider: &dyn LlmProvider,
        sink: Option<Arc<dyn TokenSink>>,
    ) -> Result<AgentReply, RuntimeError>;
}

/// Constructs conversations. An embedding crate replaces this wholesale.
pub trait AgentLoop: Send + Sync {
    fn create(
        &self,
        config: LlmApiConfig,
        kind: &str,
    ) -> Result<Box<dyn AgentSessionApi>, RuntimeError>;
    fn restore(
        &self,
        snapshot: AgentSessionSnapshot,
    ) -> Result<Box<dyn AgentSessionApi>, RuntimeError>;
}

static LOOP: OnceLock<Box<dyn AgentLoop>> = OnceLock::new();

/// Install the process-wide agent loop. Idempotent: a second install returns
/// `false` and leaves the first one in place, mirroring `install_plugin_builder`.
pub fn install_agent_loop(agent_loop: Box<dyn AgentLoop>) -> bool {
    LOOP.set(agent_loop).is_ok()
}

/// The installed loop, or `None` before one is installed.
pub fn agent_loop() -> Option<&'static dyn AgentLoop> {
    LOOP.get().map(|installed| installed.as_ref())
}

/// The uniform error for a path that needs a loop but has none installed.
pub fn missing_loop_error() -> RuntimeError {
    RuntimeError::Invariant {
        message: "no agent loop installed (this build has no agent loop)".to_string(),
    }
}

/// The kernel's own loop: today's `AgentSession`, unchanged.
pub struct KernelAgentLoop;

impl AgentLoop for KernelAgentLoop {
    fn create(
        &self,
        config: LlmApiConfig,
        kind: &str,
    ) -> Result<Box<dyn AgentSessionApi>, RuntimeError> {
        Ok(Box::new(AgentSession::new(config, kind)?))
    }

    fn restore(
        &self,
        snapshot: AgentSessionSnapshot,
    ) -> Result<Box<dyn AgentSessionApi>, RuntimeError> {
        Ok(Box::new(AgentSession::from_snapshot(snapshot)?))
    }
}

impl AgentSessionApi for AgentSession {
    fn kind(&self) -> &str {
        AgentSession::kind(self)
    }

    fn status(&self) -> AgentSessionStatus {
        AgentSession::status(self)
    }

    fn history_len(&self) -> usize {
        AgentSession::history_len(self)
    }

    fn compact_history(&mut self) -> (usize, usize) {
        AgentSession::compact_history(self)
    }

    fn swap_config(&mut self, config: LlmApiConfig) -> Result<(), RuntimeError> {
        AgentSession::swap_config(self, config)
    }

    fn set_soul_key(&mut self, key: String) {
        AgentSession::set_soul_key(self, key);
    }

    fn set_require_end_tool(&mut self, require: bool) {
        AgentSession::set_require_end_tool(self, require);
    }

    fn set_caller_policy(&mut self, is_group: bool, is_admin: bool) {
        AgentSession::set_caller_policy(self, is_group, is_admin);
    }

    fn set_approval_capability(&mut self, can_approve: bool) {
        AgentSession::set_approval_capability(self, can_approve);
    }

    fn caller_policy(&self) -> (bool, bool) {
        AgentSession::caller_policy(self)
    }

    fn can_approve_iteration(&self) -> bool {
        AgentSession::can_approve_iteration(self)
    }

    fn tool_execution_summary(&self) -> AgentToolExecutionSummary {
        AgentSession::tool_execution_summary(self)
    }

    fn transcript(&self) -> Vec<AgentTranscriptEntry> {
        AgentSession::transcript(self).to_vec()
    }

    fn replace_last_action_with_reply(&mut self, reply: &str) -> bool {
        AgentSession::replace_last_action_with_reply(self, reply)
    }

    fn to_snapshot(&self) -> AgentSessionSnapshot {
        AgentSession::to_snapshot(self)
    }

    fn respond(
        &mut self,
        host: &dyn AgentToolHost,
        session_id: &str,
        input: &str,
        images: &[Value],
        notes: &str,
        provider: &dyn LlmProvider,
        sink: Option<Arc<dyn TokenSink>>,
    ) -> Result<AgentReply, RuntimeError> {
        AgentSession::respond_with_runtime_host_via_images(
            self, host, session_id, input, images, notes, provider, sink,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{agent_loop, install_agent_loop, AgentLoop, KernelAgentLoop};
    use crate::config::LlmApiConfig;

    // The seam must build a session the same way the kernel always did; this is
    // the contract `cordis-agent` will replace with its own implementation.
    #[test]
    fn the_default_loop_creates_a_session() {
        let session = KernelAgentLoop
            .create(LlmApiConfig::default(), "runtime_shell")
            .expect("default loop creates a session");
        assert_eq!(session.kind(), "runtime_shell");
        assert_eq!(session.history_len(), 0);
    }

    // Installing is idempotent and the first loop wins, like the plugin builder.
    #[test]
    fn installing_twice_keeps_the_first_loop() {
        assert!(install_agent_loop(Box::new(KernelAgentLoop)));
        assert!(!install_agent_loop(Box::new(KernelAgentLoop)));
        assert!(agent_loop().is_some());
    }
}
