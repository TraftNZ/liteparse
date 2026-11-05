//! Conservative glyph visibility under PDF graphics-state clips (axis-aligned
//! rectangles and straight-edged polygons) and under a near-transparent
//! ancestor form (`/gs` with a near-zero `ca` before `Do`), which paints as nothing. Clip points already
//! include the object's CTM; only ancestor form matrices remain. Unknown
//! paths/transforms preserve text, including OCR text layers.

use std::collections::{HashMap, HashSet};

use pdfium::{Matrix, Page, PageObject, PageObjectKind, RawPathSegment, RectF, SegmentKind};

use crate::extract::CharView;

const IDENTITY: Matrix = Matrix {
    a: 1.0,
    b: 0.0,
    c: 0.0,
    d: 1.0,
    e: 0.0,
    f: 0.0,
};

/// The clips one text object is painted through, in y-up page coordinates.
/// An inverted `rect` means an empty intersection. A glyph is visible only
/// where it meets the rectangle and every polygon.
#[derive(Clone)]
struct ClipShape {
    rect: Option<RectF>,
    polygons: Vec<Polygon>,
}

/// A clip bounded by straight-edged rings. PDFium does not report whether the
/// clip fills by even-odd or non-zero winding, so a point counts as inside when
/// either rule says so: text is hidden only where both agree it is outside.
#[derive(Clone)]
struct Polygon {
    rings: Vec<Vec<(f64, f64)>>,
    left: f64,
    right: f64,
    bottom: f64,
    top: f64,
}

/// Most vertices a polygon clip may have; a larger outline fails open rather
/// than cost every glyph under it a long walk.
const MAX_POLYGON_VERTICES: usize = 4096;

/// Combined form alpha below which painted text cannot be read: ink at under 5%
/// is within a few grey levels of the paper. Revit exports hide duplicate
/// annotations this way (`ca 0` and `ca 0.02`); real halftone underlays sit at
/// 0.2 and above.
const MIN_VISIBLE_ALPHA: f32 = 0.05;

/// Form nesting the transparency walk follows, as deep as the clip walk.
const MAX_FORM_DEPTH: usize = 64;

/// Only objects whose complete path stack is understood have an entry.
pub(crate) struct TextClip {
    objects: HashMap<usize, ClipShape>,
    /// Text objects painted inside forms whose combined fill alpha is below
    /// [`MIN_VISIBLE_ALPHA`]. PDFium
    /// reports a text object's own alpha, not the group's it is composited in,
    /// so the form object is the only place the fact is visible.
    transparent: HashSet<usize>,
}

impl TextClip {
    pub(crate) fn new(page: &Page) -> Self {
        let mut clips = Self {
            objects: HashMap::new(),
            transparent: HashSet::new(),
        };
        for index in 0..page.object_count() {
            if let Some(object) = page.object(index) {
                clips.walk(&object, &IDENTITY, None, 1.0, 0);
            }
        }
        clips
    }

    fn walk(
        &mut self,
        object: &PageObject,
        parent: &Matrix,
        inherited: Option<ClipShape>,
        alpha: f32,
        depth: usize,
    ) {
        if depth > 64 {
            return;
        }
        // A form composited at near-zero alpha hides everything inside it,
        // whatever the clips; judged before the clip stack so an unreadable
        // clip cannot lose the fact. Nested forms multiply.
        let alpha = match (object.kind(), object.fill_color()) {
            (PageObjectKind::Form, Some(color)) => alpha * f32::from(color.a) / f32::from(u8::MAX),
            _ => alpha,
        };
        let transparent = alpha < MIN_VISIBLE_ALPHA;
        if transparent && object.kind() == PageObjectKind::Text {
            self.transparent.insert(object.id());
        }
        let Some(paths) = object.clip_paths() else {
            self.mark_transparent(object, transparent, depth);
            return;
        };
        let mut bounds = inherited;
        for path in paths {
            let shape = bounds.get_or_insert_with(|| ClipShape {
                rect: None,
                polygons: Vec::new(),
            });
            if let Some(rect) = rectangle(&path, parent) {
                shape.rect = Some(match shape.rect {
                    None => rect,
                    Some(old) => RectF {
                        left: old.left.max(rect.left),
                        right: old.right.min(rect.right),
                        bottom: old.bottom.max(rect.bottom),
                        top: old.top.min(rect.top),
                    },
                });
            } else if let Some(polygon) = polygon(&path, parent) {
                shape.polygons.push(polygon);
            } else {
                self.mark_transparent(object, transparent, depth);
                return;
            }
        }
        match object.kind() {
            PageObjectKind::Text => {
                if let Some(bounds) = bounds {
                    self.objects.insert(object.id(), bounds);
                }
            }
            PageObjectKind::Form => {
                let Some(matrix) = object.matrix() else {
                    return;
                };
                let matrix = compose(parent, &matrix);
                if !valid_matrix(&matrix) {
                    return;
                }
                let Some(count) = object.form_object_count() else {
                    return;
                };
                for index in 0..count {
                    if let Some(child) = object.form_object(index) {
                        self.walk(&child, &matrix, bounds.clone(), alpha, depth + 1);
                    }
                }
            }
            _ => {}
        }
    }

