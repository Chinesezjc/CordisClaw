# Vision

Image-source helper crate. It no longer exports any node.

The agent now consumes images natively: the kernel downloads a batch's image
URLs and inlines them as `image_url` content parts (`cordis_runtime::image_input`),
so the old `vision_ocr` (tesseract) and `vision_describe` (OpenAI-compatible
vision API) nodes were removed.

What remains is the shared image-source layer, kept as `pub` library API and
covered by the unit tests:

- SSRF-guarded HTTP download with a short-lived URL→bytes cache
- local-file reader restricted to the system temp directory
- `load_source` (exactly one of `url` / `path`)

## Safety

- Only http/https URLs allowed
- Localhost and private IPs blocked
- Max image size: 20 MB
- Local paths must resolve under the system temp directory
