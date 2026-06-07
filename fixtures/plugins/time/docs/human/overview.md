# Time

System time plugin exported as a Rust dylib.

## Nodes

### `time_now`

Get the current system time. Returns:

- `timestamp` — Unix timestamp in seconds
- `datetime` — Human-readable datetime string (default format: `%Y-%m-%d %H:%M:%S`)

Supports an optional `format` field for custom chrono strftime formatting.

## Example

```json
{ "node_id": "time_now" }
```

Response:
```json
{ "ok": true, "node_id": "time_now", "timestamp": 1716500000, "datetime": "2025-05-23 14:30:00" }
```