    /// Records the text under a transparent form whose clips are not understood.
    fn mark_transparent(&mut self, object: &PageObject, transparent: bool, depth: usize) {
        if !transparent || object.kind() != PageObjectKind::Form {
            return;
        }
        let Some(count) = object.form_object_count() else {
            return;
        };
        for index in 0..count {
            let Some(child) = object.form_object(index) else {
                continue;
            };
            match child.kind() {
                PageObjectKind::Text => {
                    self.transparent.insert(child.id());
                }
                PageObjectKind::Form if depth < MAX_FORM_DEPTH => {
                    self.mark_transparent(&child, true, depth + 1);
                }
                _ => {}
            }
        }
    }

    /// Generated separators have no reliable source object and are preserved.
    /// A glyph is kept whole when its ink box, widened by [`INK_MARGIN`] of its em
    /// extent, meets the clip; glyphs without ink are judged by their loose box.
    pub(crate) fn hides(&self, cv: &CharView<'_, '_>) -> bool {
        if cv.is_generated() {
            return false;
        }
        if cv
            .text_object()
            .is_some_and(|obj| self.transparent.contains(&(obj as usize)))
        {
            return true;
        }
        let Some(shape) = cv
            .text_object()
            .and_then(|obj| self.objects.get(&(obj as usize)))
        else {
            return false;
        };
        // The ink (strict) box decides: a loose box carries the font's side bearings and
        // ascent, so it can graze a clip edge by a fraction of a point while every pixel of
        // the glyph lies outside. Whitespace and inkless glyphs keep the loose box.
        let inked = !char::from_u32(cv.unicode()).is_some_and(char::is_whitespace);
        let loose = cv.loose_char_box();
        let strict = cv
            .strict_char_box()
            .filter(|b| inked && b.right > b.left && b.top > b.bottom);
        let (glyph, margin) = match (strict, loose) {
            (Some(ink), _) => (ink, INK_MARGIN * loose.map_or(0.0, em_extent)),
            (None, Some(loose)) => (loose, 0.0),
            (None, None) => return false,
        };
        if ![glyph.left, glyph.right, glyph.bottom, glyph.top]
            .iter()
            .all(|v| v.is_finite())
            || glyph.left > glyph.right
            || glyph.bottom > glyph.top
        {
            return false;
        }
        // A tiny tolerance prevents float rounding at a clip edge losing a glyph.
        const EPS: f64 = 0.001;
        let tol = EPS + margin;
        let outside_rect = shape.rect.is_some_and(|bounds| {
            bounds.left > bounds.right
                || bounds.bottom > bounds.top
                || f64::from(glyph.right) < f64::from(bounds.left) - tol
                || f64::from(glyph.left) > f64::from(bounds.right) + tol
                || f64::from(glyph.top) < f64::from(bounds.bottom) - tol
                || f64::from(glyph.bottom) > f64::from(bounds.top) + tol
        });
        outside_rect
            || shape.polygons.iter().any(|polygon| {
                !polygon.meets(
                    f64::from(glyph.left) - tol,
                    f64::from(glyph.right) + tol,
                    f64::from(glyph.bottom) - tol,
                    f64::from(glyph.top) + tol,
                )
            })
    }
}

