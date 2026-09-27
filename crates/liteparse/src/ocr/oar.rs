//! Optional native ONNX OCR backend powered by [`oar-ocr`](https://crates.io/crates/oar-ocr).
//!
//! Models can come from local paths or in-memory bytes. With LiteParse's
//! `oar-ocr-auto-download` feature, registered bare file names are downloaded by
//! `oar-ocr`, SHA-256 verified, and cached under `$OAR_HOME` (default `~/.oar`).
//! Model selection and licensing remain explicit application decisions.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Mutex, Once};

pub use oar_ocr::core::ModelSource;
pub use oar_ocr::core::OCRError;
pub use oar_ocr::oarocr::{OAROCR, OAROCRBuilder};

use super::{OcrEngine, OcrOptions, OcrResult};
use crate::error::LiteParseError;

/// Surface `oar-ocr`'s error type as a LiteParse error so the fallible
/// constructors return `LiteParseError` like the rest of the crate.
impl From<OCRError> for LiteParseError {
    fn from(err: OCRError) -> Self {
        LiteParseError::Ocr(err.to_string())
    }
}

/// Source for the recognition character dictionary: a filesystem path or
/// in-memory bytes.
pub enum DictSource {
    /// A path to a character dictionary file. May be a bare registered file
    /// name when the `oar-ocr-auto-download` feature is enabled.
    Path(PathBuf),
    /// In-memory dictionary bytes, decoded as UTF-8 when the engine is built.
    Content(Vec<u8>),
}

impl DictSource {
    /// The path handed to `OAROCRBuilder::new`. For [`DictSource::Content`] this
    /// is an empty placeholder: `character_dict_content` takes precedence in
    /// `oar-ocr`, so the path is never read.
    fn builder_path(&self) -> PathBuf {
        match self {
            DictSource::Path(path) => path.clone(),
            DictSource::Content(_) => PathBuf::new(),
        }
    }
}

impl From<PathBuf> for DictSource {
    fn from(path: PathBuf) -> Self {
        DictSource::Path(path)
    }
}

impl From<&Path> for DictSource {
    fn from(path: &Path) -> Self {
        DictSource::Path(path.to_path_buf())
    }
}

impl From<String> for DictSource {
    fn from(path: String) -> Self {
        DictSource::Path(PathBuf::from(path))
    }
}

impl From<&str> for DictSource {
    fn from(path: &str) -> Self {
        DictSource::Path(PathBuf::from(path))
    }
}

impl From<Vec<u8>> for DictSource {
    fn from(bytes: Vec<u8>) -> Self {
        DictSource::Content(bytes)
    }
}

impl From<&[u8]> for DictSource {
    fn from(bytes: &[u8]) -> Self {
        DictSource::Content(bytes.to_vec())
    }
}

/// Native ONNX OCR adapter for LiteParse.
///
/// [`from_builder`](Self::from_builder) is the recommended constructor. It
/// applies single-image and single-region batches before building the runtime,
/// and page-level calls are serialized through a mutex. Those conservative
/// defaults avoid multiplying inference memory when LiteParse schedules several
/// OCR pages concurrently.
///
/// Advanced callers can use [`from_runtime`](Self::from_runtime) with an
/// already-built runtime, but then own the `oar-ocr` batch-size policy.
pub struct OarOcrEngine {
    runtime: Mutex<OAROCR>,
    tiling: Option<Tiling>,
}

const MAX_TILE_BATCH_SIZE: usize = 8;
const STITCH_MIN_VERTICAL_IOU: f32 = 0.5;
const STITCH_MIN_OVERLAP_CHARS: usize = 2;
const VERTICAL_LINE_X_TOLERANCE_PX: f32 = 1.0;
const VERTICAL_LINE_MIN_ADVANCE: f32 = 0.5;
const VERTICAL_LINE_MAX_ADVANCE: f32 = 1.25;
const MAX_VERTICAL_JOIN_LINES: usize = 3;

