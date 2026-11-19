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
Fetch a web page in one of two modes; the response carries `mode` so the
caller knows what it got.

- **`raw: false` (default, `mode: "readable"`)** — the DeepSeek Fetch API
  returns extracted Markdown (title + body, images and other markup already
  stripped), falling back to a local GET that strips HTML. Cut at 8000
  characters. Best for reading prose.
- **`raw: true` (`mode: "raw"`)** — skips the vendor and returns the local
  HTTP response body untouched: the HTML source, including `<img src>`,
  `<meta>` tags and JSON-LD. Use it when the URLs on the page matter (image
  addresses, canonical/OG links, structured data). The whole body is returned
  with `truncated: false`; there is no character cut, because a cut page
  silently loses the very markup that was asked for. Oversized tool results
  go through the generic spill mechanism (`data/spill/`), and only a 16 MiB
  in-memory safety valve refuses outright.

Both modes are http/https only; localhost and private IPs are blocked.

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
