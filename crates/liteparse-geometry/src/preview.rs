//! Bounded preview geometry with indices into the uncapped page artifact.

use serde::{Deserialize, Serialize};

use crate::compat_math::hypot;
use crate::compat_sort::sort_by_less;
use crate::model::{PageGeometry, PaintStyle, Polyline, TextSpan, Viewport};

pub const MAX_SEGMENTS: usize = 1_000;
pub const MAX_TEXT_SPANS: usize = 5_000;
const MIN_LENGTH_PTS: f64 = 0.01;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreviewOptions {
    pub min_length_pts: f64,
    pub max_segments: usize,
    pub max_text_spans: usize,
    pub bbox_frac: Option<[f64; 4]>,
    pub no_text: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedSegment {
    pub index: usize,
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
    pub length_pts: f64,
    #[serde(flatten)]
    pub style: PaintStyle,
}

#[derive(Debug, Serialize)]
pub struct IndexedPolyline {
    pub index: usize,
    pub points: Vec<[f64; 2]>,
    pub closed: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PagePreview {
    pub version: u32,
    pub page: u32,
    pub width_pts: f64,
    pub height_pts: f64,
    pub class: String,
    pub path_ops: usize,
    pub image_ops: usize,
    #[serde(skip_serializing_if = "zero")]
    pub image_coverage: f64,
    pub source: String,
    #[serde(skip_serializing_if = "zero")]
    pub confidence: f64,
    pub total_segments: usize,
    pub total_polylines: usize,
    #[serde(skip_serializing_if = "zero_count")]
    pub total_text_spans: usize,
    pub segments: Vec<IndexedSegment>,
    pub polylines: Vec<IndexedPolyline>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub text_spans: Vec<TextSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_positions_error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub viewports: Vec<Viewport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub viewports_error: Option<String>,
}

fn zero(value: &f64) -> bool {
    *value == 0.0
}
fn zero_count(value: &usize) -> bool {
    *value == 0
}
fn cap(requested: usize, maximum: usize) -> usize {
    if requested == 0 {
        maximum
    } else {
        requested.min(maximum)
    }
}
fn intersects(bounds: [f64; 4], bbox: [f64; 4]) -> bool {
    bounds[0].min(bounds[2]) <= bbox[2]
        && bounds[0].max(bounds[2]) >= bbox[0]
        && bounds[1].min(bounds[3]) <= bbox[3]
        && bounds[1].max(bounds[3]) >= bbox[1]
}
fn polyline_length(polyline: &Polyline) -> f64 {
    let mut length: f64 = polyline
        .points
        .windows(2)
        .map(|pair| hypot(pair[1][0] - pair[0][0], pair[1][1] - pair[0][1]))
        .sum();
    if polyline.closed && polyline.points.len() > 1 {
        let first = polyline.points[0];
        let last = polyline.points[polyline.points.len() - 1];
        length += hypot(first[0] - last[0], first[1] - last[1]);
    }
    length
}

/// Preserve the Go helper's length ordering, bounding-box semantics and caps.
pub fn filter_page(page: &PageGeometry, options: &PreviewOptions) -> PagePreview {
    let bbox = options.bbox_frac.map(|fraction| {
        let bounds = [
            fraction[0] * page.width_pts,
            fraction[1] * page.height_pts,
            fraction[2] * page.width_pts,
            fraction[3] * page.height_pts,
        ];
        [
            bounds[0].min(bounds[2]),
            bounds[1].min(bounds[3]),
            bounds[0].max(bounds[2]),
            bounds[1].max(bounds[3]),
        ]
    });
    let segments = page.segments.as_deref().unwrap_or_default();
    let polylines = page.polylines.as_deref().unwrap_or_default();
    let text = page.text_spans.as_deref().unwrap_or_default();
    let floor = options.min_length_pts.max(MIN_LENGTH_PTS);
    let mut scored_segments: Vec<_> = segments
        .iter()
        .enumerate()
        .filter_map(|(index, segment)| {
            let length = hypot(segment.x2 - segment.x1, segment.y2 - segment.y1);
            (length >= floor
                && bbox.is_none_or(|bounds| {
                    intersects([segment.x1, segment.y1, segment.x2, segment.y2], bounds)
                }))
            .then_some((index, length, segment))
        })
        .collect();
    sort_by_less(&mut scored_segments, |left, right| left.1 > right.1);
    scored_segments.truncate(cap(options.max_segments, MAX_SEGMENTS));
    let filtered_segments = scored_segments
        .into_iter()
        .map(|(index, length, segment)| {
            // The established preview schema carries style but omits layer and
            // ownerPath. Both remain available in the uncapped full artifact.
            let mut style = (*segment.style).clone();
            style.layer.clear();
            IndexedSegment {
                index,
                x1: segment.x1,
                y1: segment.y1,
                x2: segment.x2,
                y2: segment.y2,
                length_pts: length,
                style,
            }
        })
        .collect();
    let mut scored_polylines: Vec<_> = polylines
        .iter()
        .enumerate()
        .filter_map(|(index, polyline)| {
            let length = polyline_length(polyline);
            (length >= floor
                && bbox.is_none_or(|bounds| {
                    polyline.points.iter().any(|point| {
                        point[0] >= bounds[0]
                            && point[0] <= bounds[2]
                            && point[1] >= bounds[1]
                            && point[1] <= bounds[3]
                    })
                }))
            .then_some((index, length, polyline))
        })
        .collect();
    sort_by_less(&mut scored_polylines, |left, right| left.1 > right.1);
    scored_polylines.truncate(cap(options.max_segments, MAX_SEGMENTS));
    let filtered_polylines = scored_polylines
        .into_iter()
        .map(|(index, _, polyline)| IndexedPolyline {
            index,
            points: polyline.points.clone(),
            closed: polyline.closed,
        })
        .collect();
    let filtered_text = if options.no_text {
        Vec::new()
    } else {
        text.iter()
            .filter(|span| {
                bbox.is_none_or(|bounds| intersects([span.x0, span.y0, span.x1, span.y1], bounds))
            })
            .take(cap(options.max_text_spans, MAX_TEXT_SPANS))
            .cloned()
            .collect()
    };
    PagePreview {
        version: page.version,
        page: page.page,
        width_pts: page.width_pts,
        height_pts: page.height_pts,
        class: page.class.clone(),
        path_ops: page.path_ops,
        image_ops: page.image_ops,
        image_coverage: page.image_coverage,
        source: page.source.clone(),
        confidence: page.confidence,
        total_segments: segments.len(),
        total_polylines: polylines.len(),
        total_text_spans: text.len(),
        segments: filtered_segments,
        polylines: filtered_polylines,
        text_spans: filtered_text,
        text_positions_error: page.text_positions_error.clone(),
        viewports: page.viewports.clone(),
        viewports_error: page.viewports_error.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Segment;

    fn page() -> PageGeometry {
        PageGeometry {
            version: crate::model::EXTRACTOR_VERSION,
            page: 1,
            width_pts: 100.0,
            height_pts: 100.0,
            class: "vector".into(),
            path_ops: 1,
            image_ops: 0,
            image_coverage: 0.0,
            segments: None,
            polylines: None,
            paths: None,
            text_spans: None,
            relations: Vec::new(),
            relations_partial: false,
            relations_analyzed: false,
            vertices: Vec::new(),
            vertex_refs: Vec::new(),
            source: "vector".into(),
            confidence: 0.0,
            text_positions_error: None,
            viewports: Vec::new(),
            viewports_error: None,
        }
    }

    #[test]
    fn keeps_indices_and_independent_caps_and_counts() {
        let mut page = page();
        page.segments = Some(
            (0..MAX_SEGMENTS + 1)
                .map(|index| Segment {
                    x1: 0.0,
                    y1: 0.0,
                    x2: (index + 1) as f64,
                    y2: 0.0,
                    owner_path: 1,
                    style: PaintStyle::default().into(),
                })
                .collect(),
        );
        page.text_spans = Some(
            (0..MAX_TEXT_SPANS + 1)
                .map(|index| TextSpan {
                    text: format!("label {index}"),
                    x0: 0.0,
                    y0: 0.0,
                    x1: 1.0,
                    y1: 1.0,
                    font_size: 10.0,
                    font_name: "Helvetica".into(),
                    rotation: 0.0,
                })
                .collect(),
        );
        let preview = filter_page(&page, &PreviewOptions::default());
        assert_eq!(preview.segments.len(), MAX_SEGMENTS);
        assert_eq!(preview.text_spans.len(), MAX_TEXT_SPANS);
        assert_eq!(preview.segments[0].index, MAX_SEGMENTS);
        assert_eq!(preview.total_segments, MAX_SEGMENTS + 1);
        assert_eq!(preview.total_text_spans, MAX_TEXT_SPANS + 1);
        let small = filter_page(
            &page,
            &PreviewOptions {
                max_segments: 1,
                ..Default::default()
            },
        );
        assert_eq!(small.segments.len(), 1);
        assert_eq!(small.text_spans.len(), MAX_TEXT_SPANS);
        let no_text = filter_page(
            &page,
            &PreviewOptions {
                no_text: true,
                ..Default::default()
            },
        );
        assert!(no_text.text_spans.is_empty());
        assert_eq!(no_text.total_text_spans, MAX_TEXT_SPANS + 1);
    }

    #[test]
    fn reversed_bbox_uses_segment_bounds_and_polyline_vertices() {
        let mut page = page();
        page.segments = Some(vec![
            Segment {
                x1: 0.0,
                y1: 50.0,
                x2: 100.0,
                y2: 50.0,
                owner_path: 1,
                style: PaintStyle {
                    layer: "Layer One".into(),
                    ..PaintStyle::default()
                }
                .into(),
            },
            Segment {
                x1: 10.0,
                y1: 10.0,
                x2: 20.0,
                y2: 10.0,
                owner_path: 2,
                style: PaintStyle::default().into(),
            },
        ]);
        page.polylines = Some(vec![
            Polyline {
                points: vec![[0.0, 50.0], [100.0, 50.0]],
                closed: false,
            },
            Polyline {
                points: vec![[50.0, 50.0], [60.0, 50.0]],
                closed: true,
            },
        ]);
        let preview = filter_page(
            &page,
            &PreviewOptions {
                bbox_frac: Some([0.6, 0.6, 0.4, 0.4]),
                min_length_pts: 15.0,
                ..Default::default()
            },
        );
        assert_eq!(preview.segments.len(), 1);
        assert_eq!(preview.segments[0].index, 0);
        assert!(preview.segments[0].style.layer.is_empty());
        assert_eq!(page.segments.as_ref().unwrap()[0].style.layer, "Layer One");
        assert_eq!(preview.polylines.len(), 1);
        assert_eq!(preview.polylines[0].index, 1);
    }
}
