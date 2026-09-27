//! Deterministic primitive relations and shared vertices, computed in the child.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::f64::consts::PI;

use crate::compat_math::{atan2, direction_sin_cos, hypot};
use crate::compat_sort::sort_by_less;
use crate::model::{PageGeometry, Path, PrimitiveRef, Relation, TextSpan, Vertex, VertexRef};

const CONNECTIVITY_TOLERANCE: f64 = 0.5;
const PAIR_MIN_OFFSET: f64 = 1.0;
const PAIR_MAX_OFFSET: f64 = 48.0;
const PAIR_ANGLE_TOLERANCE: f64 = 2.0;
const PAIR_MIN_OVERLAP: f64 = 4.0;
const CONCENTRIC_CENTER_TOLERANCE: f64 = 2.0;
const CONCENTRIC_RADIUS_MIN_DELTA: f64 = 0.5;
const CIRCULARITY_VARIANCE_COEFFICIENT: f64 = 0.05;
const MIN_CIRCULAR_VERTICES: usize = 4;
const MIN_REGION_VERTICES: usize = 3;
const MIN_REGION_AREA: f64 = 1.0;
const ADJACENCY_TOLERANCE: f64 = 12.0;
const LABEL_MAX_DISTANCE: f64 = 40.0;
const GRID_TARGET_OCCUPANCY: f64 = 4.0;
const MIN_GRID_CELL_SIZE: f64 = 4.0;
const MAX_GRID_CELL_SIZE: f64 = 64.0;
const MAX_PAIR_EDGES: usize = 200_000;
const MAX_RELATIONS: usize = 200_000;
const DEGENERATE_SEGMENT_LENGTH_SQUARED: f64 = 1e-12;
const COLLINEAR_EPSILON: f64 = 1e-9;
const PARAMETER_LENGTH_EPSILON: f64 = 1e-9;

#[derive(Clone, Copy, Default)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Clone, Copy)]
struct LineSegment {
    first: Point,
    last: Point,
}

