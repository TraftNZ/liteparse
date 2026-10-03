//! Deterministic line tracing over a lossless grayscale render of a scanned
//! drawing.
//!
//! The ink is binarised (Otsu), thinned to a one-pixel skeleton (Zhang–Suen)
//! and the skeleton followed as a graph: its ends, its junctions and the edges
//! between them. Each edge is simplified to straight strokes, each stroke is
//! fitted to the pixels it came from, and its drawn width is measured across
//! the ink on both sides. A stroke wider than the heaviest pen a drawing is
//! inked with is a filled band — a wall drawn solid — and is given as its two
//! faces; any other stroke as its centre line, carrying its width.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::f64::consts::PI;

use crate::compat_math::hypot;
use crate::compat_sort::sort_by_less;
use crate::model::{PaintStyle, Segment};

const POINTS_PER_INCH: f64 = 72.0;
const MM_PER_INCH: f64 = 25.4;
/// More ink than this is a photograph or a solid fill, not line work.
const MAX_FOREGROUND_FRACTION: f64 = 0.40;
/// How far a skeleton may wander off a straight stroke and still be that
/// stroke: a thinned edge sits within a pixel of its ink's middle, and a scan's
/// ragged edge moves that middle by up to half a pixel more.
const SIMPLIFY_TOLERANCE_PX: f64 = 1.5;
/// The shortest free-standing stroke kept, on paper: below a millimetre a
/// trace is a speck, a dot or a letter's serif, not a drawn line.
const MIN_STROKE_LENGTH_MM: f64 = 1.0;
/// A side branch this short off a junction is the thinning's own burr from a
/// ragged edge, whatever the stroke's width.
const SPUR_MIN_LENGTH_PX: f64 = 3.0;
/// Thinning a band with square corners leaves a branch from the band's middle
/// into each corner, about 0.7 of the band's width long. A free branch no
/// longer than the band is wide is such a corner, not a drawn line.
const SPUR_LENGTH_PER_WIDTH: f64 = 1.0;
/// The widest stroke a pen draws on an architectural plan: the wide line of
/// the ISO 128 0.7 line group. Ink wider than this is a filled band.
const BAND_MIN_WIDTH_MM: f64 = 0.7;
/// The widest band measured across, on paper; a wider run of ink is a solid
/// area, and its width is reported at this cap.
const MAX_STROKE_WIDTH_MM: f64 = 15.0;
/// How many places along a stroke its width is measured; the median is kept,
/// so the few places where another line crosses do not move it.
const WIDTH_SAMPLES_PER_STROKE: usize = 64;
/// The antialiased fringe of a rendered line, counted as the line's own ink.
const COVERAGE_SLACK_PX: f64 = 1.0;
/// Two strokes turning by less than this meet at their ends' midpoint rather
/// than where their lines cross, which runs away as the lines go parallel.
const CORNER_MIN_TURN_DEG: f64 = 2.0;
/// Returning to its own junction, an edge must have left it first.
const SELF_LOOP_MIN_PIXELS: usize = 3;
const SKEW_BIN_DEG: f64 = 0.05;
const SKEW_REFINEMENTS: usize = 2;
/// The spread of the drawing's own axes round its skew: lines drafted square
/// lie within this of each other after a scan turns them all together.
const SKEW_WINDOW_DEG: f64 = 0.5;
/// A turn smaller than this is within a pixel across an A3 sheet at 300 dpi.
const SKEW_MIN_REPORT_DEG: f64 = 0.1;
/// The share of the stroke length that must lie square to one pair of axes
/// before the sheet is said to be turned by them.
const SKEW_MIN_SUPPORT: f64 = 0.5;
/// Strokes on one line closer than this are one line broken by the scan.
const MERGE_GAP_MM: f64 = 0.5;
const MERGE_ANGLE_DEG: f64 = 2.0;
const MAX_MERGE_ROUNDS: usize = 6;
const BROKEN_LINE_DIRECTION_TOL_DEG: f64 = 5.0;
/// Pieces of one dashed line lie on it to within a scan pixel at 300 dpi.
const BROKEN_LINE_OFFSET_TOLERANCE_MM: f64 = 0.127;
const BROKEN_LINE_MIN_GAPS: usize = 5;
const BROKEN_LINE_GAP_SPREAD: f64 = 0.5;
const BROKEN_LINE_GAP_AGREEMENT: f64 = 0.7;
const RASTER_PAINT: &str = "raster-traced";

/// Skeleton pixel states while the graph is followed.
const SKELETON: u8 = 1;
const VISITED: u8 = 2;
const NODE: u8 = 3;

/// The eight neighbours in Zhang–Suen order P2..P9: N, NE, E, SE, S, SW, W, NW.
const RING: [(isize, isize); 8] = [
    (0, -1),
    (1, -1),
    (1, 0),
    (1, 1),
    (0, 1),
    (-1, 1),
    (-1, 0),
    (-1, -1),
];
/// Neighbours in walking order: the four sharing a side before the corners, so
/// a walk takes every pixel of a staircase rather than cutting across it.
const WALK_ORDER: [(isize, isize); 8] = [
    (0, -1),
    (1, 0),
    (0, 1),
    (-1, 0),
    (1, -1),
    (1, 1),
    (-1, 1),
    (-1, -1),
];

/// A traced page: its lines in PDF points, the fraction of the ink they
/// explain, and the sheet's skew where one is detected.
#[derive(Default)]
pub struct RasterTrace {
    pub segments: Vec<Segment>,
    pub confidence: f64,
    pub skew_degrees: Option<f64>,
}

#[derive(Clone, Copy)]
struct LineOnPage {
    theta: f64,
    rho: f64,
}

#[derive(Clone, Copy)]
struct MarkOnLine {
    line: LineOnPage,
    segment: usize,
    start: f64,
    stop: f64,
}

type Point = (f64, f64);

struct Grid<'a> {
    width: usize,
    height: usize,
    ink: &'a [bool],
}

