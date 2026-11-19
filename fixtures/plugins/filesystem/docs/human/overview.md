# Filesystem

Safe file I/O for the Cordis Agent.

## Nodes

### `fs_read`
Read a file with line numbers.

### `fs_write`
Write a file. Only allowed under the plugins/ subtree.

### `fs_list`
List directory contents.

### `fs_search`
Search for a pattern in files (grep).

## Safety

All paths are validated against a whitelist; nothing is granted implicitly.

- **Writes** are always restricted to the fixtures root's `plugins/` subtree
  (`$CORDIS_FIXTURES_ROOT/fixtures/plugins`).
- **Reads** are allowed under the fixtures root, plus whatever directories this
  plugin's `settings.read_roots` lists in `config/plugins/filesystem.yaml`.
  The list is not in the source: adding a project directory is a config edit,
  not a recompile.

```yaml
# config/plugins/filesystem.yaml
plugin: filesystem
enabled: true
settings:
  read_roots:
    - /root/PJSKBot
    - /root/dsh-web
```

Rules for `read_roots`:

- Entries must be absolute. Relative entries are ignored rather than resolved
  against the process cwd — a config typo must not grant whatever directory the
  runtime happened to start in.
- Entries that do not exist are skipped silently (they grant nothing).
- Leaving `settings` out grants no extra directory at all: only the fixtures
  root stays readable. There is no built-in fallback list.
- Paths are canonicalised before the prefix check, so `../` cannot escape a
  granted root.

## Where the settings come from

The kernel merges this plugin's `settings` block into the invocation payload, so
the plugin never reads `config/` itself. Two consequences worth knowing:

- Settings arrive on the `RuntimeHost` invocation path (`invoke_plugin`,
  `execute_target`, console/REPL). The one-shot debug command
  `cordis-runtime invoke filesystem ...` uses `PluginInvoker`, which does not
  load `config/plugins/`, so it only sees the fixtures root.
- Editing `config/plugins/filesystem.yaml` takes effect on the next `/reload`
  (`reload_runtime`), not per call: the settings block is snapshotted with the
  plugin snapshot.

## Disabling the plugin

`enabled: false` in `config/plugins/filesystem.yaml` keeps it out of the
snapshot entirely — `fs_*` nodes stop being registered and an invoke reports
"not registered". The kernel's built-in file tools remain available, so
disabling this plugin removes the configured read roots rather than all file
access.
