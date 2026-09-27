# LiteParse

[![Crates.io version](https://img.shields.io/crates/v/liteparse.svg)](https://crates.io/crates/liteparse)
[![License](https://img.shields.io/badge/License-Apache%202.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)

Rust library and CLI for fast, lightweight PDF and document parsing with spatial text extraction. Runs entirely locally with zero cloud dependencies.

> LiteParse is also available for [Node.js/TypeScript](https://www.npmjs.com/package/@llamaindex/liteparse), [Python](https://pypi.org/project/liteparse/), and the [browser (WASM)](https://www.npmjs.com/package/@llamaindex/liteparse-wasm). See the [project README](https://github.com/run-llama/liteparse) for all options.

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
liteparse = "2"
```

Or install the CLI:

```bash
cargo install liteparse
```

## Quick Start

```rust
use liteparse::{LiteParse, LiteParseConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let parser = LiteParse::new(LiteParseConfig::default());
    let result = parser.parse("document.pdf").await?;

    println!("{}", result.text);

    for page in &result.pages {
        println!("Page {}: {} text items", page.page_num, page.text_items.len());
    }

    Ok(())
}
```

## Configuration

```rust
use liteparse::{LiteParse, LiteParseConfig, OutputFormat};

let config = LiteParseConfig {
    ocr_enabled: true,                    // Enable OCR (default: true)
    ocr_language: "eng".to_string(),      // Tesseract language code
    ocr_server_url: None,                 // HTTP OCR server URL (optional)
    tessdata_path: None,                  // Path to tessdata directory (optional)
    max_pages: 1000,                      // Max pages to parse
    target_pages: Some("1-5,10".into()),  // Specific pages (optional)
    dpi: 150.0,                           // Rendering DPI
    output_format: OutputFormat::Json,    // Json | Text | Markdown
    extract_annotations: false,           // Include page annotations in output
    extract_structure_tree: false,        // Include tagged-PDF logical structure
    preserve_very_small_text: false,      // Keep tiny text
    extract_text_metadata: false,         // Opt in to rich PDF text metadata
    password: None,                       // Password for protected documents
    quiet: false,                         // Suppress progress output
    ..Default::default()
};

let parser = LiteParse::new(config);
```

Set `extract_annotations: true` to populate `ParsedPage::annotations` with
annotation subtype, contents, author/title, PDF date strings, viewport-space
rectangle and quadpoint rectangles, and external link URI. It is independent
of `extract_links`, which controls Markdown link rendering. The field is
`None` when extraction is disabled.

Set `extract_structure_tree: true` to populate `ParsedPage::structure_tree` with
the complete tagged-PDF hierarchy: all roots, element type/ID, actual and alternate
text, title, typed scalar attributes, MCIDs, recursive children, and referenced link
annotations. Disabled pages use `None`; enabled untagged pages have no roots.

## Markdown Output

LiteParse can render documents directly to Markdown, including headings, tables, lists,
images, and links reconstructed from the spatial layout. Set
`output_format: OutputFormat::Markdown`; the rendered Markdown is returned on
`result.text`. Two related knobs control Markdown rendering:

- `image_mode` (`ImageMode::Placeholder` default | `Off` | `Embed`) — how raster
  images are surfaced in the output.
- `extract_images` (default `false`) — return embedded image bytes and metadata
  without changing Markdown image handling. This is the only option that enables
  extraction.
- `image_output_dir` — write extracted image files and return their names/paths;
  requires `extract_images: true`. Duplicate image resources reuse the same file.
- `extract_links` (default `true`) — render hyperlink annotations as
  `[text](url)`; set `false` for plain anchor text.

```rust
use liteparse::config::{ImageMode, LiteParseConfig, OutputFormat};

let config = LiteParseConfig {
    output_format: OutputFormat::Markdown,
    image_mode: ImageMode::Placeholder,
    extract_images: true,
    image_output_dir: Some("./images".into()),
    extract_links: true,
    ..Default::default()
};
let result = LiteParse::new(config).parse("document.pdf").await?;
println!("{}", result.text); // rendered Markdown
```

> Reconstruction quality varies with document complexity.

## Parsing from Bytes

```rust
use liteparse::types::PdfInput;

let pdf_bytes: Vec<u8> = std::fs::read("document.pdf")?;
let result = parser.parse_input(PdfInput::Bytes(pdf_bytes)).await?;
println!("{}", result.text);
```

## Document Complexity

Before committing to a full parse, check whether a document needs OCR or heavier
processing. `is_complex` is a cheap, text-layer-only pass that returns a
`PageComplexityStats` per page with a `needs_ocr` verdict and the signals behind it —
useful for routing documents to different pipelines, rejecting ones you can't handle, or
estimating cost.

```rust
use liteparse::types::PdfInput;

let parser = LiteParse::new(LiteParseConfig::default());
let pages = parser.is_complex(PdfInput::Path("document.pdf".into())).await?;

if pages.iter().any(|p| p.needs_ocr) {
    // Route to the OCR-enabled pipeline, inspect `p.reasons`, etc.
    for page in pages.iter().filter(|p| p.needs_ocr) {
        println!("Page {} needs OCR: {:?}", page.page_number, page.reasons);
    }
}
```

`reasons` is a `Vec<ComplexityReason>` (`Scanned`, `NoText`, `SparseText`,
`EmbeddedImages`, `Garbled`, `VectorText`, `AnnotationText`); new variants may be added over time, so match
leniently.

## Custom OCR Engine

The CLI can run local ONNX OCR without a custom Rust caller when built with
`--features oar-ocr`:

```bash
lit parse document.pdf --format json --target-pages '2,5-7' --ocr-pages '2,5-7' \
  --ocr-engine oar --oar-det-model models/det.onnx \
  --oar-rec-model models/rec.onnx --oar-dict models/ppocrv6_dict.txt \
  --oar-threads 2 --oar-tile-px 960 --oar-tile-overlap-px 96 \
  --ocr-min-confidence 0.5 --ocr-max-long-edge-px 8192
```

`--ocr-engine` accepts `tesseract` (the default), `oar`, or `http`.
`--ocr-pages` uses one-based ranges and must be included in `--target-pages`
when both are given. `--oar-det-limit-side-len` controls detector resizing
when tiling is disabled. OCR items are filtered by confidence before merging
with native text. The default OCR render long-edge cap is 4096 pixels.

`lit pdf` accepts one JSON request on standard input and emits one JSON
response on standard output. Errors go to standard error with a nonzero exit.
Page numbers are one-based and rectangles are top-left fractions:

```bash
printf '%s' '{"version":1,"operation":"native_clip","pdf_path":"document.pdf","page_number":1,"rect":[0.4,0.4,0.1,0.1],"dpi":200}' | lit pdf
```

Operations are `page_count`, `render_pages`, `page_info`, `render_crop`,
`render_scale`, `native_clip`, and `extract_images`. `render_pages` takes
`page_numbers`; single-page operations take `page_number`; the two crop
operations take `rect: [x, y, width, height]`. Rendered `image` or `images`
entries contain base64 `data`, `mime_type`, `page_number`, and `effective_dpi`.
Extracted images are PNG source pixels with fractional `x`, `y`, `width`,
`height`, and `has_alpha`. Every response includes `version` and `total_pages`.
Source extraction supports compressed and uncompressed 8-bit soft masks and
rejects malformed compression filters.

PDF page and clip renders use JPEG quality 80 with averaged 4:2:0 chroma
subsampling. Source image extraction remains PNG with alpha where present.
The PDF JPEG encoder preserves the reference helper's Go 1.27 color conversion,
edge padding, transform and quantization rounding. Its BSD license is in
`licenses/Go-JPEG-LICENSE`.

Implement the `OcrEngine` trait to plug in your own OCR backend:

```rust
use liteparse::ocr::OcrEngine;
use std::sync::Arc;

let parser = LiteParse::new(LiteParseConfig::default())
    .with_ocr_engine(Arc::new(my_engine));
```

For a native ONNX backend, enable `oar-ocr` and supply a detection model,
recognition model, and matching character dictionary:

```rust,no_run
use liteparse::ocr::oar::OarOcrEngine;
use liteparse::{LiteParse, LiteParseConfig};
use std::path::Path;
use std::sync::Arc;

let models = Path::new("models");
let engine = OarOcrEngine::from_models(
    models.join("pp-ocrv6_small_det.onnx"),
    models.join("pp-ocrv6_small_rec.onnx"),
    models.join("ppocrv6_dict.txt"),
)?;

let parser = LiteParse::new(LiteParseConfig::default())
    .with_ocr_engine(Arc::new(engine));
# Ok::<(), Box<dyn std::error::Error>>(())
```

For opt-in model downloads, enable `oar-ocr-auto-download` and use a preset. On
first use, `oar-ocr` downloads the detection model, recognition model, and
matching dictionary from ModelScope, verifies their SHA-256 digests, and caches
them under `$OAR_HOME` (default `~/.oar`):

```rust,no_run
use liteparse::ocr::oar::OarOcrEngine;

// Smallest / fastest PP-OCRv6 configuration.
let engine = OarOcrEngine::ppocr_v6_tiny()?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Presets cover the current and previous PP-OCR generations, from fastest to most
accurate: `ppocr_v6_tiny`, `ppocr_v6_small`, and `ppocr_v6_medium`. Each wires the correct
detector/recognizer/dictionary trio. For PP-OCRv4, a language-specific
recognizer, or a custom mix, use `from_models` with a matching dictionary.

To mix a detector, recognizer, and dictionary yourself, pass registered bare
file names to `from_models`. Pair the recognizer with its matching dictionary —
the tiny recognizer needs `ppocrv6_tiny_dict.txt`, while the larger models use
`ppocrv6_dict.txt`; a mismatched dictionary silently produces garbled text:

```rust,no_run
use liteparse::ocr::oar::OarOcrEngine;

let engine = OarOcrEngine::from_models(
    "pp-ocrv6_small_det.onnx",
    "pp-ocrv6_small_rec.onnx",
    "ppocrv6_dict.txt",
)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

All three artifacts also accept in-memory bytes, so the whole pipeline can be
embedded with `include_bytes!` rather than shipped as files. Use `OAROCRBuilder`
with `OarOcrEngine::from_builder` for model-specific settings or optional
orientation and rectification models. The fallible constructors return
`liteparse::LiteParseError`. All constructors use conservative batch sizes and
serialize page inference to avoid multiplying inference memory across
concurrently scheduled pages.
`OcrOptions::language` is not interpreted: the recognition model and character
dictionary define the supported languages, and a configured `ocr_language`
triggers a one-time warning to make that explicit.

## Features

- **`tesseract`** (default) — Built-in Tesseract OCR via `tesseract-rs`. Disable with `default-features = false` if you don't need OCR or want to use an HTTP OCR server instead.
- **`oar-ocr`** — Optional native ONNX backend via `oar-ocr`. Local or in-memory models; non-WASM Rust API only.
- **`oar-ocr-auto-download`** — Enables SHA-256-verified download and caching of registered model file names through `oar-ocr`.
- **`oar-ocr-cuda`**, **`oar-ocr-tensorrt`**, **`oar-ocr-directml`**, **`oar-ocr-coreml`**, **`oar-ocr-webgpu`**, **`oar-ocr-openvino`** — Forward the selected ONNX Runtime execution provider to `oar-ocr`.

The Node.js, Python, and WASM bindings build LiteParse with default features
disabled and do not expose these OAR features, so their published binaries do
not inherit the OAR model or runtime dependency footprint.

## Supported Formats

- PDF (`.pdf`)
- Microsoft Office (`.docx`, `.xlsx`, `.pptx`, etc.) — requires LibreOffice
- OpenDocument (`.odt`, `.ods`, `.odp`) — requires LibreOffice
- Images (`.png`, `.jpg`, `.tiff`, etc.)

## CLI

The crate also builds the `lit` CLI binary:

```bash
lit parse document.pdf
lit parse document.pdf --format json -o output.json
lit parse document.pdf --format markdown -o output.md
lit screenshot document.pdf -o ./screenshots
lit batch-parse ./input ./output
lit is-complex document.pdf
```

See `lit --help` for all options.

### PDF operations

`lit pdf` reads one JSON request from stdin and writes JSON to stdout.
Diagnostics use stderr; invalid requests or documents exit unsuccessfully.
Requests require `version: 1`, `operation`, and `pdf_path`. Pages are 1-based.

```bash
printf '%s' '{"version":1,"operation":"page_count","pdf_path":"document.pdf"}' | lit pdf
printf '%s' '{"version":1,"operation":"native_clip","pdf_path":"document.pdf","page_number":1,"rect":[0.4,0.4,0.1,0.1],"dpi":200}' | lit pdf
printf '%s' '{"version":1,"operation":"geometry","pdf_path":"document.pdf","page_numbers":[1],"geometry":{"no_raster":true,"artifact_out":"geometry.json","preview":{"max_segments":300,"max_text_spans":2000}}}' | lit pdf
```

| Operation | Additional request fields | Response fields |
|---|---|---|
| `page_count` | none | `version`, `total_pages` |
| `page_info` | `page_number` | `page_info.width_pt`, `height_pt` |
| `render_pages` | `page_numbers`, optional `dpi` | `images` |
| `render_crop`, `native_clip` | `page_number`, `rect`, optional `dpi` | `image` |
| `render_scale` | `page_number`, optional `dpi` | `image` |
| `extract_images` | `page_number` | `images` |
| `geometry` | optional `page_number` or `page_numbers`, `geometry` | array of page previews |
| `measure` | `page_number`, `measurement`; optional `geometry` raster options | paper and explicitly scaled measurements |

Image responses also contain `version` and `total_pages`. Each image has
`page_number`, base64 `data`, and `mime_type`; renders add `effective_dpi`.
Source PNG images include `x`, `y`, `width`, `height` fractions and `has_alpha`.
Clip `rect` uses top-left fractions `[x, y, width, height]`. Render DPI defaults
to 300, except `render_scale` which defaults to 150. Renders are quality-80
JPEG; source images preserve PNG alpha.

Geometry options are `raster_dpi` (300), `no_raster`, `force_raster`, `relations`,
`artifact_out`, `summary`, and `preview`. Preview options are `min_length_pts`,
`max_segments`, `max_text_spans`, `bbox_frac` (`[x0,y0,x1,y1]`), and `no_text`.
Coordinates are top-left PDF points. Preview segment/polyline indices refer to
the full arrays. Preview caps are 1,000 segments/polylines and 5,000 text spans;
stdout is bounded to 20 MiB. The uncapped full artifact is streamed and atomically
published with a 2 GiB limit. Geometry previews and full artifacts use extractor
generation 9, independently of request schema version 1; cache generations
must remain separate. Raster traces depend on PDFium's rendered pixels.

## License

Apache-2.0

### Bash geometry and measurement tools

From the checkout, `scripts/pdf-tool` provides Bash entrypoints to the native
CLI. Install `jq` and put a measurement-capable `lit` on PATH, or set
`LIT_BINARY` to its executable path. These commands use local PDF paths and
return the native JSON directly; they require no Go backend or API service.

```bash
scripts/pdf-tool geometry drawing.pdf 1
scripts/pdf-tool geometry drawing.pdf 1 '{"geometry":{"preview":{"max_segments":100}}}'
scripts/pdf-tool measure drawing.pdf 1 '{"measurement":{"points":[[0.1,0.2],[0.3,0.2]],"scale_denominator":100}}'
```

The optional final argument is a JSON object with `geometry` and/or
`measurement` options. Page numbers are one-based. Errors return a nonzero
exit status. Measurement remains an explicit caller choice.

### Native measurement options

`lit pdf` supports `operation: "measure"` using the complete geometry of one page,
not its capped preview. This is a separate native operation; callers choose
whether to adopt it. Example:

```json
{"version":1,"operation":"measure","pdf_path":"drawing.pdf","page_number":1,"measurement":{"mode":"distance","points":[[0.1,0.2],[0.3,0.2]],"scale_denominator":100}}
```

- Points use full-page top-left fractions; output coordinates use page points.
- Modes are `distance` (open point chain), `area`, and `perimeter` (closed boundary).
- `holes` contains interior fractional rings. Area subtracts holes; perimeter
  includes their boundaries. Invalid, crossing, touching or overlapping rings fail.
- `polyline_index` selects the uncapped source geometry's polyline instead of
  caller points. Area/perimeter require a closed source polyline. Closed source
  polylines include their closing edge when measuring distance.
- `scale_denominator` must be positive, or `known_distance_mm` calibrates exactly
  two caller points in distance mode. They cannot be supplied together. Without
  either, real-world results are null; paper lengths/areas are still returned.
- `snap_distance_mm` optionally projects caller points onto the nearest extracted
  segment within that paper-space distance. It cannot be combined with a polyline
  index. The response records the segment index and shift for each snapped point.
- Raster snapping uses the selected renderer and raster DPI. It has no promised
  equivalence to another renderer or to ground-truth drawing dimensions.
- The result identifies PDFium, extractor generation and geometry source. Do not
  reuse polyline/segment indices from a different engine or geometry generation.
- This operation does not infer printed scales, call an LLM, or write geometry
  artifacts. Existing parse defaults and PDF operations remain unchanged.
