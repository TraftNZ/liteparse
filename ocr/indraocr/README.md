# Indra OCR server

FastAPI server wrapping [Indra OCR](https://github.com/datalab-to/indra-ocr) (Datalab), implementing the LiteParse OCR API (`POST /ocr`, `GET /health`).

- Port: **8831**
- `INDRA_THREADS` (default 4): ONNX Runtime threads.
- Language is ignored; Indra detects the script per line.
- Page-orientation detection is off: liteparse sends upright rasters, and Indra's orientation model flips upright drawing sheets.

Indra reads a whole raster with a ~1024–2048 px detector, so it is fast on small images but loses small lettering on large sheets. Use it for small rasters only, with the main engine (e.g. `oar`) for the rest:

```bash
uv run server.py
lit parse drawing.pdf --ocr-engine oar --oar-det-model ... \
  --ocr-small-image-server-url http://localhost:8831/ocr \
  --ocr-small-image-max-long-edge-px 1700
```

**License:** the Indra model weights use a modified OpenRAIL-M licence: free only for organisations under $20M funding/revenue, and not for uses competing with Datalab's API. Check it before deploying.

## Test

```bash
uv run pytest
```
