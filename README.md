# CordisClaw

`CordisClaw` is an experimental Rust runtime for contract-driven plugin trees.

It explores what a plugin system looks like when discovery is explicit, loading is strict, execution is CPN-net-oriented, and plugin capabilities are described through machine-readable docs instead of loose conventions.

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
- execution semantics are organized around CPN Net, Router, Actor, and Kernel primitives
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

## What Works Today

- **Plugin tree discovery** — resolver walks `package.metadata.cordis.children`, validates paths, crate names, docs contracts, and cycle detection at resolve time
- **Strict artifact loading** — ABI fingerprint matching, `sha256` artifact index, fail-fast on mismatch
- **Parent/child service injection** — `grants`-based authorization, typed service container with `provide`/`inject`, and `Service` trait with `start`/`stop` lifecycle for background Task nodes
- **Docs as runtime input** — `docs/agent/interfaces.json` feeds `DocRegistry`, `GraphRegistry`, node registration, and CLI graph export
- **CPN Net execution engine** — deterministic scheduler, Router with overlay transactions, Gate policies (AllOf/AnyOf/FirstSuccess/AtLeast), retry/backoff/timeout
- **Long-running host** — `serve` REPL with atomic snapshot reload, candidate staging, promote/rollback, structured JSON diagnostics
- **Interactive agent chat** — streaming LLM conversation, 15 tools (read/write/search/shell), readline editing with history, Ctrl+C draft safety, direct `/plugin` shortcuts
- **Shell console mode** — type commands directly, routed through Shell plugin's catalog-based command dispatch (NoneBot console pattern)
- **Self-iteration** — open-ended agent loop replaces the fixed 9-stage Petri Net pipeline; agent can read code, write files, run builds, and verify changes autonomously
- **Rollback safety net** — panic guard, incremental journal persistence, draft patches on error, workspace auto-restore
- **Plugin samples** — `expr` (recursive-descent arithmetic with lexer/parser/evaluator + add/sub/mul/div/modulo/pow operators), `shell` (command catalog dispatch), `qq` (OneBot v11 adapter), `root` (scaffold placeholder)

## Current Boundaries

`CordisClaw` is still a prototype, and the repository is explicit about what is not finished:

- the agent loop is functional but tool surface and safety boundaries are still evolving
- the `Service` trait exists for Task nodes but auto-start on plugin load is not yet wired
- `cdylib` / `WASM` / `external process` plugin forms are designed but not implemented
- docs and graph export are helper surfaces, not a polished external service boundary
- TODO: full canary release (traffic splitting, auto-promotion) [see status doc]

## Non-Goals

At its current stage, this repo is not trying to be:

- a generic drop-in plugin marketplace
- a permissive hot-reload system with silent compatibility fallback
- a production-ready orchestration platform
- a fully autonomous code-changing agent

The point is to make the architecture legible before making it maximally flexible.

## Quick Demo

Load and interact with the runtime:

```bash
cargo run -p cordis-runtime -- serve fixtures
```

Inside `serve`, three modes are available:

```text
> agent              # enter AI agent chat (>> prompt)
> shell              # enter shell console, type commands directly ($ prompt)
> /exit              # return from agent/shell to command mode

> status             # runtime status
> plugins            # list loaded plugins
> reload             # reload plugin workspace
> kernel status      # kernel metrics and issues
> kernel issues      # list observed plugin issues
> exit               # quit serve
```

Agent chat mode (`>>`):

```text
>> 列出当前加载的插件
>> 为 expr 实现幂运算
>> /expr::expr_entry 2^10          # direct plugin call, bypasses LLM
>> /reset                          # reset agent session
```

Shell console mode (`$`):

```text
$ Expr 1+2*3
$ Qq configure url=http://127.0.0.1:5700 target=group:123456
$ help
```

One-shot CLI invocation:

```bash
cargo run -p cordis-runtime -- execute expr::expr_entry --payload-json='{"expression":"1 + 2 * 3"}'
```

## Repository Shape

```text
.
├── crates/
│   ├── cordis-plugin-sdk/   # shared ABI, docs types, NodeType, export helpers
│   └── cordis-runtime/      # runtime, plugin, context, execution, kernel, service, agent
├── config.example/          # checked-in config templates
├── config/                  # local runtime/kernel/LLM/plugin YAML config (gitignored)
├── docs/                    # architecture and maintenance docs
├── fixtures/
│   ├── plugins/             # external plugin sample workspace
│   │   ├── expr/            # recursive-descent arithmetic (lexer/parser/evaluator + 6 operators)
│   │   ├── shell/           # command-catalog dispatch (NoneBot console pattern)
│   │   ├── qq/              # OneBot v11 QQ adapter
│   │   └── root/            # scaffold placeholder
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
- File responsibility map: [docs/rs-files-responsibility.md](./docs/rs-files-responsibility.md)