#[derive(Clone, Copy)]
struct Tiling {
    tile_px: u32,
    overlap_px: u32,
}

impl OarOcrEngine {
    /// Build an engine from detection, recognition, and dictionary artifacts.
    ///
    /// All three arguments accept local paths or in-memory bytes (models as
    /// [`ModelSource`], the dictionary as [`DictSource`]), so the whole pipeline
    /// can be embedded in the binary or loaded from disk. A dictionary path can
    /// be a bare registered file name when the `oar-ocr-auto-download` feature
    /// is enabled. Use [`from_builder`](Self::from_builder) to configure optional
    /// orientation, rectification, or model-specific settings.
    ///
    /// The recognizer and dictionary must match: a mismatched dictionary
    /// silently produces garbled text rather than an error. Prefer a preset such
    /// as [`ppocr_v6_tiny`](Self::ppocr_v6_tiny) when you want a known-good trio.
    pub fn from_models(
        text_detection_model: impl Into<ModelSource>,
        text_recognition_model: impl Into<ModelSource>,
        character_dict: impl Into<DictSource>,
    ) -> Result<Self, LiteParseError> {
        let dict = character_dict.into();
        let mut builder = OAROCRBuilder::new(
            text_detection_model,
            text_recognition_model,
            dict.builder_path(),
        );
        if let DictSource::Content(bytes) = dict {
            let content = String::from_utf8(bytes).map_err(|err| {
                LiteParseError::Ocr(format!("character dictionary is not valid UTF-8: {err}"))
            })?;
            builder = builder.character_dict_content(content);
        }
        Self::from_builder(builder)
    }

    /// Build the PP-OCRv6 Tiny pipeline, downloading the detection model,
    /// recognition model, and matching dictionary on first use.
    #[cfg(feature = "oar-ocr-auto-download")]
    pub fn ppocr_v6_tiny() -> Result<Self, LiteParseError> {
        Self::from_models(
            "pp-ocrv6_tiny_det.onnx",
            "pp-ocrv6_tiny_rec.onnx",
            "ppocrv6_tiny_dict.txt",
        )
    }

    /// Build the PP-OCRv6 **small** pipeline via auto-download.
    ///
    /// Requires the `oar-ocr-auto-download` feature. Trades throughput for
    /// higher accuracy than [`ppocr_v6_tiny`](Self::ppocr_v6_tiny); the small
    /// recognizer uses the full `ppocrv6_dict.txt`.
    #[cfg(feature = "oar-ocr-auto-download")]
    pub fn ppocr_v6_small() -> Result<Self, LiteParseError> {
        Self::from_models(
            "pp-ocrv6_small_det.onnx",
            "pp-ocrv6_small_rec.onnx",
            "ppocrv6_dict.txt",
        )
    }

    /// Build the PP-OCRv6 **medium** pipeline via auto-download.
    ///
    /// Requires the `oar-ocr-auto-download` feature. The most accurate (and
    /// heaviest) PP-OCRv6 configuration; like [`ppocr_v6_small`](Self::ppocr_v6_small)
    /// it uses the full `ppocrv6_dict.txt`.
    #[cfg(feature = "oar-ocr-auto-download")]
    pub fn ppocr_v6_medium() -> Result<Self, LiteParseError> {
        Self::from_models(
            "pp-ocrv6_medium_det.onnx",
            "pp-ocrv6_medium_rec.onnx",
            "ppocrv6_dict.txt",
        )
    }

    /// Build an engine with conservative batch defaults from a caller-configured
    /// `oar-ocr` pipeline.
    ///
    /// The adapter overrides `image_batch_size` and `region_batch_size` to one.
    /// LiteParse invokes the backend with one rendered page at a time, so image
    /// batching does not improve throughput here. A region batch of one avoids
    /// multiplying temporary recognition tensors on dense pages.
    pub fn from_builder(builder: OAROCRBuilder) -> Result<Self, LiteParseError> {
        let runtime = builder.image_batch_size(1).region_batch_size(1).build()?;
        Ok(Self::from_runtime(runtime))
    }

