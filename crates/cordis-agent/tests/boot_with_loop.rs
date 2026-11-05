//! 证明「测试跟着代码走」可行：在 crate 自己的测试里装 loop、启 host、开会话。
//!
//! runtime 的测试拿不到 loop（dev-dependency 环会编出两份 runtime），
//! 所以需要真实循环的用例只能住在 cordis-agent 这边。

use std::path::Path;

use cordis_agent::{install, CargoAgentLoop};
use cordis_runtime::agent_loop::{agent_loop, install_agent_loop};
use cordis_runtime::host::{AgentSessionKind, AgentStartOptions, RuntimeHost};

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

#[test]
fn a_booted_host_uses_the_installed_loop() {
    let _ = install_agent_loop(Box::new(CargoAgentLoop));
    let host = RuntimeHost::boot(fixtures()).expect("boot the host");
    assert!(agent_loop().is_some(), "the loop must be installed");

    let session = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("start a session through the loop");
    assert!(!session.session_id.is_empty());
    assert!(host.agent_status(session.session_id.as_str()).is_ok());
    let _ = install;
}
