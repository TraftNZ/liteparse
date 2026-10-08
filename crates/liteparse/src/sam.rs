//! Point-prompted image segmentation with SAM 2.1 (Hiera-tiny, ONNX).
//!
//! `lit sam` reads one raster, runs the SAM 2.1 image encoder once and the mask
//! decoder on the given point prompts, and writes the mask as an 8-bit PNG the
//! size of the input (255 inside, 0 outside). The model only proposes the
//! region; the caller measures it against the page's own line work.
//!
//! Model interface (cstr/sam2.1-hiera-tiny-ONNX, Apache-2.0, opset 17):
//! - encoder `image` f32 [1,3,1024,1024] (RGB/255, resized to 1024x1024 without
//!   keeping the aspect ratio, ImageNet mean/std) gives `image_embed`
//!   [1,256,64,64], `high_res_0` [1,32,256,256], `high_res_1` [1,64,128,128].
//! - decoder takes those three plus `point_coords` f32 [1,N,2] (pixels in the
//!   1024 frame) and `point_labels` i64 [1,N] (1 positive, 0 negative) and
//!   gives `mask_logits` [1,4,256,256] and `iou` [1,4]; index 0 is the
//!   single-mask output.

use std::path::{Path, PathBuf};

use image::imageops::FilterType;
use image::{GrayImage, Luma};
use ort::session::Session;
use ort::value::Tensor;

const ENCODER_FILE: &str = "encoder.onnx";
const DECODER_FILE: &str = "decoder.onnx";
/// Side of the square frame the encoder reads (model.json `image_size`).
const MODEL_SIDE: u32 = 1024;
/// Side of the decoder's mask logits (model.json `mask_size`).
const MASK_SIDE: usize = 256;
const IMAGENET_MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const IMAGENET_STD: [f32; 3] = [0.229, 0.224, 0.225];
/// Mask logits above zero are inside the object (SAM convention).
const MASK_LOGIT_THRESHOLD: f32 = 0.0;
const MASK_INSIDE: u8 = 255;
const LABEL_POSITIVE: i64 = 1;
const LABEL_NEGATIVE: i64 = 0;
/// Index of the decoder's single-mask output among its four candidates.
const SINGLE_MASK_INDEX: usize = 0;
const MASK_CANDIDATES: usize = 4;

/// One click on the raster, in its pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointPrompt {
    pub x: f32,
    pub y: f32,
    pub positive: bool,
}

#[derive(Debug)]
pub struct SamError(String);

impl std::fmt::Display for SamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SamError {}

fn fail<T>(message: impl Into<String>) -> Result<T, SamError> {
    Err(SamError(message.into()))
}

fn ort_err(context: &str, err: impl std::fmt::Display) -> SamError {
    SamError(format!("{context}: {err}"))
}

/// Parses `x,y,label;x,y,label` where label is 1 (positive) or 0 (negative).
pub fn parse_points(spec: &str) -> Result<Vec<PointPrompt>, SamError> {
    let mut points = Vec::new();
    for part in spec.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let fields: Vec<&str> = part.split(',').map(str::trim).collect();
        if fields.len() != 3 {
            return fail(format!("point {part:?} is not x,y,label"));
        }
        let x: f32 = fields[0]
            .parse()
            .map_err(|_| SamError(format!("point {part:?}: x is not a number")))?;
        let y: f32 = fields[1]
            .parse()
            .map_err(|_| SamError(format!("point {part:?}: y is not a number")))?;
        let positive = match fields[2] {
            "1" => true,
            "0" => false,
            other => return fail(format!("point {part:?}: label {other:?} is not 1 or 0")),
        };
        if !x.is_finite() || !y.is_finite() {
            return fail(format!("point {part:?}: coordinates must be finite"));
        }
        points.push(PointPrompt { x, y, positive });
    }
    if !points.iter().any(|p| p.positive) {
        return fail("at least one positive point is required");
    }
    Ok(points)
}

/// Segments `image_path` once per prompt set and returns one mask per set at
/// the image size. The encoder runs once; only the decoder repeats.
pub fn segment(
    image_path: &Path,
    models_dir: &Path,
    prompt_sets: &[Vec<PointPrompt>],
    threads: usize,
) -> Result<Vec<GrayImage>, SamError> {
    let rgb = image::open(image_path)
        .map_err(|e| SamError(format!("read {}: {e}", image_path.display())))?
        .to_rgb8();
    let (width, height) = rgb.dimensions();
    if width == 0 || height == 0 {
        return fail("image has no pixels");
    }
    for p in prompt_sets.iter().flatten() {
        if p.x < 0.0 || p.y < 0.0 || p.x > width as f32 || p.y > height as f32 {
            return fail(format!(
                "point ({}, {}) is outside the {width}x{height} image",
                p.x, p.y
            ));
        }
    }

    let input = preprocess(&rgb);
    let mut encoder = load_session(&models_dir.join(ENCODER_FILE), threads)?;
    let mut decoder = load_session(&models_dir.join(DECODER_FILE), threads)?;

    let image_tensor = Tensor::from_array((
        [1usize, 3, MODEL_SIDE as usize, MODEL_SIDE as usize],
        input,
    ))
    .map_err(|e| ort_err("image tensor", e))?;
    let encoded = encoder
        .run(ort::inputs!["image" => image_tensor])
        .map_err(|e| ort_err("encoder run", e))?;

    let mut masks = Vec::with_capacity(prompt_sets.len());
    for points in prompt_sets {
        masks.push(decode_one(&mut decoder, &encoded, points, width, height)?);
    }
    Ok(masks)
}

