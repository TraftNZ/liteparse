//! Wire types shared by the uncapped geometry artifact and the preview builder.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

fn is_zero_f64(value: &f64) -> bool {
    *value == 0.0
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

fn is_zero_i32(value: &i32) -> bool {
    *value == 0
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaintStyle {
    pub stroke_width: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub paint: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub color_space: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stroke_color: Vec<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fill_color: Vec<f64>,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub alpha: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dash_array: Vec<f64>,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub dash_phase: f64,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub clip_depth: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty", rename = "clipBBox")]
    pub clip_bbox: Vec<f64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fill_pattern_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stroke_pattern_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub form_x_object_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub form_x_object_ref: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub form_ctm: Vec<f64>,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub tile_id: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tile_ctm: Vec<f64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub layer: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub owner_path: usize,
    #[serde(flatten)]
    pub style: Arc<PaintStyle>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Polyline {
    pub points: Vec<[f64; 2]>,
    pub closed: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathOp {
    pub kind: String,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub x: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub y: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub ctrl1_x: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub ctrl1_y: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub ctrl2_x: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub ctrl2_y: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Path {
    pub ops: Vec<PathOp>,
    pub closed: bool,
    #[serde(flatten)]
    pub style: Arc<PaintStyle>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSpan {
    pub text: String,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub font_size: f64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub font_name: String,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub rotation: f64,
}

#[derive(Clone, Debug, Default, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct PrimitiveRef {
    pub kind: String,
    pub index: usize,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Relation {
    #[serde(rename = "type")]
    pub kind: String,
    pub from: PrimitiveRef,
    pub to: PrimitiveRef,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub distance: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub angle_delta: f64,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub confidence: i32,
    #[serde(default, skip_serializing_if = "is_zero_f64", rename = "centerAX")]
    pub center_ax: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64", rename = "centerAY")]
    pub center_ay: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64", rename = "centerBX")]
    pub center_bx: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64", rename = "centerBY")]
    pub center_by: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub radius_a: f64,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub radius_b: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Vertex {
    pub index: usize,
    pub x: f64,
    pub y: f64,
    pub degree: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VertexRef {
    pub primitive: PrimitiveRef,
    pub endpoint_index: i32,
    pub vertex_index: usize,
    #[serde(default, skip_serializing_if = "is_false")]
    pub on_edge: bool,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub t: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageGeometry {
    pub version: u32,
    pub page: u32,
    pub width_pts: f64,
    pub height_pts: f64,
    pub class: String,
    pub path_ops: usize,
    pub image_ops: usize,
    pub image_coverage: f64,
    pub segments: Option<Vec<Segment>>,
    pub polylines: Option<Vec<Polyline>>,
    pub paths: Option<Vec<Path>>,
    pub text_spans: Option<Vec<TextSpan>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relations: Vec<Relation>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub relations_partial: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub relations_analyzed: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vertices: Vec<Vertex>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vertex_refs: Vec<VertexRef>,
    pub source: String,
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub confidence: f64,
    /// How far a scanned sheet's drawing is turned off the page axes, in
    /// degrees, positive clockwise as the page is viewed. Reported, not
    /// corrected: the traced lines keep the scan's own coordinates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skew_degrees: Option<f64>,
    /// Why the text could not be placed at its content-stream positions. The
    /// page keeps its geometry and PDFium's text positions, which are coarser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_positions_error: Option<String>,
    /// The drawings the page declares in its `/VP` array, in page space.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub viewports: Vec<Viewport>,
    /// Why the page's `/VP` array could not be read. The page keeps its
    /// geometry; it states no viewports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewports_error: Option<String>,
}

/// One drawing's rectangle on the sheet, as the file declares it.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Viewport {
    /// `[min_x, min_y, max_x, max_y]` in the page's viewport space.
    pub bbox: [f64; 4],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<ViewportScale>,
}

/// A rectilinear `/Measure`: model units per point of the viewport.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewportScale {
    pub units_per_point: f64,
    /// The number format's unit label (`/U`); writers often leave it blank.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub units: Option<String>,
    /// The measure's ratio label (`/R`), as lettered by the writer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ratio: Option<String>,
}

/// Extractor generation for PDFium geometry and its derived raster traces.
pub const EXTRACTOR_VERSION: u32 = 12;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_keys_preserve_geometry_provenance() {
        let path = Path {
            ops: vec![PathOp {
                kind: "curveto".into(),
                x: 1.0,
                y: 2.0,
                ctrl1_x: 3.0,
                ctrl1_y: 4.0,
                ctrl2_x: 5.0,
                ctrl2_y: 6.0,
            }],
            closed: false,
            style: PaintStyle {
                stroke_width: 2.0,
                paint: "stroke".into(),
                tile_id: 6,
                tile_ctm: vec![1.0, 0.0, 0.0, -1.0, 0.0, 200.0],
                ..PaintStyle::default()
            }
            .into(),
        };
        let value = serde_json::to_value(path).unwrap();
        assert_eq!(value["ops"][0]["ctrl1X"], 3.0);
        assert_eq!(value["ops"][0]["ctrl2Y"], 6.0);
        assert_eq!(value["tileId"], 6);
        assert_eq!(value["tileCtm"][3], -1.0);
        assert!(value.get("tileDocId").is_none());
        assert!(value.get("colorSpace").is_none());
    }
}
