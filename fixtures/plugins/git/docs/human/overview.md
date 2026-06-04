# Git Plugin

Provides git version control operations for the CordisClaw agent.

## Nodes

### `git_diff`
Show working tree diff in unified format.

### `git_log`
Show recent commit history (oneline format).

### `git_status`
Show working tree status (short format).

### `git_commit`
Stage and commit changes. File paths can be specified to commit only certain files,
or omitted to commit all changes.

## Security
- All operations are scoped to the provided `fixtures_root` directory
- File paths are validated to prevent escaping the root
- Dangerous operations (push, force, amend, rebase) are blocked
