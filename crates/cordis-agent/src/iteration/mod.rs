//! The plugin-iteration pipeline: policy, driver and the agent backend.

pub mod backend;
pub mod driver;
pub mod policy;

use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::host::{KernelPluginIterationResult, RuntimeHost};
use cordis_runtime::iteration_api::IterationRunner;
use cordis_runtime::kernel::plugin_iteration::KernelPluginIterationRequest;

/// The crate's runner: the kernel keeps the seam, the pipeline lives here.
pub struct CargoIterationRunner;

impl IterationRunner for CargoIterationRunner {
    fn iterate(
        &self,
        host: &RuntimeHost,
        request: KernelPluginIterationRequest,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        driver::iterate_plugins(host, request)
    }

    fn approve(
        &self,
        host: &RuntimeHost,
        iteration_id: &str,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        host.approve_blocked_iteration(iteration_id)
    }
}
