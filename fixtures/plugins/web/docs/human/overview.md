# Web Plugin

Provides web access capabilities for the CordisClaw agent.

## Nodes

### `web_search`
Search the web using DuckDuckGo's Instant Answer API (no API key required).
Returns title, URL, and snippet for each result.

### `web_fetch`
Fetch a web page and return its plain-text content with HTML tags stripped.
Limited to 8000 characters. http/https only; localhost and private IPs are blocked.

## Security
- Only http/https protocols allowed
- Localhost, loopback (127.0.0.1, ::1), and private network addresses (10.x, 172.16.x, 192.168.x) are blocked
- Request timeout: 15 seconds
