//! Deterministic line tracing over a lossless grayscale PDF render.

use std::cmp::Ordering;
use std::f64::consts::PI;

use crate::compat_math::hypot;
use crate::compat_sort::sort_by_less;
use crate::model::{PaintStyle, Segment};

const DEFAULT_RASTER_DPI: f64 = 300.0;
const POINTS_PER_INCH: f64 = 72.0;
const THETA_STEPS: usize = 720;
const MIN_VOTES_AT_300_DPI: f64 = 40.0;
const PEAK_THETA_WINDOW_DEG: f64 = 3.0;
const PEAK_RHO_WINDOW_PX: f64 = 6.0;
const PERP_TOLERANCE_PX: f64 = 1.5;
const MAX_GAP_PX_AT_300_DPI: f64 = 8.0;
const MIN_SEG_PX_AT_300_DPI: f64 = 12.0;
const MERGE_GAP_PX_AT_300_DPI: f64 = 6.0;
const MERGE_ANGLE_DEG: f64 = 2.0;
const MAX_PEAKS: usize = 400;
const VOTE_SAMPLE_BUDGET: usize = 250_000;
const MAX_FOREGROUND_FRACTION: f64 = 0.40;
const RESIDUAL_PASSES: usize = 4;
const BROKEN_LINE_DIRECTION_TOL_DEG: f64 = 5.0;
const BROKEN_LINE_MIN_GAPS: usize = 5;
const BROKEN_LINE_GAP_SPREAD: f64 = 0.5;
const BROKEN_LINE_GAP_AGREEMENT: f64 = 0.7;
const MAX_MERGE_ROUNDS: usize = 6;

#[derive(Clone, Copy)]
struct HoughPeak {
    theta: f64,
    rho: f64,
}

#[derive(Clone, Copy)]
struct MarkOnLine {
    line: HoughPeak,
    segment: usize,
    start: f64,
    stop: f64,
}

/// Trace drawn centerlines in PDF points, returning the fraction of ink claimed.
pub fn trace_raster_segments(
    samples: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    dpi: f64,
) -> (Vec<Segment>, f64) {
    if width == 0
        || height == 0
        || dpi <= 0.0
        || stride < width
        || samples.len() < height.saturating_mul(stride)
    {
        return (Vec::new(), 0.0);
    }
    let (mut mask, foreground_count) = binarize_otsu(samples, width, height, stride);
    if foreground_count == 0
        || foreground_count as f64 > MAX_FOREGROUND_FRACTION * (width * height) as f64
    {
        return (Vec::new(), 0.0);
    }
    let scale = dpi / DEFAULT_RASTER_DPI;
    let px_to_pt = POINTS_PER_INCH / dpi;
    let mut segments = Vec::new();
    let mut on_lines = Vec::new();
    let mut claimed = 0;
    for _ in 0..=RESIDUAL_PASSES {
        let peaks = hough_peaks(&mask, width, height, scale);
        if peaks.is_empty() {
            break;
        }
        let (found, claimed_pixels, marks) =
            extract_runs(&mask, width, height, &peaks, scale, px_to_pt);
        if found.is_empty() {
            break;
        }
        on_lines.extend(marks.into_iter().map(|mut mark| {
            mark.segment += segments.len();
            mark
        }));
        segments.extend(found);
        claimed += claimed_pixels.len();
        if peaks.len() < MAX_PEAKS {
            break;
        }
        for index in claimed_pixels {
            mask[index] = false;
        }
    }
    if segments.is_empty() {
        return (segments, 0.0);
    }
    mark_lines_drawn_broken(&mut segments, &on_lines);
    segments = merge_collinear(segments);
    sort_by_less(&mut segments, |a, b| segment_length(a) > segment_length(b));
    (
        segments,
        (claimed as f64 / foreground_count as f64).min(1.0),
    )
}

fn binarize_otsu(samples: &[u8], width: usize, height: usize, stride: usize) -> (Vec<bool>, usize) {
    let mut histogram = [0usize; 256];
    for y in 0..height {
        for value in &samples[y * stride..y * stride + width] {
            histogram[usize::from(*value)] += 1;
        }
    }
    let threshold = otsu_threshold(&histogram, width * height);
    let mut mask = vec![false; width * height];
    let mut count = 0;
    for y in 0..height {
        for (x, value) in samples[y * stride..y * stride + width].iter().enumerate() {
            if usize::from(*value) <= threshold {
                mask[y * width + x] = true;
                count += 1;
            }
        }
    }
    (mask, count)
}

