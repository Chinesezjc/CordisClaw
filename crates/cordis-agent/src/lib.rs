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
        snapshot: cordis_runtime::agent_api::AgentSessionSnapshot,
    ) -> Result<Box<dyn cordis_runtime::agent_loop::AgentSessionApi>, cordis_runtime::core::error::RuntimeError>
    {
        cordis_runtime::agent_loop::KernelAgentLoop.restore(snapshot)
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
