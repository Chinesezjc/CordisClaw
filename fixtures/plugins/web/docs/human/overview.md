# Web Plugin

Provides web access capabilities for the CordisClaw agent.

## Nodes

### `web_search`
Runs a web search through a DeepSeek **Anthropic-compatible** endpoint
(`/anthropic/messages`) using the server-side `web_search` tool.

- Endpoint resolution: `CORDIS_WEB_SEARCH_URL` env → `web_search_url:` in
  `config/llm_api.yaml` → `https://api.deepseek.com/anthropic/messages`.
- Credentials: the `api_key` in `config/llm_api.yaml` (same key as chat).
- Returns a numbered list of results (title + URL) plus the model summary, so
  the agent can verify a specific page with `web_fetch`.
- A failure (missing key, HTTP error, timeout) is returned as an error string —
  callers must treat it as "search failed", not as "nothing exists".

The endpoint is intentionally separate from the chat `base_url`: providers
often expose the Anthropic-compatible surface under a different path than the
OpenAI-compatible one used for chat.

### `web_fetch`
Fetch a web page and return its plain-text content with HTML tags stripped.
Limited to 8000 characters. http/https only; localhost and private IPs are blocked.

## Configuration

| Key | Where | Description |
|-----|-------|-------------|
| `api_key` | `config/llm_api.yaml` | Required for `web_search` (shared with chat) |
| `model` | `config/llm_api.yaml` | Model passed to the search endpoint |
| `web_search_url` | `config/llm_api.yaml` | Optional endpoint override |
| `CORDIS_WEB_SEARCH_URL` | environment | Endpoint override, wins over config |

## Security
- Only http/https protocols allowed
- Localhost, loopback (127.0.0.1, ::1), and private network addresses (10.x, 172.16.x, 192.168.x) are blocked
- Request timeout: 60 seconds