fn otsu_threshold(histogram: &[usize; 256], total: usize) -> usize {
    if total == 0 {
        return 128;
    }
    let sum: f64 = histogram
        .iter()
        .enumerate()
        .map(|(value, count)| (value * count) as f64)
        .sum();
    let (mut background_weight, mut background_sum, mut max_variance) = (0.0, 0.0, 0.0);
    let mut threshold = 0;
    for (value, count) in histogram.iter().enumerate() {
        background_weight += *count as f64;
        if background_weight == 0.0 {
            continue;
        }
        let foreground_weight = total as f64 - background_weight;
        if foreground_weight == 0.0 {
            break;
        }
        background_sum += (value * count) as f64;
        let background_mean = background_sum / background_weight;
        let foreground_mean = (sum - background_sum) / foreground_weight;
        let variance =
            background_weight * foreground_weight * (background_mean - foreground_mean).powi(2);
        if variance > max_variance {
            max_variance = variance;
            threshold = value;
        }
    }
    threshold
}

#[derive(Clone, Copy)]
struct Candidate {
    theta: usize,
    rho: usize,
    votes: i32,
}

fn hough_peaks(mask: &[bool], width: usize, height: usize, scale: f64) -> Vec<HoughPeak> {
    let rho_offset = hypot(width as f64, height as f64).ceil() as usize;
    let rho_count = 2 * rho_offset + 1;
    let angles: Vec<_> = (0..THETA_STEPS)
        .map(|index| {
            let theta = PI * index as f64 / THETA_STEPS as f64;
            (theta.cos(), theta.sin())
        })
        .collect();
    let foreground: Vec<_> = mask
        .iter()
        .enumerate()
        .filter_map(|(index, ink)| ink.then_some((index % width, index / width)))
        .collect();
    let stride = foreground.len().div_ceil(VOTE_SAMPLE_BUDGET).max(1);
    let mut accumulator = vec![0i32; THETA_STEPS * rho_count];
    for (x, y) in foreground.iter().step_by(stride) {
        for (theta, (cosine, sine)) in angles.iter().enumerate() {
            let rho = (*x as f64 * cosine + *y as f64 * sine).round() as isize;
            let index = (rho + rho_offset as isize) as usize;
            accumulator[theta * rho_count + index] += 1;
        }
    }
    let min_votes = (MIN_VOTES_AT_300_DPI * scale / stride as f64)
        .round()
        .max(1.0) as i32;
    let mut candidates = Vec::new();
    for theta in 0..THETA_STEPS {
        for rho in 0..rho_count {
            let votes = accumulator[theta * rho_count + rho];
            if votes >= min_votes {
                candidates.push(Candidate { theta, rho, votes });
            }
        }
    }
    sort_by_less(&mut candidates, |a, b| a.votes > b.votes);
    let rho_window = (PEAK_RHO_WINDOW_PX * scale).round().max(1.0) as isize;
    let theta_window = (PEAK_THETA_WINDOW_DEG * THETA_STEPS as f64 / 180.0)
        .round()
        .max(1.0) as isize;
    let mut accepted = Vec::new();
    let mut suppressed = vec![false; THETA_STEPS * rho_count];
    for candidate in candidates {
        if accepted.len() >= MAX_PEAKS {
            break;
        }
        if suppressed[candidate.theta * rho_count + candidate.rho] {
            continue;
        }
        accepted.push(HoughPeak {
            theta: PI * candidate.theta as f64 / THETA_STEPS as f64,
            rho: candidate.rho as f64 - rho_offset as f64,
        });
        for delta_theta in -theta_window..=theta_window {
            let theta = candidate.theta as isize + delta_theta;
            if !(0..THETA_STEPS as isize).contains(&theta) {
                continue;
            }
            for delta_rho in -rho_window..=rho_window {
                let rho = candidate.rho as isize + delta_rho;
                if (0..rho_count as isize).contains(&rho) {
                    suppressed[theta as usize * rho_count + rho as usize] = true;
                }
            }
        }
    }
    accepted
}

#[derive(Clone, Copy)]
struct AssignedPixel {
    projection: f64,
    x: usize,
    y: usize,
}

fn raster_segment(first: AssignedPixel, last: AssignedPixel, px_to_pt: f64) -> Segment {
    Segment {
        x1: first.x as f64 * px_to_pt,
        y1: first.y as f64 * px_to_pt,
        x2: last.x as f64 * px_to_pt,
        y2: last.y as f64 * px_to_pt,
        owner_path: 0,
        style: PaintStyle {
            paint: "raster-traced".into(),
            ..PaintStyle::default()
        }
        .into(),
    }
}