#[derive(Clone, Copy)]
struct BoundingBox {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl BoundingBox {
    fn empty() -> Self {
        Self {
            min_x: f64::INFINITY,
            min_y: f64::INFINITY,
            max_x: f64::NEG_INFINITY,
            max_y: f64::NEG_INFINITY,
        }
    }
    fn point(point: Point) -> Self {
        Self {
            min_x: point.x,
            min_y: point.y,
            max_x: point.x,
            max_y: point.y,
        }
    }
    fn expand(self, distance: f64) -> Self {
        Self {
            min_x: self.min_x - distance,
            min_y: self.min_y - distance,
            max_x: self.max_x + distance,
            max_y: self.max_y + distance,
        }
    }
    fn contains(self, other: Self) -> bool {
        self.min_x <= other.min_x
            && self.min_y <= other.min_y
            && self.max_x >= other.max_x
            && self.max_y >= other.max_y
    }
    fn separation_exceeds(self, other: Self, distance: f64) -> bool {
        self.min_x - other.max_x > distance
            || other.min_x - self.max_x > distance
            || self.min_y - other.max_y > distance
            || other.min_y - self.max_y > distance
    }
    fn union(self, other: Self) -> Self {
        Self {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }
}

fn points_box(points: &[Point]) -> BoundingBox {
    points.iter().fold(BoundingBox::empty(), |bbox, point| {
        bbox.union(BoundingBox::point(*point))
    })
}

struct Primitive {
    reference: PrimitiveRef,
    bbox: BoundingBox,
    free_endpoints: Vec<Point>,
    boundary: Vec<LineSegment>,
    pair_segments: Vec<LineSegment>,
    vertices: Vec<Point>,
    is_region: bool,
    circle: Option<(Point, f64)>,
}

fn path_vertices(path: &Path) -> (Vec<Point>, Vec<bool>) {
    let mut vertices = Vec::new();
    let mut straight = Vec::new();
    for op in &path.ops {
        match op.kind.as_str() {
            "moveto" | "lineto" | "curveto" => {
                vertices.push(Point { x: op.x, y: op.y });
                straight.push(op.kind == "lineto");
            }
            _ => {}
        }
    }
    (vertices, straight)
}

fn consecutive_segments(vertices: &[Point], closed: bool) -> Vec<LineSegment> {
    if vertices.len() < 2 {
        return Vec::new();
    }
    let mut segments: Vec<_> = vertices
        .windows(2)
        .map(|pair| LineSegment {
            first: pair[0],
            last: pair[1],
        })
        .collect();
    if closed {
        segments.push(LineSegment {
            first: *vertices.last().unwrap(),
            last: vertices[0],
        });
    }
    segments
}

fn straight_legs(vertices: &[Point], straight: &[bool], closed: bool) -> Vec<LineSegment> {
    if vertices.len() < 2 {
        return Vec::new();
    }
    let mut segments: Vec<_> = vertices
        .windows(2)
        .enumerate()
        .filter_map(|(index, pair)| {
            straight[index + 1].then_some(LineSegment {
                first: pair[0],
                last: pair[1],
            })
        })
        .collect();
    if closed {
        segments.push(LineSegment {
            first: *vertices.last().unwrap(),
            last: vertices[0],
        });
    }
    segments
}

fn polygon_area(vertices: &[Point]) -> f64 {
    let mut sum = 0.0;
    for index in 0..vertices.len() {
        let next = (index + 1) % vertices.len();
        sum += vertices[index].x * vertices[next].y - vertices[next].x * vertices[index].y;
    }
    sum.abs() * 0.5
}

fn circularity(vertices: &[Point]) -> Option<(Point, f64)> {
    let mut center = Point::default();
    for vertex in vertices {
        center.x += vertex.x;
        center.y += vertex.y;
    }
    center.x /= vertices.len() as f64;
    center.y /= vertices.len() as f64;
    let radii: Vec<_> = vertices
        .iter()
        .map(|vertex| hypot(vertex.x - center.x, vertex.y - center.y))
        .collect();
    let mean = radii.iter().sum::<f64>() / vertices.len() as f64;
    if mean <= 0.0 {
        return None;
    }
    let variance = radii
        .iter()
        .map(|radius| (radius - mean).powi(2))
        .sum::<f64>()
        / vertices.len() as f64;
    (variance.sqrt() / mean <= CIRCULARITY_VARIANCE_COEFFICIENT).then_some((center, mean))
}

fn build_primitives(page: &PageGeometry) -> Vec<Primitive> {
    let mut primitives = Vec::new();
    for (index, segment) in page
        .segments
        .as_deref()
        .unwrap_or_default()
        .iter()
        .enumerate()
    {
        if segment.owner_path > 0 {
            continue;
        }
        let first = Point {
            x: segment.x1,
            y: segment.y1,
        };
        let last = Point {
            x: segment.x2,
            y: segment.y2,
        };
        let edge = LineSegment { first, last };
        primitives.push(Primitive {
            reference: PrimitiveRef {
                kind: "segment".into(),
                index,
            },
            bbox: points_box(&[first, last]),
            free_endpoints: vec![first, last],
            boundary: vec![edge],
            pair_segments: vec![edge],
            vertices: Vec::new(),
            is_region: false,
            circle: None,
        });
    }
    for (index, path) in page.paths.as_deref().unwrap_or_default().iter().enumerate() {
        let (mut vertices, mut straight) = path_vertices(path);
        if vertices.is_empty() {
            continue;
        }
        if path.closed && vertices.len() > 1 {
            let first = vertices[0];
            let last = *vertices.last().unwrap();
            if hypot(last.x - first.x, last.y - first.y) <= CONNECTIVITY_TOLERANCE {
                vertices.pop();
                straight.pop();
            }
        }
        let is_region = path.closed
            && vertices.len() >= MIN_REGION_VERTICES
            && polygon_area(&vertices) >= MIN_REGION_AREA;
        let circle = if is_region && vertices.len() >= MIN_CIRCULAR_VERTICES {
            circularity(&vertices)
        } else {
            None
        };
        primitives.push(Primitive {
            reference: PrimitiveRef {
                kind: "path".into(),
                index,
            },
            bbox: points_box(&vertices),
            free_endpoints: if path.closed {
                Vec::new()
            } else {
                vec![vertices[0], *vertices.last().unwrap()]
            },
            boundary: consecutive_segments(&vertices, path.closed),
            pair_segments: straight_legs(&vertices, &straight, path.closed),
            vertices: if is_region { vertices } else { Vec::new() },
            is_region,
            circle,
        });
    }
    primitives
}

struct SpatialGrid {
    cell_size: f64,
    buckets: HashMap<(i64, i64), Vec<usize>>,
}

impl SpatialGrid {
    fn new(cell_size: f64) -> Self {
        Self {
            cell_size: if cell_size > 0.0 {
                cell_size
            } else {
                MIN_GRID_CELL_SIZE
            },
            buckets: HashMap::new(),
        }
    }
    fn coordinate(&self, x: f64, y: f64) -> (i64, i64) {
        (
            (x / self.cell_size).floor() as i64,
            (y / self.cell_size).floor() as i64,
        )
    }
    fn insert(&mut self, index: usize, bbox: BoundingBox) {
        let first = self.coordinate(bbox.min_x, bbox.min_y);
        let last = self.coordinate(bbox.max_x, bbox.max_y);
        for x in first.0..=last.0 {
            for y in first.1..=last.1 {
                self.buckets.entry((x, y)).or_default().push(index);
            }
        }
    }
    fn candidates(&self, bbox: BoundingBox) -> Vec<usize> {
        let first = self.coordinate(bbox.min_x, bbox.min_y);
        let last = self.coordinate(bbox.max_x, bbox.max_y);
        let mut seen = HashSet::new();
        for x in first.0..=last.0 {
            for y in first.1..=last.1 {
                if let Some(indices) = self.buckets.get(&(x, y)) {
                    seen.extend(indices.iter().copied());
                }
            }
        }
        let mut indices: Vec<_> = seen.into_iter().collect();
        indices.sort_unstable();
        indices
    }
}

fn grid_cell_size(extent: BoundingBox, count: usize) -> f64 {
    let width = extent.max_x - extent.min_x;
    let height = extent.max_y - extent.min_y;
    let area = width * height;
    if count == 0 || width <= 0.0 || height <= 0.0 || !area.is_finite() {
        return MIN_GRID_CELL_SIZE;
    }
    (area * GRID_TARGET_OCCUPANCY / count as f64)
        .sqrt()
        .clamp(MIN_GRID_CELL_SIZE, MAX_GRID_CELL_SIZE)
}

fn grid_for_boxes(boxes: &[BoundingBox]) -> SpatialGrid {
    let extent = boxes
        .iter()
        .fold(BoundingBox::empty(), |extent, bbox| extent.union(*bbox));
    let mut grid = SpatialGrid::new(grid_cell_size(extent, boxes.len()));
    for (index, bbox) in boxes.iter().enumerate() {
        grid.insert(index, *bbox);
    }
    grid
}

fn point_segment_distance(point: Point, edge: LineSegment) -> (f64, Point) {
    let dx = edge.last.x - edge.first.x;
    let dy = edge.last.y - edge.first.y;
    let squared = dx * dx + dy * dy;
    if squared < DEGENERATE_SEGMENT_LENGTH_SQUARED {
        return (
            hypot(point.x - edge.first.x, point.y - edge.first.y),
            edge.first,
        );
    }
    let position =
        (((point.x - edge.first.x) * dx + (point.y - edge.first.y) * dy) / squared).clamp(0.0, 1.0);
    let closest = Point {
        x: edge.first.x + position * dx,
        y: edge.first.y + position * dy,
    };
    (hypot(point.x - closest.x, point.y - closest.y), closest)
}

fn segments_intersect(a: LineSegment, b: LineSegment) -> bool {
    let orient =
        |a: Point, b: Point, c: Point| (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    let on_segment = |a: Point, b: Point, c: Point| {
        a.x.min(b.x) <= c.x && c.x <= a.x.max(b.x) && a.y.min(b.y) <= c.y && c.y <= a.y.max(b.y)
    };
    let o1 = orient(a.first, a.last, b.first);
    let o2 = orient(a.first, a.last, b.last);
    let o3 = orient(b.first, b.last, a.first);
    let o4 = orient(b.first, b.last, a.last);
    ((o1 > 0.0) != (o2 > 0.0) && (o3 > 0.0) != (o4 > 0.0))
        || (o1.abs() < COLLINEAR_EPSILON && on_segment(a.first, a.last, b.first))
        || (o2.abs() < COLLINEAR_EPSILON && on_segment(a.first, a.last, b.last))
        || (o3.abs() < COLLINEAR_EPSILON && on_segment(b.first, b.last, a.first))
        || (o4.abs() < COLLINEAR_EPSILON && on_segment(b.first, b.last, a.last))
}

fn segment_distance(a: LineSegment, b: LineSegment) -> f64 {
    if segments_intersect(a, b) {
        return 0.0;
    }
    point_segment_distance(a.first, b)
        .0
        .min(point_segment_distance(a.last, b).0)
        .min(point_segment_distance(b.first, a).0)
        .min(point_segment_distance(b.last, a).0)
}

fn confidence(distance: f64, tolerance: f64) -> i32 {
    if tolerance <= 0.0 {
        return 100;
    }
    ((1.0 - distance / tolerance).clamp(0.0, 1.0) * 100.0).round() as i32
}

fn reference_rank(reference: &PrimitiveRef) -> (usize, usize) {
    (
        match reference.kind.as_str() {
            "segment" => 0,
            "path" => 1,
            _ => 2,
        },
        reference.index,
    )
}

struct PairEdge {
    primitive: usize,
    edge: LineSegment,
    direction: f64,
    bbox: BoundingBox,
}

fn build_pair_edges(primitives: &[Primitive]) -> Vec<PairEdge> {
    let mut edges = Vec::new();
    for (index, primitive) in primitives.iter().enumerate() {
        for edge in &primitive.pair_segments {
            let dx = edge.last.x - edge.first.x;
            let dy = edge.last.y - edge.first.y;
            if hypot(dx, dy) < PAIR_MIN_OVERLAP {
                continue;
            }
            edges.push(PairEdge {
                primitive: index,
                edge: *edge,
                direction: atan2(dy, dx),
                bbox: points_box(&[edge.first, edge.last]),
            });
        }
    }
    edges
}

fn angle_delta_degrees(a: f64, b: f64) -> f64 {
    let mut delta = (a - b).abs() % PI;
    if delta > PI * 0.5 {
        delta = PI - delta;
    }
    delta * 180.0 / PI
}

fn signed_offset(a: &PairEdge, b: &PairEdge) -> f64 {
    let (sine, normal_y) = direction_sin_cos(a.direction);
    let normal_x = -sine;
    let first =
        (b.edge.first.x - a.edge.first.x) * normal_x + (b.edge.first.y - a.edge.first.y) * normal_y;
    let last =
        (b.edge.last.x - a.edge.first.x) * normal_x + (b.edge.last.y - a.edge.first.y) * normal_y;
    (first + last) * 0.5
}

fn projected_overlap(a: &PairEdge, b: &PairEdge) -> f64 {
    let (dy, dx) = direction_sin_cos(a.direction);
    let project = |point: Point| (point.x - a.edge.first.x) * dx + (point.y - a.edge.first.y) * dy;
    let a_first = project(a.edge.first);
    let a_last = project(a.edge.last);
    let b_first = project(b.edge.first);
    let b_last = project(b.edge.last);
    let low = a_first.min(a_last).max(b_first.min(b_last));
    let high = a_first.max(a_last).min(b_first.max(b_last));
    (high - low).max(0.0)
}

struct PairPartner {
    primitive: usize,
    offset: f64,
    angle_delta: f64,
    overlap: f64,
}

fn pair_partners(index: usize, edges: &[PairEdge], grid: &SpatialGrid) -> Vec<PairPartner> {
    let a = &edges[index];
    let mut positive: Option<PairPartner> = None;
    let mut negative: Option<PairPartner> = None;
    for candidate in grid.candidates(a.bbox.expand(PAIR_MAX_OFFSET)) {
        let b = &edges[candidate];
        if candidate == index || b.primitive == a.primitive {
            continue;
        }
        let angle_delta = angle_delta_degrees(a.direction, b.direction);
        if angle_delta > PAIR_ANGLE_TOLERANCE {
            continue;
        }
        let signed = signed_offset(a, b);
        let offset = signed.abs();
        if !(PAIR_MIN_OFFSET..=PAIR_MAX_OFFSET).contains(&offset) {
            continue;
        }
        let overlap = projected_overlap(a, b);
        if overlap < PAIR_MIN_OVERLAP {
            continue;
        }
        let best = if signed >= 0.0 {
            &mut positive
        } else {
            &mut negative
        };
        if best
            .as_ref()
            .is_none_or(|previous| offset < previous.offset)
        {
            *best = Some(PairPartner {
                primitive: b.primitive,
                offset,
                angle_delta,
                overlap,
            });
        }
    }
    positive.into_iter().chain(negative).collect()
}

fn detect_pairs(
    primitives: &[Primitive],
    edges: &[PairEdge],
    grid: &SpatialGrid,
) -> Vec<(Relation, f64)> {
    let mut positions = HashMap::new();
    let mut output: Vec<(Relation, f64)> = Vec::new();
    for (index, edge) in edges.iter().enumerate() {
        for partner in pair_partners(index, edges, grid) {
            let mut from = primitives[edge.primitive].reference.clone();
            let mut to = primitives[partner.primitive].reference.clone();
            if reference_rank(&from) >= reference_rank(&to) {
                std::mem::swap(&mut from, &mut to);
            }
            let key = (from.clone(), to.clone());
            let relation = Relation {
                kind: "pair".into(),
                from,
                to,
                distance: partner.offset,
                angle_delta: partner.angle_delta,
                confidence: confidence(partner.angle_delta, PAIR_ANGLE_TOLERANCE),
                ..Relation::default()
            };
            if let Some(previous) = positions.get(&key).copied() {
                let previous: usize = previous;
                if partner.overlap > output[previous].1 {
                    output[previous] = (relation, partner.overlap);
                }
            } else {
                positions.insert(key, output.len());
                output.push((relation, partner.overlap));
            }
        }
    }
    output
}

fn detect_concentric(primitives: &[Primitive]) -> Vec<Relation> {
    let mut circles: Vec<_> = primitives
        .iter()
        .filter(|primitive| primitive.circle.is_some())
        .collect();
    circles.sort_by_key(|primitive| reference_rank(&primitive.reference));
    let boxes: Vec<_> = circles
        .iter()
        .map(|primitive| BoundingBox::point(primitive.circle.unwrap().0))
        .collect();
    let grid = grid_for_boxes(&boxes);
    let mut output = Vec::new();
    for (index, a) in circles.iter().enumerate() {
        let (a_center, a_radius) = a.circle.unwrap();
        for candidate in grid.candidates(boxes[index].expand(CONCENTRIC_CENTER_TOLERANCE)) {
            if candidate <= index {
                continue;
            }
            let b = circles[candidate];
            let (b_center, b_radius) = b.circle.unwrap();
            let center_distance = hypot(a_center.x - b_center.x, a_center.y - b_center.y);
            let radius_delta = (a_radius - b_radius).abs();
            if center_distance > CONCENTRIC_CENTER_TOLERANCE
                || radius_delta < CONCENTRIC_RADIUS_MIN_DELTA
            {
                continue;
            }
            output.push(Relation {
                kind: "concentric-pair".into(),
                from: a.reference.clone(),
                to: b.reference.clone(),
                distance: radius_delta,
                confidence: confidence(center_distance, CONCENTRIC_CENTER_TOLERANCE),
                center_ax: a_center.x,
                center_ay: a_center.y,
                center_bx: b_center.x,
                center_by: b_center.y,
                radius_a: a_radius,
                radius_b: b_radius,
                ..Relation::default()
            });
        }
    }
    output
}

fn point_in_polygon(point: Point, polygon: &[Point]) -> bool {
    let mut inside = false;
    let mut previous = polygon.len() - 1;
    for (index, vertex) in polygon.iter().enumerate() {
        let last = polygon[previous];
        if (vertex.y > point.y) != (last.y > point.y)
            && point.x < (last.x - vertex.x) * (point.y - vertex.y) / (last.y - vertex.y) + vertex.x
        {
            inside = !inside;
        }
        previous = index;
    }
    inside
}

fn contains_polygon(outer: &Primitive, inner: &Primitive) -> bool {
    outer.bbox.contains(inner.bbox)
        && inner
            .vertices
            .iter()
            .all(|point| point_in_polygon(*point, &outer.vertices))
}

/// Return the exact minimum only when it can produce an adjacency.
fn region_distance_within(a: &Primitive, b: &Primitive, maximum_distance: f64) -> Option<f64> {
    let mut nearest = None;
    let mut limit = maximum_distance;
    for a_edge in &a.boundary {
        let a_bounds = points_box(&[a_edge.first, a_edge.last]);
        for b_edge in &b.boundary {
            let b_bounds = points_box(&[b_edge.first, b_edge.last]);
            if a_bounds.separation_exceeds(b_bounds, limit) {
                continue;
            }
            let distance = segment_distance(*a_edge, *b_edge);
            if distance <= limit {
                nearest = Some(distance);
                limit = distance;
                if distance == 0.0 {
                    return nearest;
                }
            }
        }
    }
    nearest
}

fn detect_regions(primitives: &[Primitive], grid: &SpatialGrid) -> Vec<Relation> {
    let mut output = Vec::new();
    for (index, a) in primitives.iter().enumerate() {
        if !a.is_region {
            continue;
        }
        for candidate in grid.candidates(a.bbox.expand(ADJACENCY_TOLERANCE)) {
            let b = &primitives[candidate];
            if candidate == index
                || !b.is_region
                || a.bbox.separation_exceeds(b.bbox, ADJACENCY_TOLERANCE)
                || reference_rank(&a.reference) >= reference_rank(&b.reference)
            {
                continue;
            }
            let contained = if contains_polygon(a, b) {
                Some((a, b))
            } else if contains_polygon(b, a) {
                Some((b, a))
            } else {
                None
            };
            if let Some((outer, inner)) = contained {
                output.push(Relation {
                    kind: "contains".into(),
                    from: outer.reference.clone(),
                    to: inner.reference.clone(),
                    confidence: 100,
                    ..Relation::default()
                });
                continue;
            }
            if let Some(distance) = region_distance_within(a, b, ADJACENCY_TOLERANCE) {
                output.push(Relation {
                    kind: "adjacent".into(),
                    from: a.reference.clone(),
                    to: b.reference.clone(),
                    distance,
                    confidence: confidence(distance, ADJACENCY_TOLERANCE),
                    ..Relation::default()
                });
            }
        }
    }
    output
}

fn primitive_point_distance(primitive: &Primitive, point: Point) -> f64 {
    if primitive.boundary.is_empty() {
        primitive
            .free_endpoints
            .iter()
            .map(|endpoint| hypot(point.x - endpoint.x, point.y - endpoint.y))
            .fold(f64::INFINITY, f64::min)
    } else {
        primitive
            .boundary
            .iter()
            .map(|edge| point_segment_distance(point, *edge).0)
            .fold(f64::INFINITY, f64::min)
    }
}

fn detect_labels(
    spans: &[TextSpan],
    primitives: &[Primitive],
    grid: &SpatialGrid,
) -> Vec<Relation> {
    let mut output = Vec::new();
    for (index, span) in spans.iter().enumerate() {
        let center = Point {
            x: (span.x0 + span.x1) * 0.5,
            y: (span.y0 + span.y1) * 0.5,
        };
        let bbox = BoundingBox {
            min_x: span.x0.min(span.x1),
            min_y: span.y0.min(span.y1),
            max_x: span.x0.max(span.x1),
            max_y: span.y0.max(span.y1),
        };
        let mut best: Option<(&Primitive, f64)> = None;
        for candidate in grid.candidates(bbox.expand(LABEL_MAX_DISTANCE)) {
            let primitive = &primitives[candidate];
            let distance = primitive_point_distance(primitive, center);
            if best.is_none_or(|(_, previous)| distance < previous) {
                best = Some((primitive, distance));
            }
        }
        if let Some((primitive, distance)) =
            best.filter(|(_, distance)| *distance <= LABEL_MAX_DISTANCE)
        {
            output.push(Relation {
                kind: "label-near".into(),
                from: PrimitiveRef {
                    kind: "text".into(),
                    index,
                },
                to: primitive.reference.clone(),
                distance,
                confidence: confidence(distance, LABEL_MAX_DISTANCE),
                ..Relation::default()
            });
        }
    }
    output
}

struct UnionFind {
    parents: Vec<usize>,
}

impl UnionFind {
    fn new(count: usize) -> Self {
        Self {
            parents: (0..count).collect(),
        }
    }
    fn find(&mut self, mut index: usize) -> usize {
        while self.parents[index] != index {
            self.parents[index] = self.parents[self.parents[index]];
            index = self.parents[index];
        }
        index
    }
    fn union(&mut self, first: usize, second: usize) {
        let first_root = self.find(first);
        let second_root = self.find(second);
        if first_root != second_root {
            self.parents[first_root] = second_root;
        }
    }
}

struct Cluster {
    x: f64,
    y: f64,
    members: Vec<usize>,
}

struct Landing {
    distance: f64,
    primitive: usize,
    segment: usize,
    closest: Point,
    parameter: f64,
}

fn segment_parameter(closest: Point, edge: LineSegment) -> f64 {
    let length = hypot(edge.last.x - edge.first.x, edge.last.y - edge.first.y);
    if length < PARAMETER_LENGTH_EPSILON {
        return 0.0;
    }
    hypot(closest.x - edge.first.x, closest.y - edge.first.y) / length
}

fn cluster_vertices(primitives: &[Primitive], grid: &SpatialGrid) -> (Vec<Vertex>, Vec<VertexRef>) {
    let mut endpoints = Vec::new();
    let mut endpoint_points = Vec::new();
    let mut index_of = HashMap::new();
    for (primitive_index, primitive) in primitives.iter().enumerate() {
        for (endpoint_index, endpoint) in primitive.free_endpoints.iter().enumerate() {
            let key = (primitive_index, endpoint_index);
            index_of.insert(key, endpoints.len());
            endpoints.push(key);
            endpoint_points.push(*endpoint);
        }
    }
    if endpoints.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut union = UnionFind::new(endpoints.len());
    for (index, key) in endpoints.iter().enumerate() {
        let point = endpoint_points[index];
        for candidate in grid.candidates(BoundingBox::point(point).expand(CONNECTIVITY_TOLERANCE)) {
            if candidate == key.0 {
                continue;
            }
            for (endpoint_index, other) in primitives[candidate].free_endpoints.iter().enumerate() {
                if hypot(point.x - other.x, point.y - other.y) <= CONNECTIVITY_TOLERANCE {
                    union.union(index, index_of[&(candidate, endpoint_index)]);
                }
            }
        }
    }
    let mut members_by_root: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for index in 0..endpoints.len() {
        members_by_root
            .entry(union.find(index))
            .or_default()
            .push(index);
    }
    let mut clusters = Vec::new();
    for members in members_by_root.values() {
        let mut members = members.clone();
        members.sort_by_key(|index| endpoints[*index]);
        let mut x = 0.0;
        let mut y = 0.0;
        for index in &members {
            x += endpoint_points[*index].x;
            y += endpoint_points[*index].y;
        }
        clusters.push(Cluster {
            x: x / members.len() as f64,
            y: y / members.len() as f64,
            members,
        });
    }
    clusters.sort_by(|a, b| {
        a.x.total_cmp(&b.x)
            .then(a.y.total_cmp(&b.y))
            .then(endpoints[a.members[0]].cmp(&endpoints[b.members[0]]))
    });
    let mut positions = Vec::new();
    let mut reference_counts = Vec::new();
    let mut vertex_by_endpoint = vec![0; endpoints.len()];
    for (vertex, cluster) in clusters.iter().enumerate() {
        positions.push(Point {
            x: cluster.x,
            y: cluster.y,
        });
        reference_counts.push(cluster.members.len());
        for endpoint in &cluster.members {
            vertex_by_endpoint[*endpoint] = vertex;
        }
    }
    let mut references: Vec<_> = endpoints
        .iter()
        .enumerate()
        .map(|(index, key)| VertexRef {
            primitive: primitives[key.0].reference.clone(),
            endpoint_index: key.1 as i32,
            vertex_index: vertex_by_endpoint[index],
            on_edge: false,
            t: 0.0,
        })
        .collect();
    let vertex_boxes: Vec<_> = positions
        .iter()
        .map(|point| BoundingBox::point(*point))
        .collect();
    let vertex_grid = grid_for_boxes(&vertex_boxes);
    for (index, key) in endpoints.iter().enumerate() {
        let root = union.find(index);
        if members_by_root[&root].len() != 1 {
            continue;
        }
        let endpoint = endpoint_points[index];
        let mut best: Option<Landing> = None;
        for candidate in
            grid.candidates(BoundingBox::point(endpoint).expand(CONNECTIVITY_TOLERANCE))
        {
            if candidate == key.0 {
                continue;
            }
            for (segment_index, edge) in primitives[candidate].boundary.iter().enumerate() {
                let (distance, closest) = point_segment_distance(endpoint, *edge);
                if distance > CONNECTIVITY_TOLERANCE
                    || hypot(closest.x - edge.first.x, closest.y - edge.first.y)
                        <= CONNECTIVITY_TOLERANCE
                    || hypot(closest.x - edge.last.x, closest.y - edge.last.y)
                        <= CONNECTIVITY_TOLERANCE
                {
                    continue;
                }
                let better = best.as_ref().is_none_or(|previous| {
                    distance < previous.distance
                        || (distance == previous.distance
                            && (candidate, segment_index) < (previous.primitive, previous.segment))
                });
                if better {
                    best = Some(Landing {
                        distance,
                        primitive: candidate,
                        segment: segment_index,
                        closest,
                        parameter: segment_parameter(closest, *edge),
                    });
                }
            }
        }
        let Some(best) = best else {
            continue;
        };
        let mut target = vertex_by_endpoint[index];
        let mut merge: Option<(usize, f64)> = None;
        for vertex in
            vertex_grid.candidates(BoundingBox::point(best.closest).expand(CONNECTIVITY_TOLERANCE))
        {
            if vertex == target || reference_counts[vertex] == 0 {
                continue;
            }
            let distance = hypot(
                positions[vertex].x - best.closest.x,
                positions[vertex].y - best.closest.y,
            );
            if distance > CONNECTIVITY_TOLERANCE {
                continue;
            }
            if merge.is_none_or(|(previous_vertex, previous_distance)| {
                distance < previous_distance
                    || (distance == previous_distance && vertex < previous_vertex)
            }) {
                merge = Some((vertex, distance));
            }
        }
        if let Some((vertex, _)) = merge {
            reference_counts[target] -= 1;
            references[index].vertex_index = vertex;
            target = vertex;
        }
        reference_counts[target] += 1;
        references.push(VertexRef {
            primitive: primitives[best.primitive].reference.clone(),
            endpoint_index: -1,
            vertex_index: target,
            on_edge: true,
            t: best.parameter,
        });
    }
    let mut live: Vec<_> = positions
        .iter()
        .enumerate()
        .filter(|(index, _)| reference_counts[*index] > 0)
        .map(|(index, point)| (index, *point))
        .collect();
    live.sort_by(|a, b| {
        a.1.x
            .total_cmp(&b.1.x)
            .then(a.1.y.total_cmp(&b.1.y))
            .then(a.0.cmp(&b.0))
    });
    let mut old_to_new = vec![0; positions.len()];
    let mut vertices = Vec::new();
    for (index, (old, point)) in live.iter().enumerate() {
        old_to_new[*old] = index;
        vertices.push(Vertex {
            index,
            x: point.x,
            y: point.y,
            degree: reference_counts[*old],
        });
    }
    for reference in &mut references {
        reference.vertex_index = old_to_new[reference.vertex_index];
    }
    (vertices, references)
}

/// Compute the same graph as the geometry helper over the page's final primitives.
/// Raster substitution must happen before calling this function.
pub fn analyze_page(page: &mut PageGeometry) {
    let primitives = build_primitives(page);
    let boxes: Vec<_> = primitives.iter().map(|primitive| primitive.bbox).collect();
    let grid = grid_for_boxes(&boxes);
    let edges = build_pair_edges(&primitives);
    let partial = edges.len() > MAX_PAIR_EDGES;
    let mut pairs = if partial {
        eprintln!(
            "geometry: page {} pairing skipped: {} edges exceeds {}",
            page.page,
            edges.len(),
            MAX_PAIR_EDGES
        );
        Vec::new()
    } else {
        let edge_boxes: Vec<_> = edges.iter().map(|edge| edge.bbox).collect();
        detect_pairs(&primitives, &edges, &grid_for_boxes(&edge_boxes))
    };
    let mut relations = detect_concentric(&primitives);
    let mut regions = detect_regions(&primitives, &grid);
    // Reuse the larger detector allocation while combining all graph rows.
    if regions.capacity() > relations.capacity() {
        std::mem::swap(&mut relations, &mut regions);
    }
    relations.append(&mut regions);
    drop(regions);
    relations.extend(detect_labels(
        page.text_spans.as_deref().unwrap_or_default(),
        &primitives,
        &grid,
    ));
    if relations.len() + pairs.len() > MAX_RELATIONS {
        let before = relations.len() + pairs.len();
        let budget = MAX_RELATIONS.saturating_sub(relations.len());
        pairs.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then(reference_rank(&a.0.from).cmp(&reference_rank(&b.0.from)))
                .then(reference_rank(&a.0.to).cmp(&reference_rank(&b.0.to)))
        });
        pairs.truncate(budget);
        eprintln!(
            "geometry: page {} relation cap {} trims {} to {}",
            page.page,
            MAX_RELATIONS,
            before,
            relations.len() + pairs.len()
        );
    }
    relations.extend(pairs.into_iter().map(|pair| pair.0));
    sort_by_less(&mut relations, |a, b| {
        a.kind
            .cmp(&b.kind)
            .then(reference_rank(&a.from).cmp(&reference_rank(&b.from)))
            .then(reference_rank(&a.to).cmp(&reference_rank(&b.to)))
            .is_lt()
    });
    let (vertices, references) = cluster_vertices(&primitives, &grid);
    page.relations = relations;
    page.vertices = vertices;
    page.vertex_refs = references;
    page.relations_partial = partial;
    page.relations_analyzed = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64) -> Primitive {
        let vertices = vec![
            Point { x, y },
            Point { x: x + 1.0, y },
            Point {
                x: x + 1.0,
                y: y + 1.0,
            },
            Point { x, y: y + 1.0 },
        ];
        Primitive {
            reference: PrimitiveRef {
                kind: "path".into(),
                index: 0,
            },
            bbox: points_box(&vertices),
            free_endpoints: Vec::new(),
            boundary: consecutive_segments(&vertices, true),
            pair_segments: Vec::new(),
            vertices,
            is_region: true,
            circle: None,
        }
    }

