//! The agent loop, as a component the kernel does not contain.
//!
//! The kernel defines the seam (`cordis_runtime::agent_loop`) and ships no
//! policy of its own: a binary installs the loop it wants before booting the
//! host. Today this crate wraps the kernel's reference implementation; the
//! implementation moves here next, and the kernel keeps only the trait.

use cordis_runtime::agent_loop::{install_agent_loop, AgentLoop};

mod imp;
pub use imp::*;

/// Install this crate's loop for the process. Call it before booting a host.
pub fn install() -> bool {
    install_agent_loop(Box::new(CargoAgentLoop))
}

/// The loop this crate provides.
pub struct CargoAgentLoop;

impl AgentLoop for CargoAgentLoop {
    fn create(
        &self,
        config: cordis_runtime::config::LlmApiConfig,
        kind: &str,
    ) -> Result<Box<dyn cordis_runtime::agent_loop::AgentSessionApi>, cordis_runtime::core::error::RuntimeError>
    {
        Ok(Box::new(imp::AgentSession::new(config, kind)?))
    }

    fn restore(
        &self,
        snapshot: cordis_runtime::agent_api::AgentSessionSnapshot,
    ) -> Result<Box<dyn cordis_runtime::agent_loop::AgentSessionApi>, cordis_runtime::core::error::RuntimeError>
    {
        Ok(Box::new(imp::AgentSession::from_snapshot(snapshot)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{install, CargoAgentLoop};
    use cordis_runtime::agent_loop::{agent_loop, AgentLoop};
    use cordis_runtime::config::LlmApiConfig;
    use std::sync::Once;

    /// The registry is process-global, so the tests share one install.
    static INIT: Once = Once::new();

    fn ensure_installed() {
        INIT.call_once(|| {
            let _ = install();
        });
    }

    // The crate's whole contract: installing it makes the process use this
    // loop, and that loop can build a session.
    #[test]
    fn installing_makes_the_process_use_this_loop() {
        ensure_installed();
        assert!(agent_loop().is_some());
        let session = CargoAgentLoop
            .create(LlmApiConfig::default(), "runtime_shell")
            .expect("create a session");
        assert_eq!(session.kind(), "runtime_shell");
    }

    // A second install is a no-op, matching `install_plugin_builder`.
    #[test]
    fn a_second_install_is_ignored() {
        ensure_installed();
        assert!(!install());
    }
}

impl cordis_runtime::agent_loop::AgentSessionApi for imp::AgentSession {
    fn kind(&self) -> &str { imp::AgentSession::kind(self) }
    fn status(&self) -> cordis_runtime::agent_api::AgentSessionStatus { imp::AgentSession::status(self) }
    fn history_len(&self) -> usize { imp::AgentSession::history_len(self) }
    fn compact_history(&mut self) -> (usize, usize) { imp::AgentSession::compact_history(self) }
    fn swap_config(&mut self, config: cordis_runtime::config::LlmApiConfig) -> Result<(), cordis_runtime::core::error::RuntimeError> { imp::AgentSession::swap_config(self, config) }
    fn set_soul_key(&mut self, key: String) { imp::AgentSession::set_soul_key(self, key) }
    fn set_require_end_tool(&mut self, require: bool) { imp::AgentSession::set_require_end_tool(self, require) }
    fn set_caller_policy(&mut self, is_group: bool, is_admin: bool) { imp::AgentSession::set_caller_policy(self, is_group, is_admin) }
    fn set_approval_capability(&mut self, can_approve: bool) { imp::AgentSession::set_approval_capability(self, can_approve) }
    fn caller_policy(&self) -> (bool, bool) { imp::AgentSession::caller_policy(self) }
    fn can_approve_iteration(&self) -> bool { imp::AgentSession::can_approve_iteration(self) }
    fn tool_execution_summary(&self) -> cordis_runtime::agent_api::AgentToolExecutionSummary { imp::AgentSession::tool_execution_summary(self) }
    fn transcript(&self) -> Vec<cordis_runtime::agent_api::AgentTranscriptEntry> { imp::AgentSession::transcript(self).to_vec() }
    fn replace_last_action_with_reply(&mut self, reply: &str) -> bool { imp::AgentSession::replace_last_action_with_reply(self, reply) }
    fn inject_exchange(&mut self, user_input: &str, assistant_output: &str) { imp::AgentSession::inject_exchange(self, user_input, assistant_output) }
    fn to_snapshot(&self) -> cordis_runtime::agent_api::AgentSessionSnapshot { imp::AgentSession::to_snapshot(self) }
    fn respond(
        &mut self,
        host: &dyn cordis_runtime::agent_api::AgentToolHost,
        session_id: &str,
        input: &str,
        images: &[serde_json::Value],
        notes: &str,
        provider: &dyn cordis_runtime::agent_api::LlmProvider,
        sink: Option<std::sync::Arc<dyn cordis_runtime::llm_sink::TokenSink>>,
    ) -> Result<cordis_runtime::agent_api::AgentReply, cordis_runtime::core::error::RuntimeError> {
        imp::AgentSession::respond_with_runtime_host_via_images(self, host, session_id, input, images, notes, provider, sink)
    }
    fn respond_with_iteration(
        &mut self,
        host: &cordis_runtime::host::RuntimeHost,
        state: &mut cordis_runtime::host::PluginIterationAgentState,
        input: &str,
        images: &[serde_json::Value],
        notes: &str,
        provider: &dyn cordis_runtime::agent_api::LlmProvider,
        sink: Option<std::sync::Arc<dyn cordis_runtime::llm_sink::TokenSink>>,
    ) -> Result<cordis_runtime::agent_api::AgentReply, cordis_runtime::core::error::RuntimeError> {
        let mut backend = cordis_runtime::host::PluginIterationAgentBackend { host, state };
        imp::AgentSession::respond_with_provider_images(self, &mut backend, input, images, notes, provider, sink)
    }
}
