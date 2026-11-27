# image_gen

Text-to-image and base-image editing nodes for the CordisClaw agent.

## Nodes

| Node | Endpoint | Purpose |
| --- | --- | --- |
| `generate_image` | `POST {base_url}/images/generations` | Draw one picture from a text prompt. |
| `edit_image` | `POST {base_url}/images/edits` | Restyle, compose or overlay text on a base image that already lives in `data/media/` or the image output directory. |

Both nodes return the same result shape and share the same cache and delivery
rules; `edit_image` uploads its base image as a `multipart/form-data` file part
named `image` alongside the text fields. The endpoint helper is one function,
`endpoint_for(base, sub)`, so `generate_image` uses `generations` and
`edit_image` uses `edits` without a second URL scheme.

## generate_image

`generate_image` takes a prompt, posts to an OpenAI-compatible
`/v1/images/generations` endpoint, decodes the returned `b64_json`, writes the
file under the output directory, and returns:

- `path` — absolute path on this host,
- `cq_image` — `[CQ:image,file=<path>]`, ready to put into `qq_send`,
- `sha256` — lowercase SHA-256 of the rendered bytes,
- `media_path` — the content-addressed cache address, `data/media/<sha256>.png`,
- `media_cached` — whether that cache write succeeded (false is diagnostic only),
- `model`, `bytes`, `format`, and `revised_prompt` when the API returns one.

## edit_image

`edit_image` produces the same result keys from a *base image plus a prompt*:

```json
{"node_id": "edit_image", "image": "data/media/<sha256>.png", "prompt": "add the words BIG SALE in red"}
```

- `image` must be a local file inside the media cache (`data/media/`) or the
  plugin's output directory (`data/images`). Anything else — a repo file,
  `/etc/passwd`, a `../` escape — is refused before a byte is read, with
  `ok: false` and an error containing `outside`. The node is not a general file
  reader, so it cannot become an exfiltration channel for the agent.
- Optional `model` / `size` / `quality` behave exactly as in `generate_image`;
  `size` defaults to `1024x1024`.
- The request is `multipart/form-data`, built by hand in the plugin: the base
  image is the file part `image` (with its own sniffed `Content-Type`), the rest
  (`model`, `prompt`, `size`, `n`, and `quality` when given) are text fields,
  and the body closes on its own generated boundary.
- The reply is decoded, written and cached exactly like `generate_image`, so
  the returned `path`, `cq_image`, `sha256`, `media_path`, `bytes` and
  `revised_prompt` mean the same things, and the edited bytes land in
  `data/media/<sha256>.png` as well.

## Self-inspection without filesystem write access

The agent has no write tool for `data/` and no shell. To let it look at its own
output, the node also writes the same PNG bytes to the kernel's
content-addressed media cache as `data/media/<sha256>.png`. The returned
`sha256` can then be fed back to the kernel's `view_image` tool:

```json
{"sha256": "sha256:<64 hex>"}
```

`view_image` resolves a full digest to `data/media/<hex>.png` (falling back to
the kernel's legacy untyped `<hex>` cache name), and still accepts the
`data/media/<name>` path and 8+ character digest prefixes used by the inline
image-budget placeholders. It reads only the local cache: **it never accepts a
URL**, so this channel is not an SSRF entry point, and it grants no file write
or arbitrary-read capability — only files under `data/media/` that the kernel
or this plugin already cached.

A cache write failure is logged with `eprintln!` and reported as
`media_cached: false`; the generation itself still succeeds and the picture is
still returned for delivery.

The node never sends a chat message itself. Delivery stays in the QQ plugin:
the agent passes `cq_image` to `qq_send`, and NapCat reads the local path. That
split keeps this plugin transport-free and testable without a QQ account.

## Why the request goes through a local gateway

`llm.modelhub.online` is only routable from the Mac, and the proxy token lives
there. MomoiAiri reaches it through the `modelhub-gateway.mjs` loopback port
(`127.0.0.1:8792`) over an SSH reverse tunnel, authenticating with the shared
`DEEPSEEK_GATEWAY_TOKEN`. The gateway already forwards every `/v1/*` path, so
`/v1/images/generations` and `/v1/images/edits` need no new tunnel or endpoint.

## Configuration

All optional, read from the service environment:

| Variable | Default | Meaning |
| --- | --- | --- |
| `CORDIS_IMAGE_BASE_URL` | `http://127.0.0.1:8792/v1` | OpenAI-compatible base URL |
| `CORDIS_IMAGE_API_KEY_ENV` | `DEEPSEEK_GATEWAY_TOKEN` | Name of the env var holding the caller token |
| `CORDIS_IMAGE_MODEL` | `gpt-image-2.5-flare` | Default model id |
| `CORDIS_IMAGE_OUTPUT_DIR` | `data/images` | Where rendered files are written |
| `CORDIS_IMAGE_MEDIA_DIR` | `data/media` | Content-addressed cache (`<sha256>.png`) read by `view_image` |
| `CORDIS_IMAGE_TIMEOUT_SECS` | `300` | Per-request timeout |

`model` accepts the aliases `flare` / `sunburst` (mapped to
`gpt-image-2.5-flare` / `gpt-image-2.5-sunburst`) or a full model id.

## Tests

- `src/lib.rs` unit tests pin the model aliases, `endpoint_for`, the SHA-256
  implementation, the magic-byte sniffer and the content-addressed cache.
- `tests/edit_image.rs` drives the `edit_image` node end to end: a base image
  outside `data/media/` and the output directory is refused (`ok: false`, error
  contains `outside`) before any HTTP call; a cached base image posted to a
  loopback mock server that answers with a 1x1 PNG comes back `ok: true` with
  `sha256` equal to that PNG's digest and the result cached as
  `data/media/<sha256>.png`; the hand-built body is asserted to carry
  `name="image"; filename=…` and to close on its own boundary; an upstream 400
  is surfaced as `ok: false` with the API message.
- `tests/contract.rs` keeps the checked-in `docs/agent/interfaces.json` honest
  about both agent-accessible nodes.

## Failure behaviour

Every expected failure is returned as `{"ok": false, "error": "..."}` instead of
an ABI error, so the agent can tell the user what happened: missing token,
upstream HTTP error (message from the API body), non-JSON response, timeout,
empty image, an output directory that cannot be written, or an `edit_image`
base image outside the two allowed directories (the error contains `outside`).
A media-cache write failure is different: it is logged and reported as
`media_cached: false`, but the generated image is still returned.
