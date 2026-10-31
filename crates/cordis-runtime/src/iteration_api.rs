//! The plugin-iteration seam.
//!
//! Running an iteration — staging a candidate, verifying it, replaying a canary,
//! then promoting or rolling back — is policy, and it is about to live in
//! `cordis-agent` next to the loop that drives it. The kernel keeps the tool
//! entry point and this trait, exactly like `agent_loop`: the kernel ships a
//! default implementation, an embedding crate may replace it.
//!
//! Dependency direction stays acyclic: the crate depends on these traits, never
//! the other way round.

use std::sync::OnceLock;

use crate::core::error::RuntimeError;
use crate::host::{KernelPluginIterationResult, RuntimeHost};
use crate::kernel::plugin_iteration::KernelPluginIterationRequest;

/// One iteration runner, as the kernel sees it.
pub trait IterationRunner: Send + Sync {
    /// Run one plugin iteration to its final verdict.
    fn iterate(
        &self,
        host: &RuntimeHost,
        request: KernelPluginIterationRequest,
    ) -> Result<KernelPluginIterationResult, RuntimeError>;

    /// Promote a previously blocked iteration.
    fn approve(
        &self,
        host: &RuntimeHost,
        iteration_id: &str,
    ) -> Result<KernelPluginIterationResult, RuntimeError>;
}

static RUNNER: OnceLock<Box<dyn IterationRunner>> = OnceLock::new();

/// Install the process-wide iteration runner. Idempotent: a second install
/// returns `false` and leaves the first one in place.
pub fn install_iteration_runner(runner: Box<dyn IterationRunner>) -> bool {
    RUNNER.set(runner).is_ok()
}

/// The installed runner, or `None` before one is installed.
pub fn iteration_runner() -> Option<&'static dyn IterationRunner> {
    RUNNER.get().map(|installed| installed.as_ref())
}

/// The uniform error for a path that needs a runner but has none installed.
pub fn missing_runner_error() -> RuntimeError {
    RuntimeError::Invariant {
        message: "no plugin iteration runner installed (this build has no iteration runner)"
            .to_string(),
    }
}

/// The kernel's own runner: today's `RuntimeHost::iterate_plugins`.
pub struct KernelIterationRunner;

impl IterationRunner for KernelIterationRunner {
    fn iterate(
        &self,
        host: &RuntimeHost,
        request: KernelPluginIterationRequest,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        host.iterate_plugins(request)
    }

    fn approve(
        &self,
        host: &RuntimeHost,
        iteration_id: &str,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        host.approve_blocked_iteration(iteration_id)
    }
}
