# CordisClaw

`CordisClaw` is an experimental Rust runtime for contract-driven plugin trees.

It explores what a plugin system looks like when discovery is explicit, loading is strict, execution is DAG-oriented, and plugin capabilities are described through machine-readable docs instead of loose conventions.

Instead of treating plugins as opaque dynamic modules, `CordisClaw` treats them as a documented tree with explicit parent/child edges, prebuilt artifacts, and fail-fast loading rules.

## The Problem

Many plugin systems become hard to reason about for the same recurring reasons:

- discovery is implicit, so it is unclear why something was loaded
- extension boundaries are loose, so compatibility failures show up late
- service access is ambient, so plugins can depend on more than they declare
- docs drift away from runtime behavior, so humans and tools read stale contracts
- orchestration logic lives outside the plugin model, so execution and loading evolve separately

`CordisClaw` is an attempt to make those failure modes first-class design constraints instead of cleanup work after the fact.

## Design Thesis

Most plugin systems trade clarity for flexibility. `CordisClaw` pushes in the opposite direction:

- plugins are discovered from explicit `package.metadata.cordis.children`
- loading is guarded by ABI fingerprints, artifact indexes, and `sha256` checks
- docs are runtime input through `docs/agent/interfaces.json`
- parent/child service access is controlled through `grants`
- execution semantics are organized around DAG, Gate, Router, Actor, and Kernel primitives
- long-running host mode keeps an atomic runtime snapshot and supports explicit plugin reload
- self-iteration is treated as a guarded runtime workflow, not unrestricted automation

The result is less "drop anything into a folder and hope it works" and more "make contracts visible, make failures explainable."

## Design Tradeoffs

The project makes a few deliberate choices that are useful precisely because they are restrictive:

- explicit discovery over directory scanning
- fail-fast loading over best-effort fallback
- prebuilt artifacts over runtime compilation
- documented contracts over ad hoc runtime introspection
- grants-based access over ambient service visibility
- guarded iteration over unrestricted self-modification

Those tradeoffs reduce convenience in the short term, but they make the runtime easier to inspect, test, and debug.

## Example Runtime Flow

At a high level, the runtime path looks like this:

```text
workspace members
  -> resolve plugin tree from metadata.children
  -> validate docs and contracts
  -> load prebuilt artifacts from index.json
  -> stage artifacts into a snapshot-specific runtime root
  -> register plugins, nodes, and grants-aware context
  -> invoke nodes or export graph views
  -> evaluate / promote / rollback inside the Kernel loop
```

This is the core shape of the system: explicit discovery, strict loading, registered capabilities, then execution and evaluation on top.

## Why It Is Interesting

This repo is most useful if you care about runtime architecture questions such as:

- how to model plugins as a tree instead of a flat extension list
- how strict a loader can be before a plugin system becomes easier to reason about
- how docs can double as machine-readable runtime input
- how parent/child authorization can replace ambient service access
- how DAG, Gate, and Router semantics can sit next to a plugin runtime
- how a rollback-oriented Kernel loop might coexist with explicit safety boundaries

## What It Demonstrates

- Plugin tree discovery and fail-fast contract validation
- Strict artifact loading for `dylib` and JSON-based plugin artifacts
- Parent/child service injection with explicit authorization
- Registry-backed docs queries and graph export
- A deterministic execution prototype built around DAG and Gate semantics
- A minimal self-iteration kernel prototype for evaluate / promote / rollback flows

## What Works Today

The repository already has a working prototype core:

- `resolver`, `loader`, `registry`, and `context` are implemented
- `shell`, `expr`, and `root` fixtures exercise the loading and invocation path
- `RuntimeHost` and `serve` provide a long-running host with atomic snapshot reload
- `execute`, graph export, docs sync, and artifact index refresh are available from the CLI
- `reload` and `kernel` control-plane output are structured JSON with elapsed/failure diagnostics
- Kernel and `auto-update` support verification pipelines, plugin verifiers, and structured JSON/TOML patch kinds as guarded prototype capabilities

Current status and gaps are tracked in [docs/architecture/status-and-open-items.md](./docs/architecture/status-and-open-items.md).

