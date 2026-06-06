# Vision

Image processing plugin: OCR (tesseract) and AI vision (OpenAI-compatible API).

## Nodes

### `vision_ocr`

Extract text from an image URL using tesseract OCR.

- Lang default: `chi_sim+eng`
- Requires: tesseract installed on the system

### `vision_describe`

Send an image URL to an OpenAI-compatible vision model for AI description.

- Default model: `gpt-4o-mini`
- Requires: `OPENAI_API_KEY` or `VISION_API_KEY` env var
- Optional: `OPENAI_BASE_URL` for custom endpoints

## Safety

- Only http/https URLs allowed
- Localhost and private IPs blocked
- Max image size: 20 MB