/// Share of a glyph's em extent by which its ink box may miss a clip and still count
/// as visible. PDFium's strict box can understate the painted outline (a substituted
/// font, quantised glyph bounds) by about 1% of the font size; that edge of ink is drawn.
const INK_MARGIN: f64 = 0.02;

/// The larger side of the loose box, about one em (ascent to descent) at any rotation.
fn em_extent(loose: RectF) -> f64 {
    let em = f64::from(
        (loose.right - loose.left)
            .abs()
            .max((loose.top - loose.bottom).abs()),
    );
    if em.is_finite() { em } else { 0.0 }
}

fn compose(p: &Matrix, c: &Matrix) -> Matrix {
    Matrix {
        a: p.a * c.a + p.c * c.b,
        b: p.b * c.a + p.d * c.b,
        c: p.a * c.c + p.c * c.d,
        d: p.b * c.c + p.d * c.d,
        e: p.a * c.e + p.c * c.f + p.e,
        f: p.b * c.e + p.d * c.f + p.f,
    }
}

fn valid_matrix(m: &Matrix) -> bool {
    [m.a, m.b, m.c, m.d, m.e, m.f].iter().all(|v| v.is_finite())
        && (f64::from(m.a) * f64::from(m.d) - f64::from(m.b) * f64::from(m.c)).abs() > 1e-12
}

/// Accept exactly one closed rectangle, never the bounding box of a compound
/// path (which can contain holes). Rotated/sheared non-axis-aligned clips fail open.
fn rectangle(path: &[RawPathSegment], parent: &Matrix) -> Option<RectF> {
    if !matches!(path.len(), 4 | 5) || !path.last()?.close || !valid_matrix(parent) {
        return None;
    }
    let mut points = Vec::with_capacity(path.len());
    for (i, segment) in path.iter().enumerate() {
        let expected = if i == 0 {
            SegmentKind::MoveTo
        } else {
            SegmentKind::LineTo
        };
        if segment.kind != Some(expected) || (i + 1 != path.len() && segment.close) {
            return None;
        }
        let (x, y) = segment.point?;
        let point = (
            parent.a * x + parent.c * y + parent.e,
            parent.b * x + parent.d * y + parent.f,
        );
        if !point.0.is_finite() || !point.1.is_finite() {
            return None;
        }
        points.push(point);
    }
    if points.len() == 5 && points.pop()? != points[0] {
        return None;
    }
    let mut horizontal = [false; 4];
    for i in 0..4 {
        let a = points[i];
        let b = points[(i + 1) % 4];
        horizontal[i] = a.1 == b.1 && a.0 != b.0;
        if !(horizontal[i] || a.0 == b.0 && a.1 != b.1) {
            return None;
        }
    }
    if (0..4).any(|i| horizontal[i] == horizontal[(i + 1) % 4]) {
        return None;
    }
    Some(RectF {
        left: points.iter().map(|p| p.0).fold(f32::INFINITY, f32::min),
        right: points.iter().map(|p| p.0).fold(f32::NEG_INFINITY, f32::max),
        bottom: points.iter().map(|p| p.1).fold(f32::INFINITY, f32::min),
        top: points.iter().map(|p| p.1).fold(f32::NEG_INFINITY, f32::max),
    })
}