    /// Build an engine that detects text on overlapping image tiles.
    /// Tiling is opt-in; the ordinary constructor keeps its original behavior.
    pub fn from_builder_with_tiling(
        builder: OAROCRBuilder,
        tile_px: u32,
        overlap_px: u32,
    ) -> Result<Self, LiteParseError> {
        if tile_px == 0 || overlap_px >= tile_px {
            return Err(LiteParseError::Ocr(
                "tile size must be positive and overlap smaller than tile size".into(),
            ));
        }
        let runtime = builder
            .image_batch_size(MAX_TILE_BATCH_SIZE)
            .region_batch_size(1)
            .build()?;
        Ok(Self {
            runtime: Mutex::new(runtime),
            tiling: Some(Tiling {
                tile_px,
                overlap_px,
            }),
        })
    }

    /// Wrap an already-built `oar-ocr` runtime.
    ///
    /// Inference calls remain serialized, but the runtime's internal image and
    /// region batch sizes are preserved. Prefer [`from_builder`](Self::from_builder)
    /// unless larger batches have been measured against an explicit memory
    /// budget.
    pub fn from_runtime(runtime: OAROCR) -> Self {
        Self {
            runtime: Mutex::new(runtime),
            tiling: None,
        }
    }

    fn recognize_sync(
        &self,
        image_data: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<OcrResult>, Box<dyn std::error::Error + Send + Sync>> {
        let image = rgb_image(image_data, width, height)?;
        let runtime = self.runtime.lock().map_err(|_| {
            io::Error::other("oar-ocr runtime mutex was poisoned by a previous panic")
        })?;
        let Some(tiling) = self.tiling else {
            let mut predictions = runtime.predict(vec![image])?;
            let prediction = predictions.pop().ok_or_else(|| {
                io::Error::other("oar-ocr returned no prediction for a non-empty image batch")
            })?;
            return Ok(prediction
                .text_regions
                .into_iter()
                .filter_map(region_to_result)
                .collect());
        };
        let xs = tile_starts(width, tiling.tile_px, tiling.overlap_px);
        let ys = tile_starts(height, tiling.tile_px, tiling.overlap_px);
        let origins: Vec<(u32, u32)> = ys
            .iter()
            .flat_map(|y| xs.iter().map(move |x| (*x, *y)))
            .collect();
        let tiles = origins
            .iter()
            .map(|(x, y)| {
                image::imageops::crop_imm(
                    &image,
                    *x,
                    *y,
                    tiling.tile_px.min(width - *x),
                    tiling.tile_px.min(height - *y),
                )
                .to_image()
            })
            .collect();
        let predictions = runtime.predict(tiles)?;
        if predictions.len() != origins.len() {
            return Err(
                io::Error::other("oar-ocr returned the wrong number of tile predictions").into(),
            );
        }
        let mut results = Vec::new();
        for (index, prediction) in predictions.into_iter().enumerate() {
            let (x, y) = origins[index];
            let col = index % xs.len();
            let row = index / xs.len();
            let x_core = tile_core(&xs, col, tiling.tile_px, width);
            let y_core = tile_core(&ys, row, tiling.tile_px, height);
            for region in prediction.text_regions {
                if let Some(mut result) = region_to_result(region) {
                    shift_result(&mut result, x as f32, y as f32);
                    if core_contains(&result, x_core, y_core) {
                        results.push(result);
                    }
                }
            }
        }
        let seams: Vec<f32> = xs
            .windows(2)
            .map(|pair| (pair[0] + tiling.tile_px + pair[1]) as f32 / 2.0)
            .collect();
        Ok(join_vertical_lines(stitch_regions(
            results,
            &seams,
            tiling.overlap_px as f32,
        )))
    }
}

fn core_contains(result: &OcrResult, x_core: (f32, f32), y_core: (f32, f32)) -> bool {
    let cx = (result.bbox[0] + result.bbox[2]) / 2.0;
    let cy = (result.bbox[1] + result.bbox[3]) / 2.0;
    cx >= x_core.0 && cx < x_core.1 && cy >= y_core.0 && cy < y_core.1
}

fn tile_starts(extent: u32, tile: u32, overlap: u32) -> Vec<u32> {
    if extent <= tile {
        return vec![0];
    }
    let mut out = Vec::new();
    let mut start = 0;
    loop {
        if start + tile >= extent {
            let last = extent - tile;
            if out.last().copied() != Some(last) {
                out.push(last);
            }
            return out;
        }
        out.push(start);
        start += tile - overlap;
    }
}

fn tile_core(starts: &[u32], index: usize, tile: u32, extent: u32) -> (f32, f32) {
    let left = if index == 0 {
        0.0
    } else {
        (starts[index - 1] + tile + starts[index]) as f32 / 2.0
    };
    let right = if index + 1 == starts.len() {
        extent as f32
    } else {
        (starts[index] + tile + starts[index + 1]) as f32 / 2.0
    };
    (left, right)
}

fn shift_result(result: &mut OcrResult, dx: f32, dy: f32) {
    result.bbox[0] += dx;
    result.bbox[1] += dy;
    result.bbox[2] += dx;
    result.bbox[3] += dy;
    if let Some(polygon) = result.polygon.as_mut() {
        for point in polygon {
            point[0] += dx;
            point[1] += dy;
        }
    }
}

fn overlap_suffix_prefix(left: &str, right: &str) -> usize {
    let a: Vec<char> = left.chars().collect();
    let b: Vec<char> = right.chars().collect();
    for count in (STITCH_MIN_OVERLAP_CHARS..=a.len().min(b.len())).rev() {
        if a[a.len() - count..] == b[..count] {
            return count;
        }
    }
    0
}

fn vertical_iou(a: &OcrResult, b: &OcrResult) -> f32 {
    let intersection = (a.bbox[3].min(b.bbox[3]) - a.bbox[1].max(b.bbox[1])).max(0.0);
    let union = a.bbox[3].max(b.bbox[3]) - a.bbox[1].min(b.bbox[1]);
    if union <= 0.0 {
        0.0
    } else {
        intersection / union
    }
}

fn stitch_regions(mut regions: Vec<OcrResult>, seams: &[f32], overlap: f32) -> Vec<OcrResult> {
    regions.sort_by(|a, b| a.bbox[0].total_cmp(&b.bbox[0]));
    let mut i = 0;
    while i < regions.len() {
        let mut j = i + 1;
        while j < regions.len() {
            let a = &regions[i];
            let b = &regions[j];
            let near_seam = seams.iter().any(|seam| {
                a.bbox[0] < *seam
                    && b.bbox[2] > *seam
                    && (a.bbox[2] - seam).abs() <= overlap
                    && (b.bbox[0] - seam).abs() <= overlap
            });
            let shared = if near_seam && vertical_iou(a, b) >= STITCH_MIN_VERTICAL_IOU {
                overlap_suffix_prefix(&a.text, &b.text)
            } else {
                0
            };
            if shared >= STITCH_MIN_OVERLAP_CHARS {
                let right = regions.remove(j);
                let left = &mut regions[i];
                let right_chars: Vec<char> = right.text.chars().collect();
                let left_chars = left.text.chars().count();
                left.text.extend(right_chars[shared..].iter().copied());
                let unique_right_chars = right_chars.len() - shared;
                let total_chars = left_chars + unique_right_chars;
                left.confidence = (left.confidence * left_chars as f32
                    + right.confidence * unique_right_chars as f32)
                    / total_chars as f32;
                left.bbox = [
                    left.bbox[0].min(right.bbox[0]),
                    left.bbox[1].min(right.bbox[1]),
                    left.bbox[2].max(right.bbox[2]),
                    left.bbox[3].max(right.bbox[3]),
                ];
                left.polygon = None;
            } else {
                j += 1;
            }
        }
        i += 1;
    }
    regions
}

fn join_vertical_lines(mut regions: Vec<OcrResult>) -> Vec<OcrResult> {
    regions.sort_by(|a, b| {
        a.bbox[1]
            .total_cmp(&b.bbox[1])
            .then_with(|| a.bbox[0].total_cmp(&b.bbox[0]))
    });
    let mut joined: Vec<(OcrResult, f32, f32, usize, usize)> = Vec::with_capacity(regions.len());
    for region in regions {
        let has_words = |text: &str| {
            text.chars().any(char::is_alphabetic) && text.chars().any(char::is_whitespace)
        };
        let previous_line =
            joined
                .iter_mut()
                .rev()
                .find(|(previous, last_y, last_height, _, lines)| {
                    let y_advance = region.bbox[1] - *last_y;
                    let same_left_edge =
                        (region.bbox[0] - previous.bbox[0]).abs() <= VERTICAL_LINE_X_TOLERANCE_PX;
                    let consecutive_lines = *last_height > 0.0
                        && y_advance >= *last_height * VERTICAL_LINE_MIN_ADVANCE
                        && y_advance <= *last_height * VERTICAL_LINE_MAX_ADVANCE;
                    same_left_edge
                        && consecutive_lines
                        && *lines < MAX_VERTICAL_JOIN_LINES
                        && has_words(&previous.text)
                        && has_words(&region.text)
                });
        if let Some((previous, last_y, last_height, previous_chars, lines)) = previous_line {
            let next_chars = region.text.chars().count();
            previous.confidence = (previous.confidence * *previous_chars as f32
                + region.confidence * next_chars as f32)
                / (*previous_chars + next_chars) as f32;
            previous.text.push(' ');
            previous.text.push_str(&region.text);
            previous.bbox[0] = previous.bbox[0].min(region.bbox[0]);
            previous.bbox[2] = previous.bbox[2].max(region.bbox[2]);
            previous.bbox[3] = previous.bbox[3].max(region.bbox[3]);
            previous.polygon = None;
            *last_y = region.bbox[1];
            *last_height = region.bbox[3] - region.bbox[1];
            *previous_chars += next_chars;
            *lines += 1;
            continue;
        }
        let height = region.bbox[3] - region.bbox[1];
        let y = region.bbox[1];
        let chars = region.text.chars().count();
        joined.push((region, y, height, chars, 1));
    }
    let mut results: Vec<OcrResult> = joined
        .into_iter()
        .map(|(region, _, _, _, _)| region)
        .collect();
    results.sort_by(|a, b| {
        a.bbox[0]
            .total_cmp(&b.bbox[0])
            .then_with(|| a.bbox[1].total_cmp(&b.bbox[1]))
    });
    results
}

impl OcrEngine for OarOcrEngine {
    fn name(&self) -> &str {
        "oar-ocr"
    }

