//! Artifact staging lives in the runtime, not here.
//!
//! This script used to copy `target/debug/libsoul_store.so` into
//! `fixtures/artifacts/` and rewrite `artifacts/index.json` (including a
//! hand-written index entry with a stale ABI fingerprint). That job is owned by
//! `cordis_runtime::plugin::tooling::prepare_artifacts` (reached through the
//! `build_plugins` agent tool), which stages every plugin from the workspace
//! build into a content-addressed snapshot and rewrites the index atomically.
//!
//! Keeping a second copy path here duplicated the index rewrite, and because
//! the destination was a hard-coded absolute path (`/root/CordisClaw/...`), a
//! build from any other checkout silently overwrote a *different* checkout's
//! artifacts. Removed in the qq-session hardening batch; only the rerun
//! triggers remain.

fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
}