/// A region bounded by straight-edged subpaths, each a MoveTo then LineTos and
/// closed by its last segment or implicitly by the next MoveTo or the path's
/// end. A path with curves or a malformed subpath is not accepted.
fn polygon(path: &[RawPathSegment], parent: &Matrix) -> Option<Polygon> {
    if path.len() < 3 || path.len() > MAX_POLYGON_VERTICES || !valid_matrix(parent) {
        return None;
    }
    let mut rings: Vec<Vec<(f64, f64)>> = Vec::new();
    for (i, segment) in path.iter().enumerate() {
        let (x, y) = segment.point?;
        let point = (
            f64::from(parent.a * x + parent.c * y + parent.e),
            f64::from(parent.b * x + parent.d * y + parent.f),
        );
        if !point.0.is_finite() || !point.1.is_finite() {
            return None;
        }
        match segment.kind? {
            SegmentKind::MoveTo => rings.push(vec![point]),
            SegmentKind::LineTo => {
                let ring = rings.last_mut()?;
                if ring.last() != Some(&point) {
                    ring.push(point);
                }
            }
            SegmentKind::BezierTo => return None,
        }
        let ends_ring = path
            .get(i + 1)
            .is_none_or(|next| next.kind == Some(SegmentKind::MoveTo));
        if segment.close && !ends_ring {
            return None;
        }
    }
    for ring in &mut rings {
        if ring.len() > 1 && ring.first() == ring.last() {
            ring.pop();
        }
    }
    // A degenerate subpath bounds nothing and paints nothing; drop it.
    rings.retain(|ring| ring.len() >= 3 && ring_area(ring).abs() > f64::EPSILON);
    if rings.is_empty() {
        return None;
    }
    let all = || rings.iter().flatten();
    Some(Polygon {
        left: all().map(|p| p.0).fold(f64::INFINITY, f64::min),
        right: all().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max),
        bottom: all().map(|p| p.1).fold(f64::INFINITY, f64::min),
        top: all().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
        rings,
    })
}

/// Twice the signed area of a ring.
fn ring_area(ring: &[(f64, f64)]) -> f64 {
    ring.iter()
        .zip(ring.iter().cycle().skip(1))
        .map(|(a, b)| a.0 * b.1 - b.0 * a.1)
        .sum()
}

impl Polygon {
    fn edges(&self) -> impl Iterator<Item = ((f64, f64), (f64, f64))> + '_ {
        self.rings.iter().flat_map(|ring| {
            ring.iter()
                .copied()
                .zip(ring.iter().copied().cycle().skip(1))
        })
    }

    /// Whether the box shares any area with the region: an edge crosses the
    /// box, or the box lies wholly inside.
    fn meets(&self, left: f64, right: f64, bottom: f64, top: f64) -> bool {
        if right < self.left || left > self.right || top < self.bottom || bottom > self.top {
            return false;
        }
        if self
            .edges()
            .any(|(a, b)| segment_meets_box(a, b, left, right, bottom, top))
        {
            return true;
        }
        // No edge touches the box, so it is wholly inside or wholly outside.
        self.contains((left, bottom))
    }

    /// Whether the point is painted. PDFium does not report a clip's fill
    /// rule, so the point is judged by both even-odd and non-zero winding;
    /// where they disagree the text is kept.
    fn contains(&self, point: (f64, f64)) -> bool {
        let mut crossings = 0_i32;
        let mut winding = 0_i32;
        for (previous, current) in self.edges() {
            if (current.1 > point.1) == (previous.1 > point.1) {
                continue;
            }
            let at = (previous.0 - current.0) * (point.1 - current.1) / (previous.1 - current.1)
                + current.0;
            if point.0 < at {
                crossings += 1;
                winding += if current.1 > previous.1 { 1 } else { -1 };
            }
        }
        let even_odd = crossings % 2 != 0;
        let non_zero = winding != 0;
        even_odd || non_zero
    }
}

/// Liang-Barsky: whether the segment a-b meets the closed box.
fn segment_meets_box(
    a: (f64, f64),
    b: (f64, f64),
    left: f64,
    right: f64,
    bottom: f64,
    top: f64,
) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut enter, mut leave) = (0.0_f64, 1.0_f64);
    for (direction, distance) in [
        (-dx, a.0 - left),
        (dx, right - a.0),
        (-dy, a.1 - bottom),
        (dy, top - a.1),
    ] {
        if direction == 0.0 {
            if distance < 0.0 {
                return false;
            }
            continue;
        }
        let ratio = distance / direction;
        if direction < 0.0 {
            enter = enter.max(ratio);
        } else {
            leave = leave.min(ratio);
        }
        if enter > leave {
            return false;
        }
    }
    true
}
