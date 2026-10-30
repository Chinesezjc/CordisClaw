//! The agent loop, as a component the kernel does not contain.
//!
//! The kernel defines the seam (`cordis_runtime::agent_loop`) and ships no
//! policy of its own: a binary installs the loop it wants before booting the
//! host. Today this crate wraps the kernel's reference implementation; the
//! implementation moves here next, and the kernel keeps only the trait.

use cordis_runtime::agent_loop::{install_agent_loop, AgentLoop};

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
        cordis_runtime::agent_loop::KernelAgentLoop.create(config, kind)
    }

    fn restore(
        &self,
        snapshot: cordis_runtime::agent::AgentSessionSnapshot,
    ) -> Result<Box<dyn cordis_runtime::agent_loop::AgentSessionApi>, cordis_runtime::core::error::RuntimeError>
    {
        cordis_runtime::agent_loop::KernelAgentLoop.restore(snapshot)
    }
}