## Current Boundaries

`CordisClaw` is still a prototype, and the repository is explicit about what is not finished:

- execution now has explicit CLI/`serve` entrypoints, but the runtime still treats it as a conservative prototype
- the Kernel is a guarded evaluate / promote / rollback loop, not a full autonomous patching system
- docs and graph export are helper surfaces, not a polished external service boundary
- the original multi-artifact vision is only partially implemented today

## Non-Goals

At its current stage, this repo is not trying to be:

- a generic drop-in plugin marketplace
- a permissive hot-reload system with silent compatibility fallback
- a production-ready orchestration platform
- a fully autonomous code-changing agent

The point is to make the architecture legible before making it maximally flexible.

## Quick Demo

Load the fixture workspace:

```bash
cargo run -p cordis-runtime -- fixtures
```

Invoke the expression plugin:

```bash
cargo run -p cordis-runtime -- invoke expr expr_entry --payload-json='{"expression":"1 + 2 * 3"}'
```

Execute a registered target through the runtime execution engine:

```bash
cargo run -p cordis-runtime -- execute expr::expr_entry --payload-json='{"expression":"1 + 2 * 3"}'
```

Run the long-lived host and reload plugins explicitly:

```bash
cargo run -p cordis-runtime -- serve fixtures
```

Inside `serve`, use:

```text
status
plugins
execute expr::expr_entry {"expression":"1 + 2 * 3"}
reload
kernel status
```

Export the registered plugin graph:

```bash
cargo run -p cordis-runtime -- graph-html fixtures --output=registered-nodes.html
```

Export the registered DAG view:

```bash
cargo run -p cordis-runtime -- dag-html fixtures --output=registered-dag.html
```

Generate plugin artifacts from source, sync docs, and refresh the artifact index:

```bash
cargo run -p cordis-runtime -- rebuild-fixture-artifacts
```

`fixtures/artifacts/` is generated on demand and gitignored. The main CLI/test entrypoints rebuild it automatically when the checked-in plugin sources or docs are newer than the local artifact index.

Configure kernel/runtime/LLM settings with YAML:

```text
config/
  runtime.yaml
  llm_api.yaml
  plugins/*.yaml
```

llm_api.yaml supports both OpenAI Responses API and DeepSeek Chat Completions when provider is set to deepseek.

llm-auto-update verification commands can call plugins directly, for example expr::expr_entry, and `--verify-profile=rust-workspace` enables a default static `cargo check` stage when a workspace manifest is present.

`config/` is intended for local runtime setup and is gitignored. Sample templates are provided in `config.example/`; copy the files you need into `config/`.

Run the test suite:

```bash
cargo test
```

## Repository Shape

```text
.
├── crates/
│   ├── cordis-plugin-sdk/   # shared ABI, docs types, export helpers
│   └── cordis-runtime/      # runtime, plugin, context, execution, kernel, service
├── config.example/          # checked-in config templates
├── config/                  # local runtime/kernel/LLM/plugin YAML config (gitignored)
├── docs/                    # architecture and maintenance docs
├── fixtures/
│   ├── plugins/             # external plugin sample workspace
│   └── artifacts/           # generated local artifacts and index (gitignored)
```

## Read the Architecture

- Project entry: [docs/project-overview.md](./docs/project-overview.md)
- Architecture index: [docs/architecture/README.md](./docs/architecture/README.md)
- System overview: [docs/architecture/system-overview.md](./docs/architecture/system-overview.md)
- Design blueprint: [docs/architecture/design-blueprint.md](./docs/architecture/design-blueprint.md)
- Contracts and loading: [docs/architecture/contracts-and-loading.md](./docs/architecture/contracts-and-loading.md)
- Async workflow API: [docs/architecture/async-workflow-api.md](./docs/architecture/async-workflow-api.md)
- Runtime semantics: [docs/architecture/runtime-semantics.md](./docs/architecture/runtime-semantics.md)
- Plugins and tooling: [docs/architecture/plugins-and-tooling.md](./docs/architecture/plugins-and-tooling.md)
- Completion status: [docs/architecture/status-and-open-items.md](./docs/architecture/status-and-open-items.md)
