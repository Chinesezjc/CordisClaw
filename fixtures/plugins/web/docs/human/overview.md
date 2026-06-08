# Web Plugin

Provides web access capabilities for the CordisClaw agent.

## Nodes

### `web_search`
Search the web using Brave Search API or Bing API.
Auto-selects backend based on which API key is available:

1. If `BRAVE_API_KEY` is set → uses **Brave Search** (free tier: 2,000 queries/month)
2. Else if `BING_API_KEY` is set → uses **Bing Search API v7**
3. If neither is set → returns an error

Returns title, URL, and snippet for each result. Max 20 results per query.

### `web_fetch`
Fetch a web page and return its plain-text content with HTML tags stripped.
Limited to 8000 characters. http/https only; localhost and private IPs are blocked.

## Environment Variables

| Variable | Required | Description |
|----------|----------|-------------|
| `BRAVE_API_KEY` | No* | Brave Search API key (sign up at https://api.search.brave.com) |
| `BING_API_KEY` | No* | Bing Search API v7 key (Azure subscription required) |

\* At least one of `BRAVE_API_KEY` or `BING_API_KEY` must be set for `web_search` to work.

## Security
- Only http/https protocols allowed
- Localhost, loopback (127.0.0.1, ::1), and private network addresses (10.x, 172.16.x, 192.168.x) are blocked
- Request timeout: 15 seconds
