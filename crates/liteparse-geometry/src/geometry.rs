//! Assemble source primitives into the full page geometry contract.

use crate::classify::classify;
use crate::content_paths::SourcePageContent;
use crate::model::TextSpan;
use crate::model::{EXTRACTOR_VERSION, PageGeometry};

/// Build the vector reading of a page. Raster tracing and relation analysis
/// run over this page before it is emitted by the geometry operation.
pub fn assemble_vector_page(
    page_number: u32,
    content: SourcePageContent,
    text_spans: Vec<TextSpan>,
) -> PageGeometry {
    let paths = content.paths;
    let class = classify(paths.path_ops, content.image_ops, text_spans.len()).to_owned();
    PageGeometry {
        version: EXTRACTOR_VERSION,
        page: page_number,
        width_pts: content.width_pts,
        height_pts: content.height_pts,
        class,
        path_ops: paths.path_ops,
        image_ops: content.image_ops,
        image_coverage: content.image_coverage,
        segments: (!paths.segments.is_empty()).then_some(paths.segments),
        polylines: (!paths.polylines.is_empty()).then_some(paths.polylines),
        paths: (!paths.paths.is_empty()).then_some(paths.paths),
        text_spans: (!text_spans.is_empty()).then_some(text_spans),
        relations: Vec::new(),
        relations_partial: false,
        relations_analyzed: false,
        vertices: Vec::new(),
        vertex_refs: Vec::new(),
        source: "vector".into(),
        confidence: 0.0,
    }
}
