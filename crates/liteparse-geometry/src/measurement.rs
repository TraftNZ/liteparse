//! Measurements from complete page geometry, independent of display pixels.

use std::io;

use serde::{Deserialize, Serialize};

use crate::model::{EXTRACTOR_VERSION, PageGeometry};

const MILLIMETERS_PER_POINT: f64 = 25.4 / 72.0;
const GEOMETRY_EPSILON: f64 = 1e-9;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementMode {
    #[default]
    Distance,
    Area,
    Perimeter,
}

/// Coordinates are full-page, top-left fractions. Scale is explicit: without
/// calibration or a supplied denominator, only paper measurements are returned.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MeasurementOptions {
    pub mode: MeasurementMode,
    pub points: Vec<[f64; 2]>,
    pub holes: Vec<Vec<[f64; 2]>>,
    pub polyline_index: Option<usize>,
    pub scale_denominator: Option<f64>,
    pub known_distance_mm: Option<f64>,
    pub snap_distance_mm: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Snap {
    pub point_index: usize,
    pub segment_index: usize,
    pub shift_paper_mm: f64,
}

#[derive(Debug, Serialize)]
pub struct Measurement {
    pub backend: &'static str,
    pub extractor_generation: u32,
    pub geometry_source: String,
    pub page: u32,
    pub mode: MeasurementMode,
    pub points_pts: Vec<[f64; 2]>,
    pub holes_pts: Vec<Vec<[f64; 2]>>,
    pub snaps: Vec<Snap>,
    pub paper_length_mm: Option<f64>,
    pub paper_area_mm2: Option<f64>,
    pub scale_denominator: Option<f64>,
    pub real_length_mm: Option<f64>,
    pub real_area_mm2: Option<f64>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn positive(value: Option<f64>) -> bool {
    value.is_none_or(|value| value.is_finite() && value > 0.0)
}

fn fractional_points(points: &[[f64; 2]], page: &PageGeometry) -> Result<Vec<[f64; 2]>, io::Error> {
    points
        .iter()
        .map(|&[x, y]| {
            if !x.is_finite()
                || !y.is_finite()
                || !(0.0..=1.0).contains(&x)
                || !(0.0..=1.0).contains(&y)
            {
                return Err(invalid(
                    "points must be finite full-page fractions between zero and one",
                ));
            }
            Ok([x * page.width_pts, y * page.height_pts])
        })
        .collect()
}

fn distance(a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn ring(points: &[[f64; 2]]) -> &[[f64; 2]] {
    if points.len() > 1 && distance(points[0], points[points.len() - 1]) <= GEOMETRY_EPSILON {
        &points[..points.len() - 1]
    } else {
        points
    }
}

fn edges(points: &[[f64; 2]]) -> impl Iterator<Item = ([f64; 2], [f64; 2])> + '_ {
    points
        .iter()
        .copied()
        .zip(points.iter().copied().cycle().skip(1))
        .take(points.len())
}

fn cross(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

fn on_segment(a: [f64; 2], b: [f64; 2], p: [f64; 2]) -> bool {
    cross(a, b, p).abs() <= GEOMETRY_EPSILON
        && p[0] >= a[0].min(b[0]) - GEOMETRY_EPSILON
        && p[0] <= a[0].max(b[0]) + GEOMETRY_EPSILON
        && p[1] >= a[1].min(b[1]) - GEOMETRY_EPSILON
        && p[1] <= a[1].max(b[1]) + GEOMETRY_EPSILON
}

fn intersects(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let ac = cross(a, b, c);
    let ad = cross(a, b, d);
    let ca = cross(c, d, a);
    let cb = cross(c, d, b);
    (ac.signum() != ad.signum()
        && ca.signum() != cb.signum()
        && ac.abs() > GEOMETRY_EPSILON
        && ad.abs() > GEOMETRY_EPSILON
        && ca.abs() > GEOMETRY_EPSILON
        && cb.abs() > GEOMETRY_EPSILON)
        || on_segment(a, b, c)
        || on_segment(a, b, d)
        || on_segment(c, d, a)
        || on_segment(c, d, b)
}

fn inside(points: &[[f64; 2]], p: [f64; 2]) -> bool {
    let mut result = false;
    for (a, b) in edges(points) {
        if (a[1] > p[1]) != (b[1] > p[1])
            && p[0] < (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) + a[0]
        {
            result = !result;
        }
    }
    result
}

fn area(points: &[[f64; 2]]) -> f64 {
    edges(points)
        .map(|(a, b)| a[0] * b[1] - b[0] * a[1])
        .sum::<f64>()
        .abs()
        / 2.0
}

fn validate_ring(points: &[[f64; 2]]) -> Result<(), io::Error> {
    if points.len() < 3 || area(points) <= GEOMETRY_EPSILON {
        return Err(invalid(
            "a boundary needs at least three points and positive area",
        ));
    }
    for (i, (a, b)) in edges(points).enumerate() {
        if distance(a, b) <= GEOMETRY_EPSILON {
            return Err(invalid("boundary contains a zero-length edge"));
        }
        for (j, (c, d)) in edges(points).enumerate().skip(i + 1) {
            if j == i + 1 || (i == 0 && j == points.len() - 1) {
                continue;
            }
            if intersects(a, b, c, d) {
                return Err(invalid("boundary intersects itself"));
            }
        }
    }
    Ok(())
}

fn boundaries_intersect(a: &[[f64; 2]], b: &[[f64; 2]]) -> bool {
    edges(a).any(|(a0, a1)| edges(b).any(|(b0, b1)| intersects(a0, a1, b0, b1)))
}

/// Measure source geometry without silently assuming a drawing scale.
pub fn measure_page(
    page: &PageGeometry,
    options: &MeasurementOptions,
) -> Result<Measurement, io::Error> {
    if page.version != EXTRACTOR_VERSION
        || !page.width_pts.is_finite()
        || !page.height_pts.is_finite()
        || page.width_pts <= 0.0
        || page.height_pts <= 0.0
    {
        return Err(invalid(
            "measurement requires current geometry with positive finite page dimensions",
        ));
    }
    if !positive(options.scale_denominator)
        || !positive(options.known_distance_mm)
        || !positive(options.snap_distance_mm)
    {
        return Err(invalid(
            "scale, known distance and snap distance must be positive and finite",
        ));
    }
    if options.scale_denominator.is_some() && options.known_distance_mm.is_some() {
        return Err(invalid(
            "use scale_denominator or known_distance_mm, not both",
        ));
    }
    let mut source_closed = false;
    let mut points = if let Some(index) = options.polyline_index {
        if !options.points.is_empty() || options.snap_distance_mm.is_some() {
            return Err(invalid(
                "polyline_index cannot be combined with points or snapping",
            ));
        }
        let polyline = page
            .polylines
            .as_ref()
            .and_then(|polylines| polylines.get(index))
            .ok_or_else(|| invalid("polyline_index is outside the complete page geometry"))?;
        source_closed = polyline.closed;
        if options.mode != MeasurementMode::Distance && !source_closed {
            return Err(invalid(
                "area and perimeter require a closed source polyline",
            ));
        }
        polyline.points.clone()
    } else {
        fractional_points(&options.points, page)?
    };
    if points.len() < 2 || points.iter().flatten().any(|value| !value.is_finite()) {
        return Err(invalid("measurement needs at least two finite points"));
    }
    let holes = options
        .holes
        .iter()
        .map(|points| fractional_points(points, page))
        .collect::<Result<Vec<_>, _>>()?;
    if options.mode == MeasurementMode::Distance && !holes.is_empty() {
        return Err(invalid("holes apply only to area or perimeter"));
    }
    if options.known_distance_mm.is_some()
        && (options.mode != MeasurementMode::Distance
            || points.len() != 2
            || options.polyline_index.is_some())
    {
        return Err(invalid(
            "known-distance calibration requires exactly two caller points in distance mode",
        ));
    }
    let mut snaps = Vec::new();
    if let Some(limit_mm) = options.snap_distance_mm {
        for (point_index, point) in points.iter_mut().enumerate() {
            let mut best = None;
            for (segment_index, segment) in page.segments.iter().flatten().enumerate() {
                let a = [segment.x1, segment.y1];
                let b = [segment.x2, segment.y2];
                let length_squared = (b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2);
                if !length_squared.is_finite() || length_squared <= GEOMETRY_EPSILON {
                    continue;
                }
                let t = (((point[0] - a[0]) * (b[0] - a[0]) + (point[1] - a[1]) * (b[1] - a[1]))
                    / length_squared)
                    .clamp(0.0, 1.0);
                let candidate = [a[0] + t * (b[0] - a[0]), a[1] + t * (b[1] - a[1])];
                if candidate[0] < 0.0
                    || candidate[0] > page.width_pts
                    || candidate[1] < 0.0
                    || candidate[1] > page.height_pts
                {
                    continue;
                }
                let shift = distance(*point, candidate) * MILLIMETERS_PER_POINT;
                if shift <= limit_mm && best.is_none_or(|(_, _, best_shift)| shift < best_shift) {
                    best = Some((segment_index, candidate, shift));
                }
            }
            if let Some((segment_index, candidate, shift_paper_mm)) = best {
                *point = candidate;
                snaps.push(Snap {
                    point_index,
                    segment_index,
                    shift_paper_mm,
                });
            }
        }
    }
    let (paper_length_mm, paper_area_mm2) = if options.mode == MeasurementMode::Distance {
        let mut length = points
            .windows(2)
            .map(|pair| distance(pair[0], pair[1]))
            .sum::<f64>();
        if source_closed {
            length += distance(points[points.len() - 1], points[0]);
        }
        (Some(length * MILLIMETERS_PER_POINT), None)
    } else {
        let outer = ring(&points);
        validate_ring(outer)?;
        for (index, hole) in holes.iter().enumerate() {
            let hole = ring(hole);
            validate_ring(hole)?;
            if !inside(outer, hole[0]) || boundaries_intersect(outer, hole) {
                return Err(invalid("holes must lie strictly inside the outer boundary"));
            }
            for other in &holes[..index] {
                let other = ring(other);
                if boundaries_intersect(other, hole)
                    || inside(other, hole[0])
                    || inside(hole, other[0])
                {
                    return Err(invalid("holes cannot overlap or contain each other"));
                }
            }
        }
        if options.mode == MeasurementMode::Area {
            let value = area(outer) - holes.iter().map(|hole| area(ring(hole))).sum::<f64>();
            (None, Some(value * MILLIMETERS_PER_POINT.powi(2)))
        } else {
            let length = edges(outer).map(|(a, b)| distance(a, b)).sum::<f64>()
                + holes
                    .iter()
                    .map(|hole| edges(ring(hole)).map(|(a, b)| distance(a, b)).sum::<f64>())
                    .sum::<f64>();
            (Some(length * MILLIMETERS_PER_POINT), None)
        }
    };
    let scale_denominator = if let Some(known) = options.known_distance_mm {
        let length = paper_length_mm.unwrap_or_default();
        if length <= GEOMETRY_EPSILON {
            return Err(invalid("calibration points must be distinct"));
        }
        Some(known / length)
    } else {
        options.scale_denominator
    };
    let real_length_mm = paper_length_mm
        .zip(scale_denominator)
        .map(|(length, scale)| length * scale);
    let real_area_mm2 = paper_area_mm2
        .zip(scale_denominator)
        .map(|(area, scale)| area * scale.powi(2));
    if [
        paper_length_mm,
        paper_area_mm2,
        scale_denominator,
        real_length_mm,
        real_area_mm2,
    ]
    .into_iter()
    .flatten()
    .any(|value| !value.is_finite())
    {
        return Err(invalid("measurement exceeds finite numeric range"));
    }
    Ok(Measurement {
        backend: "pdfium",
        extractor_generation: page.version,
        geometry_source: page.source.clone(),
        page: page.page,
        mode: options.mode,
        points_pts: points,
        holes_pts: holes,
        snaps,
        paper_length_mm,
        paper_area_mm2,
        scale_denominator,
        real_length_mm,
        real_area_mm2,
    })
}