fn extract_runs(
    mask: &[bool],
    width: usize,
    height: usize,
    peaks: &[HoughPeak],
    scale: f64,
    px_to_pt: f64,
) -> (Vec<Segment>, Vec<usize>, Vec<MarkOnLine>) {
    let perpendicular_tolerance = PERP_TOLERANCE_PX * scale;
    let max_gap = MAX_GAP_PX_AT_300_DPI * scale;
    let min_length = MIN_SEG_PX_AT_300_DPI * scale;
    let normals: Vec<_> = peaks
        .iter()
        .map(|peak| (peak.theta.cos(), peak.theta.sin()))
        .collect();
    let directions: Vec<_> = peaks
        .iter()
        .map(|peak| (-peak.theta.sin(), peak.theta.cos()))
        .collect();
    let mut buckets: Vec<Vec<AssignedPixel>> = vec![Vec::new(); peaks.len()];
    for y in 0..height {
        for x in 0..width {
            if !mask[y * width + x] {
                continue;
            }
            let mut best = None;
            let mut best_distance = perpendicular_tolerance;
            for (index, peak) in peaks.iter().enumerate() {
                let distance =
                    (x as f64 * normals[index].0 + y as f64 * normals[index].1 - peak.rho).abs();
                if distance <= best_distance {
                    best_distance = distance;
                    best = Some(index);
                }
            }
            if let Some(index) = best {
                buckets[index].push(AssignedPixel {
                    projection: x as f64 * directions[index].0 + y as f64 * directions[index].1,
                    x,
                    y,
                });
            }
        }
    }
    let mut segments = Vec::new();
    let mut claimed = Vec::new();
    let mut marks = Vec::new();
    for (peak_index, pixels) in buckets.iter_mut().enumerate() {
        if pixels.is_empty() {
            continue;
        }
        sort_by_less(pixels, |a, b| a.projection < b.projection);
        let mut run_start = 0;
        for end in 1..=pixels.len() {
            let gap = if end < pixels.len() {
                pixels[end].projection - pixels[end - 1].projection
            } else {
                f64::INFINITY
            };
            if end < pixels.len() && gap <= max_gap {
                continue;
            }
            let run = &pixels[run_start..end];
            if run.last().unwrap().projection - run[0].projection >= min_length {
                marks.push(MarkOnLine {
                    line: HoughPeak {
                        theta: peaks[peak_index].theta,
                        rho: peaks[peak_index].rho * px_to_pt,
                    },
                    segment: segments.len(),
                    start: run[0].projection * px_to_pt,
                    stop: run.last().unwrap().projection * px_to_pt,
                });
                segments.push(raster_segment(run[0], *run.last().unwrap(), px_to_pt));
                claimed.extend(run.iter().map(|pixel| pixel.y * width + pixel.x));
            }
            run_start = end;
        }
    }
    (segments, claimed, marks)
}

fn mark_lines_drawn_broken(segments: &mut [Segment], marks: &[MarkOnLine]) {
    if marks.is_empty() {
        return;
    }
    let (mut center_x, mut center_y) = (0.0, 0.0);
    for mark in marks {
        let segment = &segments[mark.segment];
        center_x += (segment.x1 + segment.x2) * 0.5;
        center_y += (segment.y1 + segment.y2) * 0.5;
    }
    center_x /= marks.len() as f64;
    center_y /= marks.len() as f64;
    let distance_between = |a: HoughPeak, b: HoughPeak| {
        let a_distance = center_x * a.theta.cos() + center_y * a.theta.sin() - a.rho;
        let b_distance = center_x * b.theta.cos() + center_y * b.theta.sin() - b.rho;
        (a_distance - b_distance).abs()
    };
    let theta_tolerance = BROKEN_LINE_DIRECTION_TOL_DEG * PI / 180.0;
    let rho_tolerance = PERP_TOLERANCE_PX * POINTS_PER_INCH / DEFAULT_RASTER_DPI;
    let mut lines = Vec::new();
    let mut by_line: Vec<Vec<MarkOnLine>> = Vec::new();
    for mark in marks {
        let position = lines.iter().position(|line: &HoughPeak| {
            let mut delta = (mark.line.theta - line.theta).abs();
            if delta > PI / 2.0 {
                delta = PI - delta;
            }
            delta <= theta_tolerance && distance_between(mark.line, *line) <= rho_tolerance
        });
        let index = if let Some(index) = position {
            index
        } else {
            lines.push(mark.line);
            by_line.push(Vec::new());
            lines.len() - 1
        };
        by_line[index].push(*mark);
    }
    for mut line in by_line {
        if line.len() < BROKEN_LINE_MIN_GAPS + 1 {
            continue;
        }
        sort_by_less(&mut line, |a, b| a.start < b.start);
        let gaps: Vec<_> = line
            .windows(2)
            .map(|pair| pair[1].start - pair[0].stop)
            .collect();
        let Some(dash) = drawn_broken_pattern(&gaps) else {
            continue;
        };
        for mark in line {
            std::sync::Arc::make_mut(&mut segments[mark.segment].style).dash_array = vec![dash];
        }
    }
}

