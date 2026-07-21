# Soul Store Plugin

SQLite-backed storage for per-user personas ("souls").

## What it does

The kernel keeps a file-based default soul store (`data/souls/*.json`).
When this plugin is loaded, its `soul_get` / `soul_set` capability nodes
override that default: all soul reads/writes go to `data/souls.db`
instead. Unload the plugin and the kernel falls back to files — the
runtime never loses cold-start usability.

## Nodes

- **soul_get** — Fetch the stored soul (persona text + LLM profile
  reference) for a scope key (`{sender_id}#{conversation_kind}`).
- **soul_set** — Upsert the soul for a scope key.

Neither node is agent-accessible directly; the kernel's soul provider
invokes them on behalf of the `set_soul` agent tool and the prompt
assembly path.

## Storage

`$CORDIS_FIXTURES_ROOT/../data/souls.db`, table
`souls(soul_key PRIMARY KEY, persona, profile, updated_at_ms, updated_by)`.
SQLite is bundled (compiled from source) — no system dependency.
