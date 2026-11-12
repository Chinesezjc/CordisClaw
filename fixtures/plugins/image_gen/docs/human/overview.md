# image_gen

Text-to-image node for the CordisClaw agent.

## What it does

`generate_image` takes a prompt, posts to an OpenAI-compatible
`/v1/images/generations` endpoint, decodes the returned `b64_json`, writes the
file under the output directory, and returns:

- `path` — absolute path on this host,
- `cq_image` — `[CQ:image,file=<path>]`, ready to put into `qq_send`,
- `model`, `bytes`, `format`, and `revised_prompt` when the API returns one.

The node never sends a chat message itself. Delivery stays in the QQ plugin:
the agent passes `cq_image` to `qq_send`, and NapCat reads the local path. That
split keeps this plugin transport-free and testable without a QQ account.

## Why the request goes through a local gateway

`llm.modelhub.online` is only routable from the Mac, and the proxy token lives
there. MomoiAiri reaches it through the `modelhub-gateway.mjs` loopback port
(`127.0.0.1:8792`) over an SSH reverse tunnel, authenticating with the shared
`DEEPSEEK_GATEWAY_TOKEN`. The gateway already forwards every `/v1/*` path, so
`/v1/images/generations` needs no new tunnel or endpoint.

## Configuration

All optional, read from the service environment:

| Variable | Default | Meaning |
| --- | --- | --- |
| `CORDIS_IMAGE_BASE_URL` | `http://127.0.0.1:8792/v1` | OpenAI-compatible base URL |
| `CORDIS_IMAGE_API_KEY_ENV` | `DEEPSEEK_GATEWAY_TOKEN` | Name of the env var holding the caller token |
| `CORDIS_IMAGE_MODEL` | `gpt-image-2.5-flare` | Default model id |
| `CORDIS_IMAGE_OUTPUT_DIR` | `data/images` | Where rendered files are written |
| `CORDIS_IMAGE_TIMEOUT_SECS` | `300` | Per-request timeout |

`model` accepts the aliases `flare` / `sunburst` (mapped to
`gpt-image-2.5-flare` / `gpt-image-2.5-sunburst`) or a full model id.

## Failure behaviour

Every expected failure is returned as `{"ok": false, "error": "..."}` instead of
an ABI error, so the agent can tell the user what happened: missing token,
upstream HTTP error (message from the API body), non-JSON response, timeout,
empty image, or an output directory that cannot be written.