impl Grid<'_> {
    fn ink_at(&self, x: f64, y: f64) -> bool {
        if x < 0.0 || y < 0.0 {
            return false;
        }
        let (px, py) = (x as usize, y as usize);
        px < self.width && py < self.height && self.ink[py * self.width + px]
    }

    /// How far ink runs on from a point in one direction, to the edge of the
    /// last inked pixel.
    fn reach(&self, from: Point, direction: Point, max_px: f64) -> f64 {
        let mut t = 0.0;
        while t <= max_px && self.ink_at(from.0 + direction.0 * t, from.1 + direction.1 * t) {
            t += 1.0;
        }
        (t - 0.5).max(0.0)
    }

    /// Distance from a point to the nearest paper pixel's edge.
    fn clearance(&self, at: Point, max_px: f64) -> f64 {
        let (cx, cy) = (at.0.floor() as isize, at.1.floor() as isize);
        let limit = max_px.ceil() as isize;
        let mut best = max_px;
        for radius in 1..=limit {
            if radius as f64 - 0.5 > best {
                break;
            }
            for dy in -radius..=radius {
                for dx in -radius..=radius {
                    if dx.abs() != radius && dy.abs() != radius {
                        continue;
                    }
                    let (x, y) = (cx + dx, cy + dy);
                    let paper = x < 0
                        || y < 0
                        || x as usize >= self.width
                        || y as usize >= self.height
                        || !self.ink[y as usize * self.width + x as usize];
                    if paper {
                        let distance = hypot(x as f64 + 0.5 - at.0, y as f64 + 0.5 - at.1) - 0.5;
                        best = best.min(distance.max(0.0));
                    }
                }
            }
        }
        best
    }
}

