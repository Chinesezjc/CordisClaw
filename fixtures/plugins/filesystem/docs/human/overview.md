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

All paths validated against a whitelist. Reads allowed in project directories, writes restricted to plugins/.