fn decode_one(
    decoder: &mut Session,
    encoded: &ort::session::SessionOutputs<'_>,
    points: &[PointPrompt],
    width: u32,
    height: u32,
) -> Result<GrayImage, SamError> {
    let frame = MODEL_SIDE as f32;
    let mut coords = Vec::with_capacity(points.len() * 2);
    let mut labels = Vec::with_capacity(points.len());
    for p in points {
        coords.push(p.x * frame / width as f32);
        coords.push(p.y * frame / height as f32);
        labels.push(if p.positive { LABEL_POSITIVE } else { LABEL_NEGATIVE });
    }
    let n = points.len();
    let decoded = decoder
        .run(ort::inputs![
            "image_embed" => copy_f32(encoded, "image_embed")?,
            "high_res_0" => copy_f32(encoded, "high_res_0")?,
            "high_res_1" => copy_f32(encoded, "high_res_1")?,
            "point_coords" => Tensor::from_array(([1usize, n, 2], coords))
                .map_err(|e| ort_err("point_coords tensor", e))?,
            "point_labels" => Tensor::from_array(([1usize, n], labels))
                .map_err(|e| ort_err("point_labels tensor", e))?,
        ])
        .map_err(|e| ort_err("decoder run", e))?;

    let (shape, logits) = decoded["mask_logits"]
        .try_extract_tensor::<f32>()
        .map_err(|e| ort_err("mask_logits", e))?;
    let plane = MASK_SIDE * MASK_SIDE;
    if logits.len() != MASK_CANDIDATES * plane {
        return fail(format!(
            "mask_logits has shape {shape:?}, want [1,{MASK_CANDIDATES},{MASK_SIDE},{MASK_SIDE}]"
        ));
    }
    let single = &logits[SINGLE_MASK_INDEX * plane..(SINGLE_MASK_INDEX + 1) * plane];
    Ok(upsample_mask(single, width, height))
}

fn load_session(path: &Path, threads: usize) -> Result<Session, SamError> {
    let builder = Session::builder().map_err(|e| ort_err("session builder", e))?;
    let mut builder = if threads > 0 {
        builder
            .with_intra_threads(threads)
            .map_err(|e| ort_err("session threads", e))?
    } else {
        builder
    };
    builder
        .commit_from_file(path)
        .map_err(|e| ort_err(&format!("load {}", path.display()), e))
}

fn copy_f32(
    outputs: &ort::session::SessionOutputs<'_>,
    name: &str,
) -> Result<Tensor<f32>, SamError> {
    let (shape, data) = outputs[name]
        .try_extract_tensor::<f32>()
        .map_err(|e| ort_err(name, e))?;
    let dims: Vec<usize> = shape.iter().map(|d| *d as usize).collect();
    Tensor::from_array((dims, data.to_vec())).map_err(|e| ort_err(name, e))
}

/// RGB/255, resized to the model frame, ImageNet-normalised, planar CHW.
fn preprocess(rgb: &image::RgbImage) -> Vec<f32> {
    let resized = image::imageops::resize(rgb, MODEL_SIDE, MODEL_SIDE, FilterType::Triangle);
    let plane = (MODEL_SIDE * MODEL_SIDE) as usize;
    let mut out = vec![0f32; 3 * plane];
    for (i, px) in resized.pixels().enumerate() {
        for c in 0..3 {
            out[c * plane + i] = (px[c] as f32 / 255.0 - IMAGENET_MEAN[c]) / IMAGENET_STD[c];
        }
    }
    out
}

/// Bilinear (align_corners=false) enlargement of the 256x256 logits to the
/// image size, thresholded at zero.
fn upsample_mask(logits: &[f32], width: u32, height: u32) -> GrayImage {
    let side = MASK_SIDE as f32;
    let last = MASK_SIDE - 1;
    let sample = |sx: f32, sy: f32| -> f32 {
        let x = sx.clamp(0.0, last as f32);
        let y = sy.clamp(0.0, last as f32);
        let (x0, y0) = (x.floor() as usize, y.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(last), (y0 + 1).min(last));
        let (fx, fy) = (x - x0 as f32, y - y0 as f32);
        let at = |xx: usize, yy: usize| logits[yy * MASK_SIDE + xx];
        let top = at(x0, y0) * (1.0 - fx) + at(x1, y0) * fx;
        let bottom = at(x0, y1) * (1.0 - fx) + at(x1, y1) * fx;
        top * (1.0 - fy) + bottom * fy
    };
    GrayImage::from_fn(width, height, |x, y| {
        let sx = (x as f32 + 0.5) * side / width as f32 - 0.5;
        let sy = (y as f32 + 0.5) * side / height as f32 - 0.5;
        Luma([if sample(sx, sy) > MASK_LOGIT_THRESHOLD { MASK_INSIDE } else { 0 }])
    })
}

/// Default location of the model pair when `--models-dir` is not given.
pub fn default_models_dir() -> Option<PathBuf> {
    std::env::var_os("INOSCOPE_SAM_MODELS_DIR").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_points_and_requires_a_positive() {
        let pts = parse_points("10.5,20,1; 3,4,0").unwrap();
        assert_eq!(pts.len(), 2);
        assert!(pts[0].positive && !pts[1].positive);
        assert!(parse_points("3,4,0").is_err());
        assert!(parse_points("3,4").is_err());
        assert!(parse_points("3,4,2").is_err());
    }

    #[test]
    fn upsample_keeps_a_positive_block() {
        let mut logits = vec![-5.0f32; MASK_SIDE * MASK_SIDE];
        for y in 64..192 {
            for x in 64..192 {
                logits[y * MASK_SIDE + x] = 5.0;
            }
        }
        let mask = upsample_mask(&logits, 512, 512);
        assert_eq!(mask.get_pixel(256, 256)[0], MASK_INSIDE);
        assert_eq!(mask.get_pixel(10, 10)[0], 0);
    }
}