/// Trace drawn lines in PDF points, with the fraction of the ink they explain.
pub fn trace_raster_segments(
    samples: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    dpi: f64,
) -> RasterTrace {
    if width == 0
        || height == 0
        || dpi <= 0.0
        || stride < width
        || samples.len() < height.saturating_mul(stride)
        || u32::try_from(width.saturating_mul(height)).is_err()
    {
        return RasterTrace::default();
    }
    let (mask, foreground_count) = binarize_otsu(samples, width, height, stride);
    if foreground_count == 0
        || foreground_count as f64 > MAX_FOREGROUND_FRACTION * (width * height) as f64
    {
        return RasterTrace::default();
    }
    let px_per_mm = dpi / MM_PER_INCH;
    let grid = Grid {
        width,
        height,
        ink: &mask,
    };
    let mut skeleton = thin_zhang_suen(&mask, width, height);
    remove_staircases(&mut skeleton, width, height);
    let graph = follow_skeleton(&mut skeleton, width, height);
    let max_width_px = MAX_STROKE_WIDTH_MM * px_per_mm;
    let chains = graph.chains(&grid, max_width_px);

    let mut strokes = Vec::new();
    for chain in &chains {
        strokes.extend(chain_strokes(chain, &graph, &grid, px_per_mm));
    }
    if strokes.is_empty() {
        return RasterTrace::default();
    }

    let covered = &mut skeleton;
    covered.fill(0);
    let mut explained = 0usize;
    for stroke in &strokes {
        explained += cover_capsule(
            covered,
            &grid,
            stroke.a,
            stroke.b,
            stroke.half_width + COVERAGE_SLACK_PX,
        );
    }
    let skew_degrees = sheet_skew(&strokes);

    let px_to_pt = POINTS_PER_INCH / dpi;
    let mut segments = Vec::new();
    let mut marks = Vec::new();
    for stroke in &strokes {
        stroke.emit(px_to_pt, &mut segments, &mut marks);
    }
    let pt_per_mm = POINTS_PER_INCH / MM_PER_INCH;
    mark_lines_drawn_broken(
        &mut segments,
        &marks,
        BROKEN_LINE_OFFSET_TOLERANCE_MM * pt_per_mm,
    );
    segments = merge_collinear(segments, MERGE_GAP_MM * pt_per_mm);
    sort_by_less(&mut segments, |a, b| segment_length(a) > segment_length(b));
    RasterTrace {
        segments,
        confidence: (explained as f64 / foreground_count as f64).min(1.0),
        skew_degrees,
    }
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

fn offset_index(p: usize, (dx, dy): (isize, isize), width: usize, height: usize) -> Option<usize> {
    let (x, y) = ((p % width) as isize + dx, (p / width) as isize + dy);
    (x >= 0 && y >= 0 && (x as usize) < width && (y as usize) < height)
        .then(|| y as usize * width + x as usize)
}

fn ring_at(image: &[u8], p: usize, width: usize, height: usize) -> [bool; 8] {
    let mut ring = [false; 8];
    for (k, offset) in RING.iter().enumerate() {
        ring[k] = offset_index(p, *offset, width, height).is_some_and(|q| image[q] != 0);
    }
    ring
}

/// The separate runs of ink round a pixel, its neighbours joined where they
/// touch: consecutive places on the ring, and two side neighbours across the
/// corner between them.
fn neighbour_components(ring: &[bool; 8]) -> usize {
    let mut seen = [false; 8];
    let mut components = 0;
    for start in 0..8 {
        if !ring[start] || seen[start] {
            continue;
        }
        components += 1;
        seen[start] = true;
        let mut stack = vec![start];
        while let Some(k) = stack.pop() {
            let mut touching = vec![(k + 1) % 8, (k + 7) % 8];
            if k % 2 == 0 {
                touching.extend([(k + 2) % 8, (k + 6) % 8]);
            }
            for j in touching {
                if ring[j] && !seen[j] {
                    seen[j] = true;
                    stack.push(j);
                }
            }
        }
    }
    components
}

fn zhang_suen_deletable(
    image: &[u8],
    p: usize,
    width: usize,
    height: usize,
    second_pass: bool,
) -> bool {
    let ring = ring_at(image, p, width, height);
    let neighbours = ring.iter().filter(|ink| **ink).count();
    if !(2..=6).contains(&neighbours) {
        return false;
    }
    let transitions = (0..8).filter(|&k| !ring[k] && ring[(k + 1) % 8]).count();
    if transitions != 1 {
        return false;
    }
    let (p2, p4, p6, p8) = (ring[0], ring[2], ring[4], ring[6]);
    if second_pass {
        !(p2 && p8 && (p4 || p6))
    } else {
        !(p4 && p6 && (p2 || p8))
    }
}

/// Zhang–Suen thinning to a skeleton of 1s on 0s. Only pixels on the ink's
/// edge are examined each pass, so a page of thin lines thins in about the time
/// it takes to read it once.
fn thin_zhang_suen(mask: &[bool], width: usize, height: usize) -> Vec<u8> {
    const ON_EDGE: u8 = 2;
    let mut image: Vec<u8> = mask.iter().map(|ink| u8::from(*ink)).collect();
    let mut edge = Vec::new();
    for p in 0..image.len() {
        if image[p] != 0 && ring_at(&image, p, width, height).iter().any(|ink| !ink) {
            image[p] = ON_EDGE;
            edge.push(p);
        }
    }
    loop {
        let mut changed = false;
        for second_pass in [false, true] {
            let deleted: Vec<usize> = edge
                .iter()
                .copied()
                .filter(|&p| zhang_suen_deletable(&image, p, width, height, second_pass))
                .collect();
            if deleted.is_empty() {
                continue;
            }
            changed = true;
            for &p in &deleted {
                image[p] = 0;
            }
            for &p in &deleted {
                for offset in RING {
                    if let Some(q) = offset_index(p, offset, width, height)
                        && image[q] == 1
                    {
                        image[q] = ON_EDGE;
                        edge.push(q);
                    }
                }
            }
            edge.retain(|&p| image[p] != 0);
        }
        if !changed {
            break;
        }
    }
    for value in &mut image {
        *value = u8::from(*value != 0);
    }
    image
}

/// Zhang–Suen leaves a two-pixel staircase wherever a line steps diagonally.
/// A pixel in the inside corner of such a step joins nothing its neighbours do
/// not already join, so it is removed and the line is one pixel wide.
fn remove_staircases(image: &mut [u8], width: usize, height: usize) {
    for p in 0..image.len() {
        if image[p] == 0 {
            continue;
        }
        let ring = ring_at(image, p, width, height);
        let sides = [ring[0], ring[2], ring[4], ring[6]];
        let inside_corner = (0..4).any(|k| sides[k] && sides[(k + 1) % 4]);
        let open_side = sides.iter().any(|ink| !ink);
        if inside_corner && open_side && neighbour_components(&ring) == 1 {
            image[p] = 0;
        }
    }
}

struct Node {
    centre: Point,
}

struct Edge {
    a: Option<usize>,
    b: Option<usize>,
    pixels: Vec<u32>,
    closed: bool,
}

struct SkeletonGraph {
    width: usize,
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

/// A run of skeleton between two places that are not a plain bend. An end
/// that stops at a junction still meeting three or more runs stops inside
/// another stroke's ink.
struct Chain {
    points: Vec<Point>,
    start: Option<usize>,
    end: Option<usize>,
    start_in_ink: bool,
    end_in_ink: bool,
    closed: bool,
}

fn pixel_centre(p: usize, width: usize) -> Point {
    ((p % width) as f64 + 0.5, (p / width) as f64 + 0.5)
}

/// Follow a one-pixel skeleton as a graph: an end is a pixel with one run of
/// ink round it, a junction one with three or more (touching junction pixels
/// are one junction), and an edge the pixels walked from one to the next.
fn follow_skeleton(image: &mut [u8], width: usize, height: usize) -> SkeletonGraph {
    let skeleton: Vec<usize> = (0..image.len()).filter(|&p| image[p] != 0).collect();
    let mut ends = Vec::new();
    let mut junctions = Vec::new();
    let mut isolated = Vec::new();
    for &p in &skeleton {
        let ring = ring_at(image, p, width, height);
        match neighbour_components(&ring) {
            0 => isolated.push(p),
            1 => ends.push(p),
            2 => {}
            _ => junctions.push(p),
        }
    }
    for p in isolated {
        image[p] = 0;
    }
    let mut nodes = Vec::new();
    let mut node_pixels: Vec<Vec<usize>> = Vec::new();
    let mut node_of: HashMap<usize, usize> = HashMap::new();
    for p in ends {
        node_of.insert(p, nodes.len());
        nodes.push(Node {
            centre: pixel_centre(p, width),
        });
        node_pixels.push(vec![p]);
        image[p] = NODE;
    }
    let junction_set: std::collections::HashSet<usize> = junctions.iter().copied().collect();
    for &seed in &junctions {
        if node_of.contains_key(&seed) {
            continue;
        }
        let id = nodes.len();
        let mut cluster = vec![seed];
        node_of.insert(seed, id);
        let mut next = 0;
        while next < cluster.len() {
            let p = cluster[next];
            next += 1;
            for offset in RING {
                if let Some(q) = offset_index(p, offset, width, height)
                    && junction_set.contains(&q)
                    && !node_of.contains_key(&q)
                {
                    node_of.insert(q, id);
                    cluster.push(q);
                }
            }
        }
        let (mut sx, mut sy) = (0.0, 0.0);
        for &p in &cluster {
            let (x, y) = pixel_centre(p, width);
            sx += x;
            sy += y;
            image[p] = NODE;
        }
        nodes.push(Node {
            centre: (sx / cluster.len() as f64, sy / cluster.len() as f64),
        });
        node_pixels.push(cluster);
    }

    let mut edges = Vec::new();
    for (id, pixels) in node_pixels.iter().enumerate() {
        for &p in pixels {
            for offset in WALK_ORDER {
                if let Some(q) = offset_index(p, offset, width, height)
                    && image[q] == SKELETON
                {
                    edges.push(walk_edge(image, width, height, &node_of, Some(id), q));
                }
            }
        }
    }
    for &p in &skeleton {
        if image[p] == SKELETON {
            let mut edge = walk_edge(image, width, height, &node_of, None, p);
            if edge.a.is_none() && edge.b.is_none() && edge.pixels.len() > 2 {
                let first = edge.pixels[0] as usize;
                let last = *edge.pixels.last().unwrap() as usize;
                edge.closed = RING
                    .iter()
                    .any(|offset| offset_index(last, *offset, width, height) == Some(first));
            }
            edges.push(edge);
        }
    }
    SkeletonGraph {
        width,
        nodes,
        edges,
    }
}

fn walk_edge(
    image: &mut [u8],
    width: usize,
    height: usize,
    node_of: &HashMap<usize, usize>,
    start: Option<usize>,
    first: usize,
) -> Edge {
    image[first] = VISITED;
    let mut pixels = vec![first as u32];
    let mut current = first;
    loop {
        let mut next = None;
        let mut arrived = None;
        for offset in WALK_ORDER {
            let Some(q) = offset_index(current, offset, width, height) else {
                continue;
            };
            match image[q] {
                SKELETON if next.is_none() => next = Some(q),
                NODE => {
                    let node = node_of[&q];
                    if arrived.is_none()
                        && (Some(node) != start || pixels.len() >= SELF_LOOP_MIN_PIXELS)
                    {
                        arrived = Some(node);
                    }
                }
                _ => {}
            }
        }
        if arrived.is_some() {
            return Edge {
                a: start,
                b: arrived,
                pixels,
                closed: false,
            };
        }
        let Some(q) = next else {
            return Edge {
                a: start,
                b: None,
                pixels,
                closed: false,
            };
        };
        image[q] = VISITED;
        pixels.push(q as u32);
        current = q;
    }
}

fn polyline_length(points: &[Point]) -> f64 {
    points
        .windows(2)
        .map(|pair| hypot(pair[1].0 - pair[0].0, pair[1].1 - pair[0].1))
        .sum()
}

impl SkeletonGraph {
    fn edge_points(&self, edge: &Edge, forward: bool) -> Vec<Point> {
        let mut points = Vec::with_capacity(edge.pixels.len() + 2);
        if let Some(a) = edge.a {
            points.push(self.nodes[a].centre);
        }
        points.extend(
            edge.pixels
                .iter()
                .map(|p| pixel_centre(*p as usize, self.width)),
        );
        if let Some(b) = edge.b {
            points.push(self.nodes[b].centre);
        }
        if !forward {
            points.reverse();
        }
        points
    }

    fn degrees(&self, alive: &[bool]) -> Vec<usize> {
        let mut degree = vec![0; self.nodes.len()];
        for (edge, _) in self.edges.iter().zip(alive).filter(|(_, alive)| **alive) {
            for node in [edge.a, edge.b].into_iter().flatten() {
                degree[node] += 1;
            }
        }
        degree
    }

    /// The corner branches thinning grows into a band's corners are cut off,
    /// all judged on the graph as thinned, and the edges left are joined
    /// through every node where only two of them meet.
    fn chains(&self, grid: &Grid, max_width_px: f64) -> Vec<Chain> {
        let all = vec![true; self.edges.len()];
        let degree = self.degrees(&all);
        let mut node_width: HashMap<usize, f64> = HashMap::new();
        let mut alive = all.clone();
        for (index, edge) in self.edges.iter().enumerate() {
            let free = |node: Option<usize>| node.is_none_or(|n| degree[n] == 1);
            let junction = |node: Option<usize>| node.is_some_and(|n| degree[n] >= 3);
            let hub = if free(edge.a) && junction(edge.b) {
                edge.b
            } else if free(edge.b) && junction(edge.a) {
                edge.a
            } else {
                continue;
            };
            let hub = hub.unwrap();
            let width = *node_width
                .entry(hub)
                .or_insert_with(|| 2.0 * grid.clearance(self.nodes[hub].centre, max_width_px));
            let limit = SPUR_MIN_LENGTH_PX.max(SPUR_LENGTH_PER_WIDTH * width);
            if polyline_length(&self.edge_points(edge, true)) <= limit {
                alive[index] = false;
            }
        }
        let degree = self.degrees(&alive);
        let mut at_node: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for (index, edge) in self.edges.iter().enumerate() {
            if alive[index] {
                for node in [edge.a, edge.b].into_iter().flatten() {
                    at_node[node].push(index);
                }
            }
        }
        let mut used = vec![false; self.edges.len()];
        let mut chains = Vec::new();
        for first in 0..self.edges.len() {
            if !alive[first] || used[first] {
                continue;
            }
            used[first] = true;
            let edge = &self.edges[first];
            if edge.closed {
                chains.push(Chain {
                    points: self.edge_points(edge, true),
                    start: None,
                    end: None,
                    start_in_ink: false,
                    end_in_ink: false,
                    closed: true,
                });
                continue;
            }
            let mut points = self.edge_points(edge, true);
            let mut closed = false;
            let mut end = edge.b;
            while let Some(node) = end {
                if degree[node] != 2 {
                    break;
                }
                let Some(&next) = at_node[node].iter().find(|&&e| !used[e]) else {
                    closed = edge.a == Some(node);
                    break;
                };
                used[next] = true;
                let following = &self.edges[next];
                let forward = following.a == Some(node);
                points.extend(self.edge_points(following, forward).into_iter().skip(1));
                end = if forward { following.b } else { following.a };
                if end == edge.a && end.is_some_and(|n| degree[n] == 2) {
                    closed = true;
                    break;
                }
            }
            let mut start = edge.a;
            if !closed {
                while let Some(node) = start {
                    if degree[node] != 2 {
                        break;
                    }
                    let Some(&previous) = at_node[node].iter().find(|&&e| !used[e]) else {
                        break;
                    };
                    used[previous] = true;
                    let preceding = &self.edges[previous];
                    let forward = preceding.b == Some(node);
                    let mut before = self.edge_points(preceding, forward);
                    before.pop();
                    before.extend(points);
                    points = before;
                    start = if forward { preceding.a } else { preceding.b };
                }
            }
            if closed {
                points.pop();
                start = None;
                end = None;
            }
            let in_ink = |node: Option<usize>| node.is_some_and(|n| degree[n] >= 3);
            chains.push(Chain {
                points,
                start,
                end,
                start_in_ink: in_ink(start),
                end_in_ink: in_ink(end),
                closed,
            });
        }
        chains
    }
}

/// Douglas–Peucker: the vertices of an open polyline that keep every other
/// point within tolerance of the straight runs between them.
fn simplify(points: &[Point], tolerance: f64) -> Vec<usize> {
    if points.len() < 3 {
        return (0..points.len()).collect();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    let mut stack = vec![(0, points.len() - 1)];
    while let Some((from, to)) = stack.pop() {
        let (a, b) = (points[from], points[to]);
        let length = hypot(b.0 - a.0, b.1 - a.1);
        let mut worst = (0.0, from);
        for (k, p) in points.iter().enumerate().take(to).skip(from + 1) {
            let distance = if length == 0.0 {
                hypot(p.0 - a.0, p.1 - a.1)
            } else {
                ((b.0 - a.0) * (a.1 - p.1) - (a.0 - p.0) * (b.1 - a.1)).abs() / length
            };
            if distance > worst.0 {
                worst = (distance, k);
            }
        }
        if worst.0 > tolerance {
            keep[worst.1] = true;
            stack.push((from, worst.1));
            stack.push((worst.1, to));
        }
    }
    (0..points.len()).filter(|&k| keep[k]).collect()
}

/// A straight stroke, in pixel coordinates, on the line fitted to its pixels.
#[derive(Clone, Copy)]
struct Stroke {
    a: Point,
    b: Point,
    half_width: f64,
    band: bool,
    /// A band's two faces, either side of its centre line a→b.
    faces: [(Point, Point); 2],
    cap_start: bool,
    cap_end: bool,
}

/// The total-least-squares line through a run of pixels: its centroid and
/// unit direction, pointing from the run's first pixel to its last.
fn fit_line(points: &[Point]) -> (Point, Point) {
    let n = points.len() as f64;
    let (mx, my) = points
        .iter()
        .fold((0.0, 0.0), |(sx, sy), p| (sx + p.0 / n, sy + p.1 / n));
    let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
    for p in points {
        sxx += (p.0 - mx) * (p.0 - mx);
        syy += (p.1 - my) * (p.1 - my);
        sxy += (p.0 - mx) * (p.1 - my);
    }
    let angle = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    let mut direction = (angle.cos(), angle.sin());
    let (first, last) = (points[0], points[points.len() - 1]);
    if direction.0 * (last.0 - first.0) + direction.1 * (last.1 - first.1) < 0.0 {
        direction = (-direction.0, -direction.1);
    }
    ((mx, my), direction)
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    values[values.len() / 2]
}

/// The ink's two edges either side of a fitted line, as signed offsets along
/// its normal: each sampled pixel is scanned out both ways to the paper.
fn ink_edges(
    grid: &Grid,
    points: &[Point],
    centre: Point,
    normal: Point,
    max_px: f64,
) -> (f64, f64) {
    let step = (points.len() / WIDTH_SAMPLES_PER_STROKE).max(1);
    let (mut lows, mut highs) = (Vec::new(), Vec::new());
    for p in points.iter().step_by(step) {
        let offset = (p.0 - centre.0) * normal.0 + (p.1 - centre.1) * normal.1;
        let reach = |sign: f64| {
            let mut t = 1.0;
            while t <= max_px && grid.ink_at(p.0 + sign * normal.0 * t, p.1 + sign * normal.1 * t) {
                t += 1.0;
            }
            t - 0.5
        };
        lows.push(offset - reach(-1.0));
        highs.push(offset + reach(1.0));
    }
    (median(&mut lows), median(&mut highs))
}

fn intersect(p: Point, d: Point, q: Point, e: Point) -> Option<Point> {
    let cross = d.0 * e.1 - d.1 * e.0;
    if cross.abs() < (CORNER_MIN_TURN_DEG * PI / 180.0).sin() {
        return None;
    }
    let t = ((q.0 - p.0) * e.1 - (q.1 - p.1) * e.0) / cross;
    Some((p.0 + d.0 * t, p.1 + d.1 * t))
}

fn project(p: Point, centre: Point, direction: Point) -> Point {
    let t = (p.0 - centre.0) * direction.0 + (p.1 - centre.1) * direction.1;
    (centre.0 + direction.0 * t, centre.1 + direction.1 * t)
}

fn unit(a: Point, b: Point) -> Point {
    let length = hypot(b.0 - a.0, b.1 - a.1);
    if length == 0.0 {
        (1.0, 0.0)
    } else {
        ((b.0 - a.0) / length, (b.1 - a.1) / length)
    }
}

/// A chain of skeleton pixels as straight strokes: simplified, each run
/// refitted to its own pixels and measured across, consecutive strokes meeting
/// where their lines cross, and every end that stops in ink carried out to the
/// ink's edge.
fn chain_strokes(chain: &Chain, graph: &SkeletonGraph, grid: &Grid, px_per_mm: f64) -> Vec<Stroke> {
    let points = &chain.points;
    let length = polyline_length(points);
    let attached = chain.start_in_ink && chain.end_in_ink;
    if points.len() < 2 || (!attached && length < MIN_STROKE_LENGTH_MM * px_per_mm) {
        return Vec::new();
    }
    let vertices = if chain.closed {
        let far = (1..points.len())
            .max_by(|&i, &j| {
                let di = hypot(points[i].0 - points[0].0, points[i].1 - points[0].1);
                let dj = hypot(points[j].0 - points[0].0, points[j].1 - points[0].1);
                di.partial_cmp(&dj).unwrap_or(Ordering::Equal)
            })
            .unwrap_or(0);
        let mut looped = points.clone();
        looped.push(points[0]);
        let mut vertices = simplify(&looped[..=far], SIMPLIFY_TOLERANCE_PX);
        vertices.pop();
        vertices.extend(
            simplify(&looped[far..], SIMPLIFY_TOLERANCE_PX)
                .into_iter()
                .map(|k| k + far),
        );
        vertices.pop();
        vertices
    } else {
        simplify(points, SIMPLIFY_TOLERANCE_PX)
    };
    let point_at = |k: usize| points[k % points.len()];
    let pieces = if chain.closed {
        vertices.len()
    } else {
        vertices.len() - 1
    };
    let max_px = MAX_STROKE_WIDTH_MM * px_per_mm;
    let band_px = BAND_MIN_WIDTH_MM * px_per_mm;
    struct Fitted {
        centre: Point,
        direction: Point,
        a: Point,
        b: Point,
        half_width: f64,
    }
    let mut fitted: Vec<Fitted> = Vec::with_capacity(pieces);
    for piece in 0..pieces {
        let from = vertices[piece];
        let to = if piece + 1 < vertices.len() {
            vertices[piece + 1]
        } else {
            vertices[0] + points.len()
        };
        let run: Vec<Point> = (from..=to).map(point_at).collect();
        let (centre, direction) = if run.len() == 2 {
            (
                (0.5 * (run[0].0 + run[1].0), 0.5 * (run[0].1 + run[1].1)),
                unit(run[0], run[1]),
            )
        } else {
            fit_line(&run)
        };
        let normal = (-direction.1, direction.0);
        let (low, high) = ink_edges(grid, &run, centre, normal, max_px);
        let middle = 0.5 * (low + high);
        let centre = (centre.0 + normal.0 * middle, centre.1 + normal.1 * middle);
        fitted.push(Fitted {
            centre,
            direction,
            a: project(run[0], centre, direction),
            b: project(run[run.len() - 1], centre, direction),
            half_width: 0.5 * (high - low),
        });
    }
    // Thinning cuts a square corner short with a diagonal no longer than the
    // stroke is wide; that diagonal is not a drawn edge, and the strokes either
    // side of it meet where their own lines cross.
    let mut index = 0;
    while fitted.len() > 2 && index < fitted.len() {
        let count = fitted.len();
        let interior = chain.closed || (index > 0 && index + 1 < count);
        let (before, after) = (
            &fitted[(index + count - 1) % count],
            &fitted[(index + 1) % count],
        );
        let piece = &fitted[index];
        let length = hypot(piece.b.0 - piece.a.0, piece.b.1 - piece.a.1);
        if interior && length < 2.0 * before.half_width.max(after.half_width) {
            fitted.remove(index);
        } else {
            index += 1;
        }
    }
    let pieces = fitted.len();
    let joints = if chain.closed { pieces } else { pieces - 1 };
    for joint in 0..joints {
        let next = (joint + 1) % pieces;
        let (before, after) = (&fitted[joint], &fitted[next]);
        let midpoint = (
            0.5 * (before.b.0 + after.a.0),
            0.5 * (before.b.1 + after.a.1),
        );
        let reach = 2.0 * SIMPLIFY_TOLERANCE_PX
            + before.half_width.max(after.half_width)
            + hypot(after.a.0 - before.b.0, after.a.1 - before.b.1);
        let meet = intersect(
            before.centre,
            before.direction,
            after.centre,
            after.direction,
        )
        .filter(|p| hypot(p.0 - midpoint.0, p.1 - midpoint.1) <= reach)
        .unwrap_or(midpoint);
        fitted[joint].b = meet;
        fitted[next].a = meet;
    }
    // A free end is carried on to where its ink stops; thinning eats back
    // into a stroke's ends. An end inside another stroke's ink is carried on
    // by its own half width, so it reaches the line it meets rather than
    // stopping a hair short of it (the junction's middle is where its pixels
    // average, not on either line) and stays inside that line's ink. A band's
    // faces run on as far as the ink round the junction reaches, to meet the
    // other stroke's faces.
    let reach_out = |node: Option<usize>,
                     in_ink: bool,
                     end: Point,
                     outward: Point,
                     half_width: f64,
                     band: bool| match node {
        Some(n) if in_ink && band => half_width.max(grid.clearance(graph.nodes[n].centre, max_px)),
        Some(_) if in_ink => half_width,
        _ => grid.reach(end, outward, max_px),
    };
    let mut strokes = Vec::with_capacity(pieces);
    for (piece, fit) in fitted.iter().enumerate() {
        let band = 2.0 * fit.half_width > band_px;
        let (mut a, mut b) = (fit.a, fit.b);
        let (mut cap_start, mut cap_end) = (false, false);
        if !chain.closed && piece == 0 {
            let outward = (-fit.direction.0, -fit.direction.1);
            let out = reach_out(
                chain.start,
                chain.start_in_ink,
                a,
                outward,
                fit.half_width,
                band,
            );
            a = (a.0 - fit.direction.0 * out, a.1 - fit.direction.1 * out);
            cap_start = band && !chain.start_in_ink;
        }
        if !chain.closed && piece + 1 == pieces {
            let out = reach_out(
                chain.end,
                chain.end_in_ink,
                b,
                fit.direction,
                fit.half_width,
                band,
            );
            b = (b.0 + fit.direction.0 * out, b.1 + fit.direction.1 * out);
            cap_end = band && !chain.end_in_ink;
        }
        let normal = (-fit.direction.1, fit.direction.0);
        let side = |sign: f64, p: Point| {
            (
                p.0 + sign * normal.0 * fit.half_width,
                p.1 + sign * normal.1 * fit.half_width,
            )
        };
        strokes.push(Stroke {
            a,
            b,
            half_width: fit.half_width,
            band,
            faces: [(side(-1.0, a), side(-1.0, b)), (side(1.0, a), side(1.0, b))],
            cap_start,
            cap_end,
        });
    }
    // Faces of two bands meeting in a chain run on to where they cross, so a
    // wall's corner is closed on both its faces.
    for joint in 0..joints {
        let next = (joint + 1) % pieces;
        if !strokes[joint].band || !strokes[next].band {
            continue;
        }
        let reach =
            2.0 * (SIMPLIFY_TOLERANCE_PX + strokes[joint].half_width.max(strokes[next].half_width));
        let centre = strokes[joint].b;
        for side in 0..2 {
            let (before, after) = (strokes[joint].faces[side], strokes[next].faces[side]);
            if let Some(meet) = intersect(
                before.0,
                unit(before.0, before.1),
                after.0,
                unit(after.0, after.1),
            )
            .filter(|p| hypot(p.0 - centre.0, p.1 - centre.1) <= reach)
            {
                strokes[joint].faces[side].1 = meet;
                strokes[next].faces[side].0 = meet;
            }
        }
    }
    strokes
}

impl Stroke {
    fn emit(&self, px_to_pt: f64, segments: &mut Vec<Segment>, marks: &mut Vec<MarkOnLine>) {
        let direction = unit(self.a, self.b);
        if !self.band {
            let index = segments.len();
            segments.push(raster_segment(
                self.a,
                self.b,
                2.0 * self.half_width * px_to_pt,
                px_to_pt,
            ));
            let theta = ((direction.1).atan2(direction.0) + PI / 2.0).rem_euclid(PI);
            let (normal, along) = ((theta.cos(), theta.sin()), (-theta.sin(), theta.cos()));
            let (a, b) = (
                (self.a.0 * px_to_pt, self.a.1 * px_to_pt),
                (self.b.0 * px_to_pt, self.b.1 * px_to_pt),
            );
            let (start, stop) = (a.0 * along.0 + a.1 * along.1, b.0 * along.0 + b.1 * along.1);
            marks.push(MarkOnLine {
                line: LineOnPage {
                    theta,
                    rho: a.0 * normal.0 + a.1 * normal.1,
                },
                segment: index,
                start: start.min(stop),
                stop: start.max(stop),
            });
            return;
        }
        let [(left_a, left_b), (right_a, right_b)] = self.faces;
        segments.push(raster_segment(left_a, left_b, 0.0, px_to_pt));
        segments.push(raster_segment(right_a, right_b, 0.0, px_to_pt));
        if self.cap_start {
            segments.push(raster_segment(left_a, right_a, 0.0, px_to_pt));
        }
        if self.cap_end {
            segments.push(raster_segment(left_b, right_b, 0.0, px_to_pt));
        }
    }
}

fn raster_segment(a: Point, b: Point, stroke_width: f64, px_to_pt: f64) -> Segment {
    Segment {
        x1: a.0 * px_to_pt,
        y1: a.1 * px_to_pt,
        x2: b.0 * px_to_pt,
        y2: b.1 * px_to_pt,
        owner_path: 0,
        style: PaintStyle {
            paint: RASTER_PAINT.into(),
            stroke_width,
            ..PaintStyle::default()
        }
        .into(),
    }
}

/// Mark every ink pixel within a stroke's reach, returning how many were not
/// marked before.
fn cover_capsule(covered: &mut [u8], grid: &Grid, a: Point, b: Point, radius: f64) -> usize {
    let lo_x = (a.0.min(b.0) - radius).floor().max(0.0) as usize;
    let lo_y = (a.1.min(b.1) - radius).floor().max(0.0) as usize;
    let hi_x = ((a.0.max(b.0) + radius).ceil() as usize).min(grid.width);
    let hi_y = ((a.1.max(b.1) + radius).ceil() as usize).min(grid.height);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length_squared = dx * dx + dy * dy;
    let mut added = 0;
    for y in lo_y..hi_y {
        for x in lo_x..hi_x {
            let index = y * grid.width + x;
            if covered[index] != 0 || !grid.ink[index] {
                continue;
            }
            let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
            let t = if length_squared == 0.0 {
                0.0
            } else {
                (((px - a.0) * dx + (py - a.1) * dy) / length_squared).clamp(0.0, 1.0)
            };
            if hypot(px - a.0 - t * dx, py - a.1 - t * dy) <= radius {
                covered[index] = 1;
                added += 1;
            }
        }
    }
    added
}

/// The turn of the sheet: the direction, square to the page within 45°, that
/// most of the stroke length lies along. None when the drawing is square to
/// the page or has no dominant axes.
fn sheet_skew(strokes: &[Stroke]) -> Option<f64> {
    const QUARTER_DEG: f64 = 90.0;
    let bins = (QUARTER_DEG / SKEW_BIN_DEG).round() as usize;
    let mut histogram = vec![0.0; bins];
    let mut total = 0.0;
    let mut angles = Vec::with_capacity(strokes.len());
    for stroke in strokes {
        let length = hypot(stroke.b.0 - stroke.a.0, stroke.b.1 - stroke.a.1);
        if length == 0.0 {
            continue;
        }
        let degrees = (stroke.b.1 - stroke.a.1)
            .atan2(stroke.b.0 - stroke.a.0)
            .to_degrees();
        let folded = (degrees + QUARTER_DEG / 2.0).rem_euclid(QUARTER_DEG) - QUARTER_DEG / 2.0;
        let bin = (((folded + QUARTER_DEG / 2.0) / SKEW_BIN_DEG) as usize).min(bins - 1);
        histogram[bin] += length;
        total += length;
        angles.push((folded, length));
    }
    if total == 0.0 {
        return None;
    }
    let half_window = (SKEW_WINDOW_DEG / SKEW_BIN_DEG / 2.0).round() as isize;
    let window_weight = |centre: usize| {
        (-half_window..=half_window)
            .map(|k| histogram[(centre as isize + k).rem_euclid(bins as isize) as usize])
            .sum::<f64>()
    };
    let mut peak = 0;
    let mut best = window_weight(0);
    for bin in 1..bins {
        let weight = window_weight(bin);
        if weight > best {
            best = weight;
            peak = bin;
        }
    }
    // The window's best place is a plateau as wide as the spread inside it;
    // the strokes in it are averaged and averaged again round their own mean.
    let mut skew = (peak as f64 + 0.5) * SKEW_BIN_DEG - QUARTER_DEG / 2.0;
    let mut weight = 0.0;
    for _ in 0..SKEW_REFINEMENTS {
        let mut sum = 0.0;
        weight = 0.0;
        for (angle, length) in &angles {
            let delta =
                (angle - skew + QUARTER_DEG / 2.0).rem_euclid(QUARTER_DEG) - QUARTER_DEG / 2.0;
            if delta.abs() <= SKEW_WINDOW_DEG / 2.0 + SKEW_BIN_DEG {
                weight += length;
                sum += length * (skew + delta);
            }
        }
        if weight == 0.0 {
            return None;
        }
        skew = sum / weight;
    }
    if weight < SKEW_MIN_SUPPORT * total {
        return None;
    }
    (skew.abs() >= SKEW_MIN_REPORT_DEG).then_some(skew)
}

fn mark_lines_drawn_broken(segments: &mut [Segment], marks: &[MarkOnLine], rho_tolerance: f64) {
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
    let distance_between = |a: LineOnPage, b: LineOnPage| {
        let a_distance = center_x * a.theta.cos() + center_y * a.theta.sin() - a.rho;
        let b_distance = center_x * b.theta.cos() + center_y * b.theta.sin() - b.rho;
        (a_distance - b_distance).abs()
    };
    let theta_tolerance = BROKEN_LINE_DIRECTION_TOL_DEG * PI / 180.0;
    let mut lines = Vec::new();
    let mut by_line: Vec<Vec<MarkOnLine>> = Vec::new();
    for mark in marks {
        let position = lines.iter().position(|line: &LineOnPage| {
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

/// Join strokes lying on one line whose ends nearly meet. Ends are looked up
/// in a grid of the gap's size, so a page of many strokes is not compared pair
/// by pair.
fn merge_collinear(mut segments: Vec<Segment>, merge_gap: f64) -> Vec<Segment> {
    if segments.len() < 2 || merge_gap <= 0.0 {
        return segments;
    }
    let angle_tolerance = MERGE_ANGLE_DEG * PI / 180.0;
    let cell = |x: f64, y: f64| {
        (
            (x / merge_gap).floor() as i64,
            (y / merge_gap).floor() as i64,
        )
    };
    for _ in 0..MAX_MERGE_ROUNDS {
        let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (index, segment) in segments.iter().enumerate() {
            for (x, y) in [(segment.x1, segment.y1), (segment.x2, segment.y2)] {
                grid.entry(cell(x, y)).or_default().push(index);
            }
        }
        let mut alive = vec![true; segments.len()];
        let mut changed = false;
        for first in 0..segments.len() {
            if !alive[first] {
                continue;
            }
            let ends = [
                (segments[first].x1, segments[first].y1),
                (segments[first].x2, segments[first].y2),
            ];
            let mut candidates = Vec::new();
            for (x, y) in ends {
                let (cx, cy) = cell(x, y);
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        if let Some(found) = grid.get(&(cx + dx, cy + dy)) {
                            candidates.extend(found.iter().copied().filter(|&s| s > first));
                        }
                    }
                }
            }
            candidates.sort_unstable();
            candidates.dedup();
            for second in candidates {
                if !alive[second] {
                    continue;
                }
                if let Some(merged) = try_merge_segments(
                    &segments[first],
                    &segments[second],
                    merge_gap,
                    angle_tolerance,
                ) {
                    segments[first] = merged;
                    alive[second] = false;
                    changed = true;
                }
            }
        }
        let mut index = 0;
        segments.retain(|_| {
            index += 1;
            alive[index - 1]
        });
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
    let a_length = segment_length(a);
    if a_length > 0.0 {
        let off_line = |p: (f64, f64)| {
            ((a.x2 - a.x1) * (a.y1 - p.1) - (a.x1 - p.0) * (a.y2 - a.y1)).abs() / a_length
        };
        if b_ends.iter().any(|p| off_line(*p) > gap) {
            return None;
        }
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
            paint: RASTER_PAINT.into(),
            stroke_width: a.style.stroke_width.max(b.style.stroke_width),
            dash_array: dash,
            ..PaintStyle::default()
        }
        .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_DPI: f64 = 300.0;
    const PAPER: u8 = 255;
    const INK: u8 = 0;

    struct Sheet {
        width: usize,
        height: usize,
        samples: Vec<u8>,
    }

    impl Sheet {
        fn new(width: usize, height: usize) -> Self {
            Self {
                width,
                height,
                samples: vec![PAPER; width * height],
            }
        }

        fn fill(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
            for y in y0..y1 {
                for x in x0..x1 {
                    self.samples[y * self.width + x] = INK;
                }
            }
        }

        fn trace(&self) -> RasterTrace {
            trace_raster_segments(&self.samples, self.width, self.height, self.width, TEST_DPI)
        }
    }

    fn px(value: f64) -> f64 {
        value * POINTS_PER_INCH / TEST_DPI
    }

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

    #[test]
    fn a_thin_line_is_its_centre_line_end_to_end() {
        let mut sheet = Sheet::new(400, 100);
        sheet.fill(50, 49, 350, 52);
        let traced = sheet.trace();
        assert_eq!(traced.segments.len(), 1, "{:?}", traced.segments);
        let line = &traced.segments[0];
        let (left, right) = (line.x1.min(line.x2), line.x1.max(line.x2));
        assert!((left - px(50.0)).abs() <= px(1.0), "left {left}");
        assert!((right - px(350.0)).abs() <= px(1.0), "right {right}");
        assert!((line.y1 - px(50.5)).abs() <= px(0.5) && (line.y2 - px(50.5)).abs() <= px(0.5));
        assert!((line.style.stroke_width - px(3.0)).abs() <= px(1.0));
        assert!(traced.confidence > 0.95, "confidence {}", traced.confidence);
        assert!(traced.skew_degrees.is_none());
    }

    #[test]
    fn a_band_wider_than_a_pen_is_its_two_faces_and_ends() {
        let mut sheet = Sheet::new(500, 200);
        sheet.fill(50, 80, 450, 110);
        let traced = sheet.trace();
        let long: Vec<_> = traced
            .segments
            .iter()
            .filter(|s| segment_length(s) > px(300.0))
            .collect();
        assert_eq!(long.len(), 2, "{:?}", traced.segments);
        let mut faces: Vec<f64> = long.iter().map(|s| 0.5 * (s.y1 + s.y2)).collect();
        faces.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert!(
            (faces[0] - px(80.0)).abs() <= px(1.0),
            "top face {}",
            faces[0]
        );
        assert!(
            (faces[1] - px(110.0)).abs() <= px(1.0),
            "bottom face {}",
            faces[1]
        );
        for face in long {
            let (left, right) = (face.x1.min(face.x2), face.x1.max(face.x2));
            assert!((left - px(50.0)).abs() <= px(1.5), "face starts {left}");
            assert!((right - px(450.0)).abs() <= px(1.5), "face ends {right}");
        }
        let caps = traced
            .segments
            .iter()
            .filter(|s| (s.x1 - s.x2).abs() < px(1.0) && segment_length(s) > px(25.0))
            .count();
        assert_eq!(caps, 2, "{:?}", traced.segments);
        assert!(traced.confidence > 0.95, "confidence {}", traced.confidence);
    }

    #[test]
    fn an_l_of_thin_lines_meets_at_its_corner() {
        let mut sheet = Sheet::new(300, 300);
        sheet.fill(50, 50, 250, 53);
        sheet.fill(247, 50, 250, 250);
        let traced = sheet.trace();
        assert_eq!(traced.segments.len(), 2, "{:?}", traced.segments);
        let corner = (px(248.5), px(51.5));
        let touching = traced
            .segments
            .iter()
            .filter(|s| {
                [(s.x1, s.y1), (s.x2, s.y2)]
                    .iter()
                    .any(|p| hypot(p.0 - corner.0, p.1 - corner.1) <= px(2.0))
            })
            .count();
        assert_eq!(touching, 2, "{:?}", traced.segments);
    }

    #[test]
    fn a_ragged_burr_is_not_a_line() {
        let mut sheet = Sheet::new(400, 100);
        sheet.fill(50, 49, 350, 52);
        sheet.fill(200, 47, 201, 49);
        let traced = sheet.trace();
        assert_eq!(traced.segments.len(), 1, "{:?}", traced.segments);
    }

    #[test]
    fn a_t_junction_keeps_three_arms() {
        let mut sheet = Sheet::new(400, 300);
        sheet.fill(50, 49, 350, 52);
        sheet.fill(199, 52, 202, 250);
        let traced = sheet.trace();
        assert_eq!(traced.segments.len(), 2, "{:?}", traced.segments);
        let stem = traced
            .segments
            .iter()
            .find(|s| (s.x1 - s.x2).abs() < px(1.0))
            .expect("vertical stem");
        let top = stem.y1.min(stem.y2);
        assert!(
            (top - px(50.5)).abs() <= px(1.5),
            "stem reaches the bar: {top}"
        );
    }

    #[test]
    fn a_room_walled_in_bands_has_its_faces_all_round() {
        let mut sheet = Sheet::new(600, 500);
        let (outer, wall) = ((100, 100, 500, 400), 30);
        sheet.fill(outer.0, outer.1, outer.2, outer.1 + wall);
        sheet.fill(outer.0, outer.3 - wall, outer.2, outer.3);
        sheet.fill(outer.0, outer.1, outer.0 + wall, outer.3);
        sheet.fill(outer.2 - wall, outer.1, outer.2, outer.3);
        let traced = sheet.trace();
        let covers = |horizontal: bool, at: f64, from: f64, to: f64| {
            traced.segments.iter().any(|s| {
                let (along_a, along_b, across_a, across_b) = if horizontal {
                    (s.x1, s.x2, s.y1, s.y2)
                } else {
                    (s.y1, s.y2, s.x1, s.x2)
                };
                (across_a - px(at)).abs() <= px(1.0)
                    && (across_b - px(at)).abs() <= px(1.0)
                    && along_a.min(along_b) <= px(from + 1.0)
                    && along_a.max(along_b) >= px(to - 1.0)
            })
        };
        let inner = (130.0, 130.0, 470.0, 370.0);
        assert!(
            covers(true, inner.1, inner.0, inner.2),
            "inner top {:?}",
            traced.segments
        );
        assert!(
            covers(true, inner.3, inner.0, inner.2),
            "inner bottom {:?}",
            traced.segments
        );
        assert!(
            covers(false, inner.0, inner.1, inner.3),
            "inner left {:?}",
            traced.segments
        );
        assert!(
            covers(false, inner.2, inner.1, inner.3),
            "inner right {:?}",
            traced.segments
        );
        assert!(
            covers(true, 100.0, 100.0, 500.0),
            "outer top {:?}",
            traced.segments
        );
        assert!(
            covers(false, 500.0, 100.0, 400.0),
            "outer right {:?}",
            traced.segments
        );
        assert!(traced.confidence > 0.95, "confidence {}", traced.confidence);
    }

    #[test]
    fn a_turned_sheet_reports_its_skew() {
        let mut sheet = Sheet::new(800, 800);
        let turn = 1.0_f64.to_radians();
        for row in [150.0, 400.0, 650.0] {
            for x in 100..700 {
                let y = (row + (x as f64 - 400.0) * turn.tan()).round() as usize;
                sheet.fill(x, y - 1, x + 1, y + 2);
            }
        }
        let traced = sheet.trace();
        let skew = traced.skew_degrees.expect("skew reported");
        assert!((skew - 1.0).abs() < 0.1, "skew {skew}");
    }
}