fn drawn_broken_pattern(gaps: &[f64]) -> Option<f64> {
    if gaps.len() < BROKEN_LINE_MIN_GAPS {
        return None;
    }
    let mut sorted = gaps.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    if median <= 0.0 {
        return None;
    }
    let agreeing = gaps
        .iter()
        .filter(|gap| (*gap - median).abs() / median <= BROKEN_LINE_GAP_SPREAD)
        .count();
    (agreeing as f64 >= BROKEN_LINE_GAP_AGREEMENT * gaps.len() as f64).then_some(median)
}

fn segment_length(segment: &Segment) -> f64 {
    hypot(segment.x2 - segment.x1, segment.y2 - segment.y1)
}

fn segment_angle(segment: &Segment) -> f64 {
    (segment.y2 - segment.y1).atan2(segment.x2 - segment.x1)
}

fn normalized_angle(mut angle: f64) -> f64 {
    while angle > PI / 2.0 {
        angle -= PI;
    }
    while angle <= -PI / 2.0 {
        angle += PI;
    }
    angle
}

fn merge_collinear(mut segments: Vec<Segment>) -> Vec<Segment> {
    if segments.len() < 2 {
        return segments;
    }
    let merge_gap = MERGE_GAP_PX_AT_300_DPI * POINTS_PER_INCH / DEFAULT_RASTER_DPI;
    let angle_tolerance = MERGE_ANGLE_DEG * PI / 180.0;
    for _ in 0..MAX_MERGE_ROUNDS {
        let mut changed = false;
        let mut first = 0;
        while first < segments.len() {
            let mut second = first + 1;
            while second < segments.len() {
                if let Some(merged) = try_merge_segments(
                    &segments[first],
                    &segments[second],
                    merge_gap,
                    angle_tolerance,
                ) {
                    segments[first] = merged;
                    segments.remove(second);
                    changed = true;
                } else {
                    second += 1;
                }
            }
            first += 1;
        }
        if !changed {
            break;
        }
    }
    segments
}

fn try_merge_segments(a: &Segment, b: &Segment, gap: f64, angle_tolerance: f64) -> Option<Segment> {
    let angle_delta = (normalized_angle(segment_angle(a) - segment_angle(b))).abs();
    if angle_delta > angle_tolerance && (angle_delta - PI).abs() > angle_tolerance {
        return None;
    }
    let a_ends = [(a.x1, a.y1), (a.x2, a.y2)];
    let b_ends = [(b.x1, b.y1), (b.x2, b.y2)];
    let min_distance = a_ends
        .iter()
        .flat_map(|first| {
            b_ends
                .iter()
                .map(move |second| hypot(first.0 - second.0, first.1 - second.1))
        })
        .fold(f64::INFINITY, f64::min);
    if min_distance > gap {
        return None;
    }
    let ends = [a_ends[0], a_ends[1], b_ends[0], b_ends[1]];
    let mut longest = -1.0;
    let mut endpoints = (ends[0], ends[1]);
    for first in 0..ends.len() {
        for second in first + 1..ends.len() {
            let distance = hypot(
                ends[first].0 - ends[second].0,
                ends[first].1 - ends[second].1,
            );
            if distance > longest {
                longest = distance;
                endpoints = (ends[first], ends[second]);
            }
        }
    }
    let dash = if a.style.dash_array.is_empty() {
        b.style.dash_array.clone()
    } else {
        a.style.dash_array.clone()
    };
    Some(Segment {
        x1: endpoints.0.0,
        y1: endpoints.0.1,
        x2: endpoints.1.0,
        y2: endpoints.1.1,
        owner_path: 0,
        style: PaintStyle {
            paint: "raster-traced".into(),
            dash_array: dash,
            ..PaintStyle::default()
        }
        .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otsu_separates_dark_ink() {
        let samples = [255, 255, 0, 0];
        let (mask, count) = binarize_otsu(&samples, 4, 1, 4);
        assert_eq!(mask, [false, false, true, true]);
        assert_eq!(count, 2);
    }

    #[test]
    fn repeated_dashes_need_consistent_gaps() {
        assert_eq!(drawn_broken_pattern(&[2.0, 2.0, 2.1, 1.9, 2.0]), Some(2.0));
        assert_eq!(drawn_broken_pattern(&[2.0, 8.0, 2.0, 10.0, 2.0]), None);
    }
}