    #[test]
    fn adjacency_search_keeps_exact_minima_and_includes_its_distance_boundary() {
        let a = square(0.0, 0.0);
        assert_eq!(
            region_distance_within(&a, &square(1.5, 0.0), ADJACENCY_TOLERANCE),
            Some(0.5)
        );
        assert_eq!(
            region_distance_within(&a, &square(1.0, 0.0), ADJACENCY_TOLERANCE),
            Some(0.0)
        );
        assert_eq!(
            region_distance_within(
                &a,
                &square(1.0 + ADJACENCY_TOLERANCE, 0.0),
                ADJACENCY_TOLERANCE
            ),
            Some(ADJACENCY_TOLERANCE)
        );
        assert_eq!(
            region_distance_within(
                &a,
                &square(1.0 + ADJACENCY_TOLERANCE, 1.0 + ADJACENCY_TOLERANCE),
                ADJACENCY_TOLERANCE,
            ),
            None
        );
        let diagonal = region_distance_within(&a, &square(1.4, 1.4), ADJACENCY_TOLERANCE).unwrap();
        assert_eq!(diagonal, hypot(1.4 - 1.0, 1.4 - 1.0));
        assert_eq!(
            region_distance_within(&a, &square(0.5, 0.5), ADJACENCY_TOLERANCE),
            Some(0.0)
        );
    }
}
