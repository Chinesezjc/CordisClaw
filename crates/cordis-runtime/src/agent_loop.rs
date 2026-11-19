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

use crate::agent_api::{
    AgentReply, AgentSessionStatus, AgentToolExecutionSummary, AgentToolHost, AgentTranscriptEntry,
};
use crate::agent_api::{AgentSessionSnapshot, LlmProvider};
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
    /// Compact the stored history when it outgrows its token budget.
    ///
    /// `provider` is the same `llm_complete` seam a turn uses; the loop asks it
    /// for one real summary and replaces the oldest messages with that memory.
    /// `None` (no provider plugin) degrades to the truncation fallback, which
    /// still never empties the history. Returns `(before_len, after_len)`.
    fn compact_history(&mut self, provider: Option<&dyn LlmProvider>) -> (usize, usize);
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
    fn inject_exchange(&mut self, user_input: &str, assistant_output: &str);
    /// Append one **user** message without producing an assistant reply.
    ///
    /// A read-only channel uses this to keep collecting the conversation it
    /// may speak about once the policy allows it: the text becomes history
    /// (and therefore visible to the next real turn) but no model call is
    /// made, so nothing is billed and nothing is said.
    fn ingest_user_message(&mut self, user_input: &str);
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
    /// Run one turn for a plugin-iteration conversation, whose backend carries
    /// state the host owns.
    #[expect(
        clippy::too_many_arguments,
        reason = "same shape as `respond`, plus the iteration state"
    )]
    fn respond_with_iteration(
        &mut self,
        host: &crate::host::RuntimeHost,
        session_id: &str,
        state: &mut crate::host::PluginIterationAgentState,
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

#[cfg(test)]
pub mod test_loop {
    use super::{AgentLoop, AgentSessionApi};
    use crate::agent_api::{
        AgentReply, AgentSessionSnapshot, AgentSessionStatus, AgentToolExecutionSummary,
        AgentToolHost, AgentTranscriptEntry, LlmProvider,
    };
    use crate::config::LlmApiConfig;
    use crate::core::error::RuntimeError;
    use serde_json::Value;
    use std::sync::Arc;

    #[derive(Default)]
    pub struct TestSession {
        kind: String,
        history_len: usize,
        can_approve: bool,
        policy: (bool, bool),
        config: LlmApiConfig,
    }

    impl AgentSessionApi for TestSession {
        fn kind(&self) -> &str {
            &self.kind
        }
        fn status(&self) -> AgentSessionStatus {
            AgentSessionStatus {
                kind: self.kind.clone(),
                provider: self.config.provider.clone(),
                model: self.config.model.clone(),
                completed_turns: 0,
                stored_messages: self.history_len,
                transcript_events: 0,
            }
        }
        fn history_len(&self) -> usize {
            self.history_len
        }
        fn compact_history(&mut self, _provider: Option<&dyn LlmProvider>) -> (usize, usize) {
            (self.history_len, self.history_len)
        }
        fn swap_config(&mut self, config: LlmApiConfig) -> Result<(), RuntimeError> {
            self.config = config;
            Ok(())
        }
        fn set_soul_key(&mut self, _key: String) {}
        fn set_require_end_tool(&mut self, _require: bool) {}
        fn set_caller_policy(&mut self, is_group: bool, is_admin: bool) {
            self.policy = (is_group, is_admin);
        }
        fn set_approval_capability(&mut self, can_approve: bool) {
            self.can_approve = can_approve;
        }
        fn caller_policy(&self) -> (bool, bool) {
            self.policy
        }
        fn can_approve_iteration(&self) -> bool {
            self.can_approve
        }
        fn tool_execution_summary(&self) -> AgentToolExecutionSummary {
            AgentToolExecutionSummary {
                total_calls: 0,
                successful_calls: 0,
                failed_calls: 0,
                tool_names: Vec::new(),
            }
        }
        fn transcript(&self) -> Vec<AgentTranscriptEntry> {
            Vec::new()
        }
        fn replace_last_action_with_reply(&mut self, _reply: &str) -> bool {
            false
        }
        fn inject_exchange(&mut self, _user_input: &str, _assistant_output: &str) {
            // Mirrors the real session: the injected exchange is one user entry
            // plus one assistant entry, so the counter moves by two.
            self.history_len += 2;
        }
        fn ingest_user_message(&mut self, _user_input: &str) {
            self.history_len += 1;
        }
        fn to_snapshot(&self) -> AgentSessionSnapshot {
            AgentSessionSnapshot {
                kind: self.kind.clone(),
                config: LlmApiConfig::default(),
                history: Vec::new(),
                transcript: Vec::new(),
                completed_turns: 0,
                estimated_tokens: 0,
                require_end_tool: false,
                soul_key: String::new(),
            }
        }
        fn respond(
            &mut self,
            _host: &dyn AgentToolHost,
            _session_id: &str,
            _input: &str,
            _images: &[Value],
            _notes: &str,
            _provider: &dyn crate::agent_api::LlmProvider,
            _sink: Option<Arc<dyn crate::llm_sink::TokenSink>>,
        ) -> Result<AgentReply, RuntimeError> {
            Ok(AgentReply {
                response_id: None,
                content: String::new(),
                tool_events: Vec::new(),
            })
        }
        fn respond_with_iteration(
            &mut self,
            _host: &crate::host::RuntimeHost,
            _session_id: &str,
            _state: &mut crate::host::PluginIterationAgentState,
            _input: &str,
            _images: &[Value],
            _notes: &str,
            _provider: &dyn crate::agent_api::LlmProvider,
            _sink: Option<Arc<dyn crate::llm_sink::TokenSink>>,
        ) -> Result<AgentReply, RuntimeError> {
            Ok(AgentReply {
                response_id: None,
                content: String::new(),
                tool_events: Vec::new(),
            })
        }
    }

    pub struct TestLoop;

    impl AgentLoop for TestLoop {
        fn create(
            &self,
            _config: LlmApiConfig,
            kind: &str,
        ) -> Result<Box<dyn AgentSessionApi>, RuntimeError> {
            Ok(Box::new(TestSession {
                kind: kind.to_string(),
                history_len: 0,
                can_approve: false,
                policy: (false, false),
                config: LlmApiConfig::default(),
            }))
        }

        fn restore(
            &self,
            snapshot: AgentSessionSnapshot,
        ) -> Result<Box<dyn AgentSessionApi>, RuntimeError> {
            Ok(Box::new(TestSession {
                kind: snapshot.kind,
                history_len: snapshot.history.len(),
                can_approve: false,
                policy: (false, false),
                config: snapshot.config,
            }))
        }
    }
}

#[cfg(test)]
mod seam_tests {
    use super::test_loop::TestLoop;
    use super::{missing_loop_error, AgentLoop, LlmProvider};
    use crate::agent_api::LlmCompletionParts;
    use crate::config::LlmApiConfig;
    use crate::core::error::RuntimeError;
    use crate::host::{
        PluginIterationAgentState, PluginIterationContextPaths, PreparedPluginIteration,
        RuntimeHost,
    };
    use crate::llm_sink::TokenSink;
    use serde_json::Value;
    use serial_test::serial;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::Arc;

    /// The loop's double ignores the provider, so a provider that always fails
    /// proves the turn never reaches the transport.
    struct UnusedProvider;

    impl LlmProvider for UnusedProvider {
        fn complete(
            &self,
            _body: Value,
            _sink: Option<Arc<dyn TokenSink>>,
            _transport: cordis_plugin_sdk::llm::LlmTransportConfig,
        ) -> Result<LlmCompletionParts, RuntimeError> {
            Err(RuntimeError::NoLlmProvider)
        }
    }

    /// A host that boots on an empty artifact index: the double only needs a
    /// `&RuntimeHost`, it never calls into it.
    fn bare_host(temp: &Path) -> RuntimeHost {
        let fixtures = temp.join("fixtures");
        std::fs::create_dir_all(fixtures.join("artifacts")).unwrap();
        let index = serde_json::json!({
            "schema_version": 2,
            "generated_at": "2026-07-25T00:00:00Z",
            "topo_order": [],
            "entries": [],
        });
        std::fs::write(
            fixtures.join("artifacts/index.json"),
            serde_json::to_string(&index).unwrap(),
        )
        .unwrap();
        RuntimeHost::boot(&fixtures).expect("boot on an empty index")
    }

    fn iteration_state() -> PluginIterationAgentState {
        let prepared = PreparedPluginIteration {
            iteration_id: "iter-agent-loop".to_string(),
            issue_id: "issue-agent-loop".to_string(),
            root_plugin_path: "mini".to_string(),
            target_plugin_paths: vec!["mini".to_string()],
            source: None,
            summary: "agent loop seam".to_string(),
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: crate::kernel::verifier::VerificationProfile::RustWorkspace,
            quality_score: None,
            edit_plan: None,
            instruction: None,
            allowed_plugin_roots: BTreeMap::from([(
                "mini".to_string(),
                "plugins/mini".to_string(),
            )]),
        };
        PluginIterationAgentState::new(
            prepared,
            PluginIterationContextPaths {
                focus_paths: Vec::new(),
                all_paths: Vec::new(),
            },
            Path::new("."),
        )
    }

    /// The uniform error is the whole answer when no loop is installed. It is
    /// asserted byte for byte because callers match on the text.
    #[test]
    fn a_missing_loop_reports_its_own_text() {
        assert_eq!(
            missing_loop_error().to_string(),
            "internal invariant broken: no agent loop installed (this build has no agent loop)"
        );
    }

    /// Every method of the test double, including the plugin-iteration turn
    /// shape that only a provider-backed iteration session reaches.
    #[test]
    #[serial]
    fn the_test_loop_session_answers_both_turn_shapes() {
        let temp = tempfile::tempdir().unwrap();
        let host = bare_host(temp.path());
        let mut session = TestLoop
            .create(LlmApiConfig::default(), "plugin_iteration")
            .expect("create a session");
        let mut state = iteration_state();
        let provider = UnusedProvider;

        // The double never reaches the transport. Asking the stub directly is
        // what pins its failure shape, so a later double that did call it
        // could not pass by accident.
        assert!(matches!(
            provider.complete(Value::Null, None, Default::default()),
            Err(RuntimeError::NoLlmProvider)
        ));
        let reply = session
            .respond(&host, "session", "hi", &[], "", &provider, None)
            .expect("a shell turn");
        assert_eq!(reply.content, "");
        let reply = session
            .respond_with_iteration(&host, "session", &mut state, "hi", &[], "", &provider, None)
            .expect("an iteration turn");
        assert_eq!(reply.content, "");
        assert!(reply.tool_events.is_empty());

        assert_eq!(session.kind(), "plugin_iteration");
        assert_eq!(session.tool_execution_summary().total_calls, 0);
        assert!(session.transcript().is_empty());
        assert!(!session.replace_last_action_with_reply("a reply"));

        // The injected exchange is two stored messages — the counter is the
        // observable the stub keeps. Pinning it here is what stops
        // `inject_exchange` from silently degrading to a no-op that drops a
        // shortcut's context.
        let before = session.history_len();
        session.inject_exchange("q", "a");
        assert_eq!(session.history_len(), before + 2);
    }
}