    fn recognize<'a, 'b: 'a, 'c: 'a>(
        &'a self,
        image_data: &'c [u8],
        width: u32,
        height: u32,
        options: &'b OcrOptions,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<OcrResult>, Box<dyn std::error::Error + Send + Sync>>>
                + Send
                + '_,
        >,
    > {
        warn_language_ignored_once(&options.language);
        // `ocr_and_merge_rendered` polls this future on a blocking worker, so
        // synchronous ONNX inference does not occupy an async runtime worker.
        Box::pin(async move { self.recognize_sync(image_data, width, height) })
    }
}

static LANGUAGE_IGNORED_WARNING: Once = Once::new();

/// Warn once per process that this backend ignores `OcrOptions::language`.
fn warn_language_ignored_once(language: &str) {
    if language.is_empty() {
        return;
    }
    LANGUAGE_IGNORED_WARNING.call_once(|| {
        eprintln!(
            "[oar-ocr] ignoring OcrOptions::language ({language:?}); recognition language is fixed by the model and character dictionary"
        );
    });
}

fn rgb_image(image_data: &[u8], width: u32, height: u32) -> Result<image::RgbImage, io::Error> {
    if width == 0 || height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid zero-sized RGB image: {width}x{height}"),
        ));
    }

    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("RGB image dimensions overflow: {width}x{height}"),
            )
        })?;

    if image_data.len() != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "invalid RGB buffer length for {width}x{height}: expected {expected_len} bytes, got {}",
                image_data.len()
            ),
        ));
    }

    image::RgbImage::from_raw(width, height, image_data.to_vec()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("failed to construct RGB image from {width}x{height} buffer"),
        )
    })
}

