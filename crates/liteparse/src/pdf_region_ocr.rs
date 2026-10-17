//! Bounded OCR crops from one page render, using the existing local OCR backend.
use image::RgbImage;
use serde::{Deserialize, Serialize};
use std::{error::Error, io};

const MAX_REGIONS: usize = 256;
const CROP_BATCH: usize = 8;
const MAX_CROP_PIXELS: u64 = 8_000_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecognitionOptions {
    pub regions: Vec<Region>,
    pub detector: String,
    pub recognizer: String,
    pub dictionary: String,
    pub threads: usize,
    pub min_confidence: f32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Region {
    /// Page fractions [left, top, right, bottom].
    pub bbox: [f64; 4],
    /// Clockwise rotation required to make the source crop upright.
    pub rotation: u16,
}

#[derive(Debug, Serialize)]
pub(super) struct RecognitionResult {
    pub dpi: u32,
    pub page_width_pts: f64,
    pub page_height_pts: f64,
    pub regions: Vec<RegionResult>,
}

#[derive(Debug, Serialize)]
pub(super) struct RegionResult {
    pub index: usize,
    pub requested: Region,
    /// Pixel-aligned actual crop, expressed in source page points.
    pub source_bbox: [f64; 4],
    pub tokens: Vec<Token>,
}

#[derive(Debug, Serialize)]
pub(super) struct Token {
    pub text: String,
    pub confidence: f32,
    pub bbox: [f64; 4],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refinement: Option<Refinement>,
}

#[derive(Debug, Serialize)]
pub(super) struct Refinement {
    pub previous_text: String,
    pub previous_confidence: f32,
    pub previous_bbox: [f64; 4],
    pub requested: Region,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

pub(super) fn validate(options: &RecognitionOptions) -> Result<(), io::Error> {
    if options.regions.is_empty() || options.regions.len() > MAX_REGIONS {
        return Err(invalid("recognition needs 1..=256 regions"));
    }
    if options.threads == 0
        || options.threads > 32
        || !options.min_confidence.is_finite()
        || !(0.0..=1.0).contains(&options.min_confidence)
    {
        return Err(invalid("invalid OCR threads or confidence"));
    }
    for region in &options.regions {
        let [x0, y0, x1, y1] = region.bbox;
        if !region.bbox.iter().all(|v| v.is_finite())
            || x0 < 0.0
            || y0 < 0.0
            || x1 > 1.0
            || y1 > 1.0
            || x1 <= x0
            || y1 <= y0
            || ![0, 90, 180, 270].contains(&region.rotation)
        {
            return Err(invalid("invalid OCR crop or rotation"));
        }
    }
    Ok(())
}

fn source_point(x: f64, y: f64, width: f64, height: f64, rotation: u16) -> (f64, f64) {
    match rotation {
        90 => (y, height - x),
        180 => (width - x, height - y),
        270 => (width - y, x),
        _ => (x, y),
    }
}

#[cfg(feature = "oar-ocr")]
pub(super) fn recognize(
    page: RgbImage,
    options: &RecognitionOptions,
    width_pts: f64,
    height_pts: f64,
    dpi: u32,
) -> Result<RecognitionResult, Box<dyn Error>> {
    use liteparse::ocr::oar::{OAROCRBuilder, OarOcrEngine};
    use oar_ocr::core::config::OrtSessionConfig;
    validate(options)?;
    let engine = OarOcrEngine::from_builder(
        OAROCRBuilder::new(
            options.detector.as_str(),
            options.recognizer.as_str(),
            options.dictionary.as_str(),
        )
        .ort_session(OrtSessionConfig::new().with_intra_threads(options.threads)),
    )?;
    let mut output = RecognitionResult {
        dpi,
        page_width_pts: width_pts,
        page_height_pts: height_pts,
        regions: Vec::new(),
    };
    let sx = width_pts / f64::from(page.width());
    let sy = height_pts / f64::from(page.height());
    for (batch_index, batch) in options.regions.chunks(CROP_BATCH).enumerate() {
        let mut images = Vec::with_capacity(batch.len());
        let mut bounds = Vec::with_capacity(batch.len());
        for region in batch {
            let (crop, source_bounds) = crop_region(&page, region)?;
            images.push(crop);
            bounds.push(source_bounds);
        }
        for (index, tokens) in engine.recognize_images(images)?.into_iter().enumerate() {
            let region = &batch[index];
            let (left, top, width, height) = bounds[index];
            let mut result = RegionResult {
                index: batch_index * CROP_BATCH + index,
                requested: region.clone(),
                source_bbox: [
                    f64::from(left) * sx,
                    f64::from(top) * sy,
                    f64::from(left + width) * sx,
                    f64::from(top + height) * sy,
                ],
                tokens: Vec::new(),
            };
            for token in tokens {
                if token.confidence < options.min_confidence {
                    continue;
                }
                result.tokens.push(Token {
                    refinement: None,
                    text: token.text,
                    confidence: token.confidence,
                    bbox: token_source_bbox(
                        token.bbox,
                        (left, top, width, height),
                        region.rotation,
                        sx,
                        sy,
                    ),
                });
            }
            output.regions.push(result);
        }
    }
    refine_vertical_numbers(&page, &engine, &mut output)?;
    Ok(output)
}

#[cfg(feature = "oar-ocr")]
fn refine_vertical_numbers(
    page: &RgbImage,
    engine: &liteparse::ocr::oar::OarOcrEngine,
    output: &mut RecognitionResult,
) -> Result<(), Box<dyn Error>> {
    const MIN_IMPROVEMENT: f32 = 0.05;
    const MAX_REFINEMENTS: usize = 128;
    const PADDING_PTS: f64 = 3.0;
    let mut requests = Vec::new();
    for (region_index, region) in output.regions.iter().enumerate() {
        for (token_index, token) in region.tokens.iter().enumerate() {
            let b = token.bbox;
            let numeric = token
                .text
                .chars()
                .all(|c| c.is_ascii_digit() || ".,-".contains(c));
            if !numeric
                || token.confidence >= 0.98
                || b[3] - b[1] < (b[2] - b[0]) * 1.4
                || requests.len() >= MAX_REFINEMENTS
            {
                continue;
            }
            let bounds = [
                ((b[0] - PADDING_PTS) / output.page_width_pts).max(0.0),
                ((b[1] - PADDING_PTS) / output.page_height_pts).max(0.0),
                ((b[2] + PADDING_PTS) / output.page_width_pts).min(1.0),
                ((b[3] + PADDING_PTS) / output.page_height_pts).min(1.0),
            ];
            for rotation in [90, 270] {
                requests.push((
                    region_index,
                    token_index,
                    Region {
                        bbox: bounds,
                        rotation,
                    },
                ));
            }
        }
    }
    for batch in requests.chunks(CROP_BATCH) {
        let mut images = Vec::new();
        let mut source_bounds = Vec::new();
        for (_, _, region) in batch {
            let (image, bounds) = crop_region(page, region)?;
            images.push(image);
            source_bounds.push(bounds);
        }
        for (index, ((region_index, token_index, region), results)) in batch
            .iter()
            .zip(engine.recognize_images(images)?)
            .enumerate()
        {
            if results.len() != 1 {
                continue;
            }
            let reading = &results[0];
            let original = &mut output.regions[*region_index].tokens[*token_index];
            if reading.confidence < original.confidence + MIN_IMPROVEMENT
                || !reading
                    .text
                    .chars()
                    .all(|c| c.is_ascii_digit() || ".,-".contains(c))
            {
                continue;
            }
            let previous_text = original.text.clone();
            let previous_confidence = original.confidence;
            let previous_bbox = original.bbox;
            original.bbox = token_source_bbox(
                reading.bbox,
                source_bounds[index],
                region.rotation,
                output.page_width_pts / f64::from(page.width()),
                output.page_height_pts / f64::from(page.height()),
            );
            original.text = reading.text.clone();
            original.confidence = reading.confidence;
            original.refinement = Some(Refinement {
                previous_text,
                previous_confidence,
                previous_bbox,
                requested: region.clone(),
            });
        }
    }
    Ok(())
}

fn token_source_bbox(
    bbox: [f32; 4],
    bounds: (u32, u32, u32, u32),
    rotation: u16,
    sx: f64,
    sy: f64,
) -> [f64; 4] {
    let (left, top, width, height) = bounds;
    let [x0, y0, x1, y1] = bbox.map(f64::from);
    let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
        .map(|(x, y)| source_point(x, y, f64::from(width), f64::from(height), rotation));
    let box_points = [
        corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min),
        corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
        corners
            .iter()
            .map(|p| p.0)
            .fold(f64::NEG_INFINITY, f64::max),
        corners
            .iter()
            .map(|p| p.1)
            .fold(f64::NEG_INFINITY, f64::max),
    ];
    [
        (f64::from(left) + box_points[0]) * sx,
        (f64::from(top) + box_points[1]) * sy,
        (f64::from(left) + box_points[2]) * sx,
        (f64::from(top) + box_points[3]) * sy,
    ]
}

fn crop_region(
    page: &RgbImage,
    region: &Region,
) -> Result<(RgbImage, (u32, u32, u32, u32)), Box<dyn Error>> {
    let [x0, y0, x1, y1] = region.bbox;
    let left = (x0 * f64::from(page.width())).floor() as u32;
    let top = (y0 * f64::from(page.height())).floor() as u32;
    let right = ((x1 * f64::from(page.width())).ceil() as u32).min(page.width());
    let bottom = ((y1 * f64::from(page.height())).ceil() as u32).min(page.height());
    let (width, height) = (right - left, bottom - top);
    if u64::from(width) * u64::from(height) > MAX_CROP_PIXELS {
        return Err(invalid("OCR region exceeds pixel budget; split the requested region").into());
    }
    let crop = image::imageops::crop_imm(page, left, top, width, height).to_image();
    let upright = match region.rotation {
        90 => image::imageops::rotate90(&crop),
        180 => image::imageops::rotate180(&crop),
        270 => image::imageops::rotate270(&crop),
        _ => crop,
    };
    Ok((upright, (left, top, width, height)))
}

#[cfg(not(feature = "oar-ocr"))]
pub(super) fn recognize(
    _page: RgbImage,
    _options: &RecognitionOptions,
    _width_pts: f64,
    _height_pts: f64,
    _dpi: u32,
) -> Result<RecognitionResult, Box<dyn Error>> {
    Err(invalid("recognize_regions requires the oar-ocr build feature").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crop_rotation_maps_back_to_source() {
        assert_eq!(source_point(2.0, 3.0, 20.0, 10.0, 90), (3.0, 8.0));
        assert_eq!(source_point(2.0, 3.0, 20.0, 10.0, 270), (17.0, 2.0));
        assert_eq!(source_point(2.0, 3.0, 20.0, 10.0, 180), (18.0, 7.0));
    }
}
