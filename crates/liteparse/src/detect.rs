//! Object-detector inference over a page image: `lit detect`.
//!
//! The model is an RF-DETR export. The image is cut into overlapping tiles,
//! each stretched to the model's input size and run once. The raw output
//! tensors are returned per tile; sigmoid, the background slot, box
//! conversion to page pixels and merging across tiles belong to the caller.

use std::path::Path;

use image::imageops::FilterType;
use ort::session::Session;
use ort::value::Tensor;
use serde::Serialize;

const INPUT_NAME: &str = "input";
const BOXES_OUTPUT: &str = "dets";
const LOGITS_OUTPUT: &str = "labels";
const BOX_VALUES: usize = 4;
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];
const CHANNELS: usize = 3;
const BYTE_MAX: f32 = 255.0;

#[derive(Debug, Clone, Copy)]
pub struct DetectOptions {
    pub tile_px: u32,
    pub overlap_px: u32,
    pub threads: usize,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct TileDetections {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Normalised cxcywh per query, relative to the tile.
    pub dets: Vec<[f32; BOX_VALUES]>,
    /// Raw logits per query, one per class slot.
    pub labels: Vec<Vec<f32>>,
}

#[derive(Debug, Serialize)]
pub struct DetectReport {
    pub image_width: u32,
    pub image_height: u32,
    pub input_width: u32,
    pub input_height: u32,
    pub tiles: Vec<TileDetections>,
}

/// Tile origins along one axis: tiles of `tile` (clamped to `len`) stepping by
/// `tile - overlap`, the last one pulled back so it ends at the edge.
pub fn tile_origins(len: u32, tile: u32, overlap: u32) -> Vec<u32> {
    let tile = tile.min(len);
    if tile == 0 || len <= tile {
        return vec![0];
    }
    let stride = tile.saturating_sub(overlap).max(1);
    let mut origins = Vec::new();
    let mut at = 0u32;
    loop {
        if at + tile >= len {
            origins.push(len - tile);
            break;
        }
        origins.push(at);
        at += stride;
    }
    origins
}

fn model_input_size(session: &Session) -> Result<(u32, u32), String> {
    let outlet = session.inputs().first().ok_or("the model has no input")?;
    let shape = outlet
        .dtype()
        .tensor_shape()
        .ok_or("the model's first input is not a tensor")?;
    let dims: Vec<i64> = shape.iter().copied().collect();
    if dims.len() != 4 || dims[2] <= 0 || dims[3] <= 0 {
        return Err(format!("the model input must be fixed NCHW, got {dims:?}"));
    }
    Ok((dims[3] as u32, dims[2] as u32))
}

fn tile_tensor(
    image: &image::RgbImage,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    input_width: u32,
    input_height: u32,
) -> Vec<f32> {
    let crop = image::imageops::crop_imm(image, x, y, width, height).to_image();
    let resized = image::imageops::resize(&crop, input_width, input_height, FilterType::Triangle);
    let plane = (input_width * input_height) as usize;
    let mut data = vec![0f32; CHANNELS * plane];
    for (i, pixel) in resized.pixels().enumerate() {
        for c in 0..CHANNELS {
            data[c * plane + i] = (pixel[c] as f32 / BYTE_MAX - MEAN[c]) / STD[c];
        }
    }
    data
}

pub fn detect(model: &Path, image_path: &Path, options: DetectOptions) -> Result<DetectReport, String> {
    let image = image::open(image_path)
        .map_err(|e| format!("could not read image {}: {e}", image_path.display()))?
        .to_rgb8();
    let (image_width, image_height) = image.dimensions();
    let mut session = Session::builder()
        .map_err(|e| e.to_string())?
        .with_intra_threads(options.threads)
        .map_err(|e| e.to_string())?
        .commit_from_file(model)
        .map_err(|e| format!("could not load model {}: {e}", model.display()))?;
    let (input_width, input_height) = model_input_size(&session)?;

    let mut tiles = Vec::new();
    let tile_w = options.tile_px.min(image_width);
    let tile_h = options.tile_px.min(image_height);
    for y in tile_origins(image_height, options.tile_px, options.overlap_px) {
        for x in tile_origins(image_width, options.tile_px, options.overlap_px) {
            let data = tile_tensor(&image, x, y, tile_w, tile_h, input_width, input_height);
            let tensor = Tensor::from_array((
                [1usize, CHANNELS, input_height as usize, input_width as usize],
                data,
            ))
            .map_err(|e| e.to_string())?;
            let outputs = session
                .run(ort::inputs![INPUT_NAME => tensor])
                .map_err(|e| format!("inference failed: {e}"))?;
            let (box_shape, box_data) = outputs
                .get(BOXES_OUTPUT)
                .ok_or_else(|| format!("the model has no output named {BOXES_OUTPUT:?}"))?
                .try_extract_tensor::<f32>()
                .map_err(|e| e.to_string())?;
            let (logit_shape, logit_data) = outputs
                .get(LOGITS_OUTPUT)
                .ok_or_else(|| format!("the model has no output named {LOGITS_OUTPUT:?}"))?
                .try_extract_tensor::<f32>()
                .map_err(|e| e.to_string())?;
            let box_dims: Vec<i64> = box_shape.iter().copied().collect();
            let logit_dims: Vec<i64> = logit_shape.iter().copied().collect();
            if box_dims.len() != 3 || box_dims[2] as usize != BOX_VALUES || logit_dims.len() != 3 {
                return Err(format!("unexpected output shapes dets={box_dims:?} labels={logit_dims:?}"));
            }
            let queries = box_dims[1] as usize;
            let classes = logit_dims[2] as usize;
            if logit_dims[1] as usize != queries {
                return Err(format!("dets has {queries} queries but labels has {}", logit_dims[1]));
            }
            let dets = box_data[..queries * BOX_VALUES]
                .chunks_exact(BOX_VALUES)
                .map(|b| [b[0], b[1], b[2], b[3]])
                .collect();
            let labels = logit_data[..queries * classes]
                .chunks_exact(classes)
                .map(<[f32]>::to_vec)
                .collect();
            tiles.push(TileDetections { x, y, width: tile_w, height: tile_h, dets, labels });
        }
    }
    Ok(DetectReport { image_width, image_height, input_width, input_height, tiles })
}

#[cfg(test)]
mod tests {
    use super::tile_origins;

    #[test]
    fn small_image_is_one_tile() {
        assert_eq!(tile_origins(800, 1024, 128), vec![0]);
    }

    #[test]
    fn last_tile_ends_at_the_edge() {
        assert_eq!(tile_origins(2000, 1024, 128), vec![0, 896, 976]);
    }

    #[test]
    fn exact_fit_is_one_tile() {
        assert_eq!(tile_origins(1024, 1024, 128), vec![0]);
    }
}