fn region_to_result(region: oar_ocr::oarocr::TextRegion) -> Option<OcrResult> {
    let text = region.text?.trim().to_owned();
    let confidence = region.confidence?;
    if text.is_empty() || !confidence.is_finite() {
        return None;
    }

    let points = &region.bounding_box.points;
    if points.is_empty()
        || points
            .iter()
            .any(|point| !point.x.is_finite() || !point.y.is_finite())
    {
        return None;
    }

    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for point in points {
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }

    let polygon = match points.as_slice() {
        [a, b, c, d] => Some([[a.x, a.y], [b.x, b.y], [c.x, c.y], [d.x, d.y]]),
        _ => None,
    };

    Some(OcrResult {
        text,
        bbox: [min_x, min_y, max_x, max_y],
        confidence: confidence.clamp(0.0, 1.0),
        polygon,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use oar_ocr::oarocr::TextRegion;
    use oar_ocr::processors::{BoundingBox, Point};

    use super::*;

    #[tokio::test]
    #[ignore = "requires LITEPARSE_OAR_MODELS_DIR with local detector, recognizer and dictionary"]
    async fn local_models_recognize_receipt_with_and_without_tiling() {
        const TILE_PX: u32 = 320;
        const OVERLAP_PX: u32 = 96;
        const RECEIPT_DPI: f32 = 200.0;
        let models = PathBuf::from(
            std::env::var_os("LITEPARSE_OAR_MODELS_DIR")
                .expect("set LITEPARSE_OAR_MODELS_DIR to the local model directory"),
        );
        let image = image::open(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../integration_tests_data/receipt.png"),
        )
        .unwrap()
        .to_rgb8();
        let options = OcrOptions {
            language: "eng".into(),
            dpi: RECEIPT_DPI,
        };
        for tiled in [false, true] {
            let detector = models.join("det.onnx");
            let recognizer = models.join("rec.onnx");
            let dictionary = models.join("ppocrv6_dict.txt");
            let engine = if tiled {
                OarOcrEngine::from_builder_with_tiling(
                    OAROCRBuilder::new(detector, recognizer, dictionary),
                    TILE_PX,
                    OVERLAP_PX,
                )
            } else {
                OarOcrEngine::from_models(detector, recognizer, dictionary)
            }
            .unwrap();
            let results = engine
                .recognize(image.as_raw(), image.width(), image.height(), &options)
                .await
                .unwrap();
            let text = results
                .iter()
                .map(|result| result.text.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            for word in ["SHOP", "GROSS", "SUM"] {
                assert!(text.contains(word), "tiled={tiled}: missing {word}: {text}");
            }
            for result in results {
                assert!((0.0..=1.0).contains(&result.confidence));
                let [x0, y0, x1, y1] = result.bbox;
                assert!(0.0 <= x0 && x0 < x1 && x1 <= image.width() as f32);
                assert!(0.0 <= y0 && y0 < y1 && y1 <= image.height() as f32);
            }
        }
    }

    fn region(text: &str, bbox: [f32; 4], confidence: f32) -> OcrResult {
        OcrResult {
            text: text.into(),
            bbox,
            confidence,
            polygon: None,
        }
    }

    #[test]
    fn tiles_cover_short_and_long_edges_without_gaps() {
        assert_eq!(tile_starts(700, 960, 96), vec![0]);
        let starts = tile_starts(2000, 960, 96);
        assert_eq!(starts, vec![0, 864, 1040]);
        assert_eq!(tile_core(&starts, 0, 960, 2000), (0.0, 912.0));
        assert_eq!(tile_core(&starts, 2, 960, 2000), (1432.0, 2000.0));
        assert_eq!(starts.last().copied().unwrap() + 960, 2000);
    }

    #[test]
    fn offsets_boxes_and_polygons_to_page_coordinates() {
        let mut result = region("note", [1.0, 2.0, 11.0, 12.0], 0.8);
        result.polygon = Some([[1.0, 2.0], [11.0, 2.0], [11.0, 12.0], [1.0, 12.0]]);
        shift_result(&mut result, 100.0, 200.0);
        assert_eq!(result.bbox, [101.0, 202.0, 111.0, 212.0]);
        assert_eq!(result.polygon.unwrap()[0], [101.0, 202.0]);
    }

    #[test]
    fn keeps_a_region_straddling_a_shared_core_boundary_exactly_once() {
        const TILE: u32 = 960;
        const OVERLAP: u32 = 96;
        const EXTENT: u32 = 2000;
        const HALF_WIDTH: f32 = 10.0;
        const Y: f32 = 100.0;
        const HEIGHT: f32 = 20.0;
        let starts = tile_starts(EXTENT, TILE, OVERLAP);
        let boundary = tile_core(&starts, 0, TILE, EXTENT).1;
        for centre in [boundary - 1.0, boundary, boundary + 1.0] {
            let mut kept = Vec::new();
            for (index, origin) in starts.iter().enumerate() {
                let local_centre = centre - *origin as f32;
                let mut result = region(
                    "boundary text",
                    [
                        local_centre - HALF_WIDTH,
                        Y,
                        local_centre + HALF_WIDTH,
                        Y + HEIGHT,
                    ],
                    0.95,
                );
                shift_result(&mut result, *origin as f32, 0.0);
                if core_contains(
                    &result,
                    tile_core(&starts, index, TILE, EXTENT),
                    (0.0, EXTENT as f32),
                ) {
                    kept.push(result);
                }
            }
            assert_eq!(kept.len(), 1, "centre={centre}");
            assert_eq!(
                kept[0].bbox,
                [centre - HALF_WIDTH, Y, centre + HALF_WIDTH, Y + HEIGHT]
            );
        }
    }

    #[test]
    fn preserves_seam_regions_without_a_suffix_prefix_match() {
        let results = stitch_regions(
            vec![
                region("NOTES", [860.0, 100.0, 920.0, 120.0], 0.9),
                region("DETAILS", [905.0, 102.0, 980.0, 121.0], 0.8),
            ],
            &[912.0],
            96.0,
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].text, "NOTES");
        assert_eq!(results[1].text, "DETAILS");
    }

    #[test]
    fn stitches_text_split_across_a_vertical_tile_seam() {
        let results = stitch_regions(
            vec![
                region("NOTES AND", [860.0, 100.0, 920.0, 120.0], 0.9),
                region("AND DETAILS", [905.0, 102.0, 980.0, 121.0], 0.8),
                region("OTHER", [905.0, 140.0, 960.0, 160.0], 0.7),
            ],
            &[912.0],
            96.0,
        );
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].text, "NOTES AND DETAILS");
        assert_eq!(results[0].bbox, [860.0, 100.0, 980.0, 121.0]);
    }

    #[test]
    fn joins_adjacent_ocr_lines_without_crossing_columns_or_numeric_callouts() {
        let joined = join_vertical_lines(vec![
            region("load walls.", [99.8, 111.0, 140.0, 121.0], 0.8),
            region("6,472", [100.0, 122.0, 130.0, 132.0], 0.9),
            region(
                "slab thickenings not required for internal",
                [100.0, 101.0, 250.0, 112.0],
                0.9,
            ),
            region("nearby column", [150.0, 105.0, 190.0, 115.0], 0.8),
            region("other note", [200.0, 111.0, 250.0, 121.0], 0.7),
        ]);
        assert_eq!(joined.len(), 4);
        assert_eq!(
            joined[0].text,
            "slab thickenings not required for internal load walls."
        );
        assert_eq!(joined[0].bbox, [99.8, 101.0, 250.0, 121.0]);
        assert_eq!(joined[1].text, "6,472");
        assert_eq!(joined[2].text, "nearby column");
        assert_eq!(joined[3].text, "other note");
    }

    #[test]
    fn bounds_vertical_join_to_preserve_line_location() {
        let joined = join_vertical_lines(vec![
            region("first line", [100.0, 100.0, 150.0, 111.0], 0.9),
            region("second line", [100.0, 110.0, 150.0, 121.0], 0.9),
            region("third line", [100.0, 120.0, 150.0, 131.0], 0.9),
            region("fourth line", [100.0, 130.0, 150.0, 141.0], 0.9),
        ]);
        assert_eq!(joined.len(), 2);
        assert_eq!(joined[0].text, "first line second line third line");
        assert_eq!(joined[0].bbox, [100.0, 100.0, 150.0, 131.0]);
        assert_eq!(joined[1].text, "fourth line");
    }

    #[test]
    fn rejects_non_rgb_buffers() {
        let error = rgb_image(&[0; 11], 2, 2).unwrap_err();
        assert!(error.to_string().contains("expected 12 bytes, got 11"));
    }

    #[test]
    fn maps_text_confidence_bbox_and_quad() {
        let points = vec![
            Point::new(10.0, 5.0),
            Point::new(30.0, 7.0),
            Point::new(28.0, 20.0),
            Point::new(8.0, 18.0),
        ];
        let region = TextRegion::with_recognition(
            BoundingBox::new(points),
            Some(Arc::<str>::from(" hello ")),
            Some(1.2),
        );

        let result = region_to_result(region).unwrap();
        assert_eq!(result.text, "hello");
        assert_eq!(result.bbox, [8.0, 5.0, 30.0, 20.0]);
        assert_eq!(result.confidence, 1.0);
        assert_eq!(
            result.polygon,
            Some([[10.0, 5.0], [30.0, 7.0], [28.0, 20.0], [8.0, 18.0]])
        );
    }

    #[test]
    fn dict_source_paths_vs_bytes() {
        // String/path inputs are dictionary paths and reach the builder verbatim.
        assert!(matches!(DictSource::from("dict.txt"), DictSource::Path(_)));
        assert_eq!(
            DictSource::from(PathBuf::from("dict.txt")).builder_path(),
            PathBuf::from("dict.txt")
        );
        // Byte inputs are in-memory content; the builder path is an unused
        // placeholder because `character_dict_content` takes precedence.
        let content = DictSource::from(b"a\nb\n".to_vec());
        assert!(matches!(content, DictSource::Content(_)));
        assert_eq!(content.builder_path(), PathBuf::new());
    }

    #[test]
    fn drops_unrecognized_or_invalid_regions() {
        let box_ = BoundingBox::from_coords(0.0, 0.0, 10.0, 10.0);
        assert!(region_to_result(TextRegion::new(box_.clone())).is_none());
        assert!(
            region_to_result(TextRegion::with_recognition(
                box_.clone(),
                Some(Arc::<str>::from("   ")),
                Some(0.9),
            ))
            .is_none()
        );
        assert!(
            region_to_result(TextRegion::with_recognition(
                box_,
                Some(Arc::<str>::from("text")),
                Some(f32::NAN),
            ))
            .is_none()
        );
    }
}
