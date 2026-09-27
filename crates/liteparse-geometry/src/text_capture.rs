//! Group native PDF text items into the line spans used by geometry readers.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::path::Path;

use lopdf::{Dictionary, Document as SourceDocument, Object};
use pdfium::{Font, Page, PageObject, PageObjectKind, RectF, TextPage};

use crate::content_paths::{Matrix, SourcePageContent, SourceTextElement, SourceTextPaint};
use crate::model::TextSpan;

const MAX_WORD_GAP_PTS: f64 = 12.0;
const MAX_CROSS_AXIS_OFFSET_PTS: f64 = 1.0;
const ROTATION_TOLERANCE_DEGREES: f64 = 0.5;
const GLYPH_MATCH_MARGIN_PTS: f64 = 0.1;
const FONT_SIZE_MATCH_FRACTION: f64 = 0.25;
const FONT_SIZE_MATCH_TOLERANCE_PTS: f64 = 0.001;
const TRAILING_SPACE_GAP_PTS: f64 = 0.1;
const REPEATED_SPACE_GAP_FACTOR: f64 = 1.5;
const MAX_FORM_DEPTH: usize = 16;
const MIN_LINE_DIRECTION_DOT: f64 = 0.999;
const MAX_BASELINE_OFFSET_EM: f64 = 0.8;
const MAX_BASELINE_GAP_EM: f64 = 0.8;
const MIN_WORD_SPACE_EM: f64 = 0.15;
const PDF_FONT_METRIC_SCALE: f64 = 1_000.0;
const PDF_FONT_WIDTH_UNITS: f32 = 1_000.0;
const PDF_FONT_WIDTH_TO_EM: f32 = 0.001;
const MAX_ENCODED_CHARACTER_BYTES: usize = 4;
const VERTICAL_TEXT_CROSS_AXIS_BOUNDS: [f64; 2] = [0.0, 1.0];

type GlyphRecord = (
    f64,
    f64,
    f64,
    f64,
    Option<usize>,
    Option<f64>,
    Option<String>,
    Option<char>,
    Option<([f64; 2], [f64; 2])>,
    Option<f64>,
    Option<f64>,
    Option<[f64; 2]>,
    usize,
    bool,
    u32,
    Option<f64>,
);

fn glyph_matches_token(token: &TextToken, glyph: &GlyphRecord) -> bool {
    let (x, y, _, _, _, _, font_name, character, baseline, _, _, _, _, vertical, code, size) =
        glyph;
    // Distinct unmapped characters share U+FFFD and can have overlapping
    // rotated boxes. Their source codes identify the actual painted glyph.
    if let [source_code] = token.char_codes.as_slice()
        && source_code != code
    {
        return false;
    }
    if *x < token.x - GLYPH_MATCH_MARGIN_PTS
        || *x > token.end_x() + GLYPH_MATCH_MARGIN_PTS
        || *y < token.y - GLYPH_MATCH_MARGIN_PTS
        || *y > token.end_y() + GLYPH_MATCH_MARGIN_PTS
    {
        return false;
    }
    if let (Some(token_font), Some(glyph_font)) = (&token.font_name, font_name)
        && token_font != glyph_font
        && !glyph_font.ends_with(&format!("+{token_font}"))
        && !token_font.ends_with(&format!("+{glyph_font}"))
    {
        return false;
    }
    if token.text.chars().count() == 1
        && character.is_some_and(|glyph| !token.text.starts_with(glyph))
    {
        return false;
    }
    if let (Some(measured), Some(visual)) = (token.font_height, size)
        && (*visual - measured).abs() > measured * FONT_SIZE_MATCH_FRACTION
    {
        return false;
    }
    if !vertical && let Some((start, end)) = baseline {
        let angle = token.normalized_rotation().to_radians();
        if angle.cos() * (end[0] - start[0]) + angle.sin() * (end[1] - start[1]) < 0.0 {
            return false;
        }
    }
    true
}

fn source_number(object: &Object) -> Option<f64> {
    match object {
        Object::Integer(value) => Some(*value as f64),
        Object::Real(value) => Some(f64::from(*value)),
        _ => None,
    }
}

fn source_dictionary<'a>(source: &'a SourceDocument, object: &'a Object) -> Option<&'a Dictionary> {
    match object {
        Object::Reference(id) => source.get_object(*id).ok()?.as_dict().ok(),
        Object::Dictionary(dictionary) => Some(dictionary),
        _ => None,
    }
}

// Base-14 substitutes use the URW font bounding-box vertical metrics when
// the PDF does not supply a font descriptor. Values are in font units.
// Source: ArtifexSoftware/mupdf, resources/fonts/urw/*.cff (FontBBox);
// these subset-font metrics differ from full URW AFM metrics for Courier.
const BASE14_VERTICAL_METRICS: &[(&str, [f64; 2])] = &[
    ("Helvetica", [1075.0, -299.0]),
    ("Helvetica-Bold", [1070.0, -307.0]),
    ("Helvetica-Oblique", [1070.0, -284.0]),
    ("Helvetica-BoldOblique", [1073.0, -309.0]),
    ("Times-Roman", [1053.0, -281.0]),
    ("Times-Bold", [1044.0, -341.0]),
    ("Times-Italic", [951.0, -270.0]),
    ("Times-BoldItalic", [972.0, -324.0]),
    ("Courier", [932.0, -317.0]),
    ("Courier-Bold", [1007.0, -393.0]),
    ("Courier-Oblique", [920.0, -317.0]),
    ("Courier-BoldOblique", [997.0, -393.0]),
    ("Symbol", [1010.0, -293.0]),
    ("ZapfDingbats", [819.0, -144.0]),
];

fn base14_font_metrics(font: &Dictionary) -> Option<[f64; 2]> {
    let name = font.get(b"BaseFont").ok()?.as_name().ok()?;
    BASE14_VERTICAL_METRICS
        .iter()
        .find_map(|(candidate, values)| {
            (candidate.as_bytes() == name)
                .then(|| values.map(|value| value / PDF_FONT_METRIC_SCALE))
        })
}

fn source_font_metrics(source: &SourceDocument, font: &Dictionary) -> Option<[f64; 2]> {
    let descriptor = if let Ok(descriptor) = font.get(b"FontDescriptor") {
        source_dictionary(source, descriptor)?
    } else if let Ok(descendants) = font.get(b"DescendantFonts") {
        let descendants = descendants.as_array().ok()?;
        let descendant = source_dictionary(source, descendants.first()?)?;
        source_dictionary(source, descendant.get(b"FontDescriptor").ok()?)?
    } else {
        return base14_font_metrics(font);
    };
    let ascent = source_number(descriptor.get(b"Ascent").ok()?)? / PDF_FONT_METRIC_SCALE;
    let descent = -(source_number(descriptor.get(b"Descent").ok()?)? / PDF_FONT_METRIC_SCALE).abs();
    (ascent.is_finite() && descent.is_finite() && ascent > descent).then_some([ascent, descent])
}

pub struct NativeTextCapture {
    pub tokens: Vec<TextToken>,
    pub characters: HashMap<usize, char>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GlyphFrame {
    pub start: [f64; 2],
    pub end: [f64; 2],
    pub vertical: [f64; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_index: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TextToken {
    #[serde(default, skip_serializing)]
    pub char_codes: Vec<u32>,
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    #[serde(default)]
    pub rotation: f64,
    pub font_name: Option<String>,
    pub font_height: Option<f64>,
    /// Geometric-mean font size from the glyph text matrix, when available.
    #[serde(default)]
    pub visual_font_size: Option<f64>,
    #[serde(default)]
    pub source_order: Option<usize>,
    #[serde(default)]
    pub baseline_start: Option<[f64; 2]>,
    #[serde(default)]
    pub baseline_end: Option<[f64; 2]>,
    #[serde(default)]
    pub glyph_vertical_size: Option<f64>,
    #[serde(default)]
    pub glyph_frames: Vec<GlyphFrame>,
    #[serde(default, skip_serializing)]
    pub vertical_writing: bool,
    #[serde(default, skip_serializing)]
    pub font_metrics: Option<[f64; 2]>,
}

impl TextToken {
    fn order_native_glyph_frames(&mut self) {
        self.glyph_frames
            .sort_by_key(|frame| frame.native_index.unwrap_or(usize::MAX));
        self.glyph_frames.dedup_by(|next, previous| {
            next.native_index.is_some() && next.native_index == previous.native_index
        });
    }

    fn end_x(&self) -> f64 {
        self.x + self.width
    }
    fn end_y(&self) -> f64 {
        self.y + self.height
    }
    fn normalized_rotation(&self) -> f64 {
        let mut degrees = self.rotation.rem_euclid(360.0);
        if degrees > 180.0 {
            degrees -= 360.0;
        }
        degrees
    }
}

fn can_join(first: &TextToken, previous: &TextToken, next: &TextToken) -> bool {
    if previous.text.is_empty() || next.text.is_empty() {
        return false;
    }
    if previous.source_order != next.source_order
        && previous.text == next.text
        && previous.baseline_start.is_some()
        && previous.baseline_start == next.baseline_start
        && previous.baseline_end == next.baseline_end
    {
        return false;
    }
    if previous.vertical_writing != next.vertical_writing {
        return false;
    }
    let rotation = previous.normalized_rotation();
    if let (Some(end), Some(start)) = (previous.baseline_end, next.baseline_start) {
        let size = next.visual_font_size.or(next.font_height).unwrap_or(0.0);
        return follows_baseline(
            first.normalized_rotation(),
            next.normalized_rotation(),
            end,
            start,
            size,
        );
    }
    if (rotation - next.normalized_rotation()).abs() > ROTATION_TOLERANCE_DEGREES {
        return false;
    }
    let (gap, cross) = if rotation.abs() < ROTATION_TOLERANCE_DEGREES {
        (next.x - previous.end_x(), (next.y - previous.y).abs())
    } else if (rotation + 90.0).abs() < ROTATION_TOLERANCE_DEGREES {
        (previous.y - next.end_y(), (next.x - previous.x).abs())
    } else if (rotation - 90.0).abs() < ROTATION_TOLERANCE_DEGREES {
        (next.y - previous.end_y(), (next.x - previous.x).abs())
    } else if (rotation.abs() - 180.0).abs() < ROTATION_TOLERANCE_DEGREES {
        (previous.x - next.end_x(), (next.y - previous.y).abs())
    } else {
        return false;
    };
    (-MAX_CROSS_AXIS_OFFSET_PTS..=MAX_WORD_GAP_PTS).contains(&gap)
        && cross <= MAX_CROSS_AXIS_OFFSET_PTS
}

fn follows_baseline(
    first_rotation: f64,
    next_rotation: f64,
    end: [f64; 2],
    start: [f64; 2],
    size: f64,
) -> bool {
    let angle = next_rotation.to_radians();
    if (angle - first_rotation.to_radians()).cos() < MIN_LINE_DIRECTION_DOT {
        return false;
    }
    let delta = [start[0] - end[0], start[1] - end[1]];
    let along = angle.cos() * delta[0] + angle.sin() * delta[1];
    let across = -angle.sin() * delta[0] + angle.cos() * delta[1];
    size > 0.0
        && along.abs() < size * MAX_BASELINE_GAP_EM
        && across.abs() < size * MAX_BASELINE_OFFSET_EM
}

fn split_glyph_gaps(token: &TextToken) -> Vec<Cow<'_, TextToken>> {
    let size = token.visual_font_size.or(token.font_height).unwrap_or(0.0);
    let Some(metrics) = token.font_metrics else {
        return vec![Cow::Borrowed(token)];
    };
    if size <= 0.0 || token.text.chars().count() != token.glyph_frames.len() {
        return vec![Cow::Borrowed(token)];
    }
    let rotation = token.normalized_rotation();
    let mut boundaries: Vec<_> = token
        .glyph_frames
        .windows(2)
        .enumerate()
        .filter_map(|(index, pair)| {
            (!follows_baseline(rotation, rotation, pair[0].end, pair[1].start, size))
                .then_some(index + 1)
        })
        .collect();
    if boundaries.is_empty() {
        return vec![Cow::Borrowed(token)];
    }
    boundaries.push(token.glyph_frames.len());
    let characters: Vec<_> = token.text.chars().collect();
    let mut start = 0;
    boundaries
        .into_iter()
        .map(|end| {
            let mut part = token.clone();
            part.text = characters[start..end].iter().collect();
            part.glyph_frames = token.glyph_frames[start..end].to_vec();
            part.char_codes = if token.char_codes.len() == characters.len() {
                token.char_codes[start..end].to_vec()
            } else {
                Vec::new()
            };
            part.baseline_start = part.glyph_frames.first().map(|frame| frame.start);
            part.baseline_end = part.glyph_frames.last().map(|frame| frame.end);
            let bounds = glyph_frame_bounds(&part.glyph_frames, metrics);
            part.x = bounds[0];
            part.y = bounds[1];
            part.width = bounds[2] - bounds[0];
            part.height = bounds[3] - bounds[1];
            start = end;
            Cow::Owned(part)
        })
        .collect()
}

fn make_span(tokens: &[&TextToken]) -> TextSpan {
    let first = tokens[0];
    let mut text = first.text.trim_start().to_owned();
    let mut weight: usize = tokens.iter().map(|token| token.text.chars().count()).sum();
    let mut total_size: f64 = tokens
        .iter()
        .map(|token| {
            token.visual_font_size.or(token.font_height).unwrap_or(0.0)
                * token.text.chars().count() as f64
        })
        .sum();
    let mut bounds = [
        tokens
            .iter()
            .map(|token| token.x)
            .fold(f64::INFINITY, f64::min),
        tokens
            .iter()
            .map(|token| token.y)
            .fold(f64::INFINITY, f64::min),
        tokens
            .iter()
            .map(|token| token.end_x())
            .fold(f64::NEG_INFINITY, f64::max),
        tokens
            .iter()
            .map(|token| token.end_y())
            .fold(f64::NEG_INFINITY, f64::max),
    ];
    for pair in tokens.windows(2) {
        let previous = pair[0];
        let next = pair[1];
        let separate_words = match (previous.baseline_end, next.baseline_start) {
            (Some(end), Some(start)) => {
                let angle = next.normalized_rotation().to_radians();
                let gap = angle.cos() * (start[0] - end[0]) + angle.sin() * (start[1] - end[1]);
                gap > next.visual_font_size.or(next.font_height).unwrap_or(0.0) * MIN_WORD_SPACE_EM
            }
            _ => true,
        };
        // The structured-text reference infers spaces only after characters
        // in these Unicode ranges. Unknown glyphs and ideographs keep their
        // painted sequence even when their baselines contain a word-sized gap.
        let allows_inferred_space =
            previous
                .text
                .trim_end()
                .chars()
                .last()
                .is_some_and(|character| {
                    character != ' '
                        && (character < '\u{700}' || ('\u{2000}'..='\u{20cf}').contains(&character))
                });
        if separate_words
            && allows_inferred_space
            && !previous.text.ends_with(' ')
            && !next.text.starts_with(' ')
        {
            text.push(' ');
            weight += 1;
            total_size += next.visual_font_size.or(next.font_height).unwrap_or(0.0);
            // An inferred space uses the following glyph's transform and font,
            // spanning from the previous pen to the following glyph origin.
            if let (Some(start), Some(end), Some(frame), Some(metrics)) = (
                previous.baseline_end,
                next.baseline_start,
                next.glyph_frames.first(),
                next.font_metrics,
            ) {
                let space = GlyphFrame {
                    start,
                    end,
                    vertical: frame.vertical,
                    native_index: None,
                };
                let space_bounds = glyph_frame_bounds(std::slice::from_ref(&space), metrics);
                bounds[0] = bounds[0].min(space_bounds[0]);
                bounds[1] = bounds[1].min(space_bounds[1]);
                bounds[2] = bounds[2].max(space_bounds[2]);
                bounds[3] = bounds[3].max(space_bounds[3]);
            }
        }
        text.push_str(&next.text);
    }
    let font_size = if weight == 0 {
        0.0
    } else {
        total_size / weight as f64
    };
    TextSpan {
        text: text.trim().to_owned(),
        x0: bounds[0],
        y0: bounds[1],
        x1: bounds[2],
        y1: bounds[3],
        font_size,
        font_name: first.font_name.clone().unwrap_or_default(),
        rotation: first.normalized_rotation(),
    }
}

#[derive(Default)]
struct ObjectOrder {
    all: HashMap<usize, usize>,
    text: Vec<usize>,
    fonts: HashMap<usize, Font>,
}

fn collect_object_order(
    object: PageObject<'_, '_>,
    depth: usize,
    next: &mut usize,
    result: &mut ObjectOrder,
) {
    if depth > MAX_FORM_DEPTH {
        return;
    }
    result.all.insert(object.id(), *next);
    if object.kind() == PageObjectKind::Text {
        result.text.push(*next);
        if let Some(font) = object.font() {
            result.fonts.insert(*next, font);
        }
    }
    *next += 1;
    if object.kind() == PageObjectKind::Form
        && let Some(count) = object.form_object_count()
    {
        for index in 0..count {
            if let Some(child) = object.form_object(index) {
                collect_object_order(child, depth + 1, next, result);
            }
        }
    }
}

/// Attach paint order and geometric-mean glyph size to native text items.
/// PDFium's text page may reorder columns while page objects retain paint order.
pub fn annotate_text_tokens(
    page: &Page<'_, '_>,
    text_page: &TextPage<'_, '_>,
    view_box: &RectF,
    content: &SourcePageContent,
    characters: &HashMap<usize, char>,
    tokens: &mut [TextToken],
) {
    let mut object_order = ObjectOrder::default();
    let mut next_order = 0;
    for index in 0..page.object_count() {
        if let Some(object) = page.object(index) {
            collect_object_order(object, 0, &mut next_order, &mut object_order);
        }
    }
    let vertical_orders: HashSet<_> = object_order
        .text
        .iter()
        .zip(content.text_paints.iter().filter(|paint| paint.has_text))
        .filter_map(|(order, paint)| paint.vertical.then_some(*order))
        .collect();
    let viewport = page.viewport_transform(view_box);
    let glyphs: Vec<GlyphRecord> = text_page
        .chars()
        .enumerate()
        .filter_map(|(native_index, character)| {
            if !characters.contains_key(&native_index)
                && char::from_u32(character.unicode()).is_none()
            {
                return None;
            }
            let bbox = character.char_box()?;
            let (x, y) = page.page_to_viewport(
                view_box,
                ((bbox.left + bbox.right) * 0.5) as f32,
                ((bbox.bottom + bbox.top) * 0.5) as f32,
            );
            let order = character
                .text_object()
                .and_then(|object| object_order.all.get(&(object as usize)).copied());
            let vertical = order.is_some_and(|order| vertical_orders.contains(&order));
            let size = character.matrix().map(|matrix| {
                let determinant = f64::from(matrix.a * matrix.d - matrix.b * matrix.c);
                character.font_size() * determinant.abs().sqrt()
            });
            let (left, _) = page.page_to_viewport(view_box, bbox.left as f32, bbox.bottom as f32);
            let (right, _) = page.page_to_viewport(view_box, bbox.right as f32, bbox.bottom as f32);
            let baseline =
                character
                    .origin()
                    .zip(character.matrix())
                    .and_then(|((ox, oy), matrix)| {
                        // Generated spaces have no source paint transform.
                        // PDFium can attach an object while supplying identity,
                        // which must not become a frame for font-metric bounds.
                        if character.is_generated() {
                            return None;
                        }
                        let object = character.text_object()?;
                        // SAFETY: the character's text object belongs to the live page;
                        // the borrowed font is used only while that page is held.
                        let font = unsafe { Font::from_text_object(object) }?;
                        let advance = f64::from(font.glyph_width_from_char_code(
                            character.char_code(),
                            character.font_size() as f32,
                        )?);
                        // PageToDevice rounds to integer device coordinates. Geometry
                        // needs the affine viewport without that subpixel quantization.
                        let (sx, sy) = viewport.transform_point(ox as f32, oy as f32);
                        let (dx, dy) = if vertical {
                            viewport.transform_vector(matrix.c, matrix.d)
                        } else {
                            viewport.transform_vector(matrix.a, matrix.b)
                        };
                        let (ex, ey) = (sx + advance as f32 * dx, sy + advance as f32 * dy);
                        let origin = [f64::from(sx), f64::from(sy)];
                        let advanced = [f64::from(ex), f64::from(ey)];
                        Some(if vertical {
                            (advanced, origin)
                        } else {
                            (origin, advanced)
                        })
                    });
            Some((
                f64::from(x),
                f64::from(y),
                f64::from(left.min(right)),
                f64::from(left.max(right)),
                order,
                size,
                character.font_name(),
                characters.get(&native_index).copied(),
                baseline,
                character.matrix().map(|matrix| {
                    let (_, start_y) = page.page_to_viewport(view_box, 0.0, 0.0);
                    let (_, end_y) = page.page_to_viewport(
                        view_box,
                        (f64::from(matrix.c) * character.font_size()) as f32,
                        (f64::from(matrix.d) * character.font_size()) as f32,
                    );
                    f64::from((end_y - start_y).abs())
                }),
                character.matrix().map(|matrix| {
                    let (dx, dy) = if vertical {
                        viewport.transform_vector(-matrix.c, -matrix.d)
                    } else {
                        viewport.transform_vector(matrix.a, matrix.b)
                    };
                    f64::from(dy).atan2(f64::from(dx)).to_degrees()
                }),
                character.matrix().map(|matrix| {
                    let (dx, dy) = if vertical {
                        viewport.transform_vector(matrix.a, matrix.b)
                    } else {
                        viewport.transform_vector(matrix.c, matrix.d)
                    };
                    [
                        f64::from(dx) * character.font_size(),
                        f64::from(dy) * character.font_size(),
                    ]
                }),
                native_index,
                vertical,
                character.char_code(),
                character.matrix().map(|matrix| {
                    f64::from(character.font_size() as f32 * matrix.scale_factors().1)
                }),
            ))
        })
        .collect();
    let mut used_glyphs = HashSet::new();
    for token in tokens {
        let single_character = token.text.chars().count() == 1;
        let mut matched_single_character = false;
        let mut total_size = 0.0;
        let mut size_count = 0;
        let mut source_font_name = None;
        // Overprinted glyphs can share a box and font while using different
        // sizes. TextItem.font_height uses the smaller principal affine
        // scale. Compare that same scale before assigning paint order;
        // the geometric-mean size remains the reported visual font size.
        let closest_size_difference = token.font_height.and_then(|measured| {
            glyphs
                .iter()
                .enumerate()
                .filter(|(index, glyph)| {
                    !used_glyphs.contains(index) && glyph_matches_token(token, glyph)
                })
                .filter_map(|(_, glyph)| glyph.15.map(|size| (size - measured).abs()))
                .min_by(f64::total_cmp)
        });
        let matching_indices: Vec<_> = glyphs
            .iter()
            .enumerate()
            .filter_map(|(index, glyph)| {
                if used_glyphs.contains(&index) || !glyph_matches_token(token, glyph) {
                    return None;
                }
                if let (Some(measured), Some(visual), Some(closest)) =
                    (token.font_height, glyph.15, closest_size_difference)
                    && (visual - measured).abs() > closest + FONT_SIZE_MATCH_TOLERANCE_PTS
                {
                    return None;
                }
                Some(index)
            })
            .collect();
        let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for index in &matching_indices {
            if let Some(order) = glyphs[*index].4 {
                groups.entry(order).or_default().push(*index);
            }
        }
        let expected: String = token
            .text
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        let matched_order = groups.iter().find_map(|(order, indices)| {
            let actual: String = indices
                .iter()
                .filter_map(|index| glyphs[*index].7)
                .filter(|character| !character.is_whitespace())
                .collect();
            (actual == expected).then_some(*order)
        });
        for glyph_index in matching_indices {
            let (
                _,
                _,
                _,
                _,
                order,
                size,
                font_name,
                _,
                baseline,
                vertical_size,
                rotation,
                vertical,
                native_index,
                vertical_writing,
                _,
                _,
            ) = &glyphs[glyph_index];
            if matched_order.is_some() && *order != matched_order {
                continue;
            }
            if single_character && matched_single_character {
                continue;
            }
            if matched_order.is_some() {
                used_glyphs.insert(glyph_index);
            }
            token.vertical_writing = *vertical_writing;
            if let Some(vertical_size) = vertical_size {
                token.glyph_vertical_size.get_or_insert(*vertical_size);
            }
            if single_character {
                used_glyphs.insert(glyph_index);
                matched_single_character = true;
            }
            if let Some((start, end)) = baseline {
                if token.baseline_start.is_none()
                    && let Some(rotation) = rotation
                {
                    token.rotation = *rotation;
                }
                token.baseline_start.get_or_insert(*start);
                token.baseline_end = Some(*end);
                if let Some(vertical) = vertical {
                    token.glyph_frames.push(GlyphFrame {
                        start: *start,
                        end: *end,
                        vertical: *vertical,
                        native_index: Some(*native_index),
                    });
                }
            }
            if let Some(order) = order {
                token.source_order = Some(
                    token
                        .source_order
                        .map_or(*order, |current| current.min(*order)),
                );
            }
            if source_font_name.is_none() {
                source_font_name = font_name.as_ref();
            }
            if let Some(size) = size
                && size.is_finite()
                && *size > 0.0
            {
                total_size += size;
                size_count += 1;
            }
        }
        if let Some(first_frame) = token.glyph_frames.first().cloned() {
            for glyph in glyphs
                .iter()
                .take_while(|glyph| glyph.8.is_none_or(|(start, _)| start != first_frame.start))
                .filter(|glyph| glyph.4 == token.source_order)
            {
                if glyph.7 != Some(' ') {
                    break;
                }
                if let (Some((start, end)), Some(vertical)) = (glyph.8, glyph.11) {
                    token.glyph_frames.push(GlyphFrame {
                        start,
                        end,
                        vertical,
                        native_index: Some(glyph.12),
                    });
                }
            }
        }
        if size_count > 0 {
            token.visual_font_size = Some(total_size / f64::from(size_count));
        }
        if let Some(name) = source_font_name {
            token.font_name = Some(name.clone());
        }
        if token.rotation.abs() < ROTATION_TOLERANCE_DEGREES {
            let matched: Vec<_> = glyphs
                .iter()
                .filter(
                    |(x, y, _, _, order, _, _, character, _, _, _, _, _, _, _, _)| {
                        *order == token.source_order
                            && *x >= token.x - GLYPH_MATCH_MARGIN_PTS
                            && *x <= token.end_x() + GLYPH_MATCH_MARGIN_PTS
                            && *y >= token.y - GLYPH_MATCH_MARGIN_PTS
                            && *y <= token.end_y() + GLYPH_MATCH_MARGIN_PTS
                            && character.is_some()
                    },
                )
                .collect();
            let glyph_text: String = matched.iter().filter_map(|glyph| glyph.7).collect();
            if glyph_text.trim_end() == token.text {
                let space_widths: Vec<_> = matched
                    .iter()
                    .filter(|glyph| glyph.7 == Some(' '))
                    .map(|glyph| glyph.3 - glyph.2)
                    .filter(|width| *width > 0.0)
                    .collect();
                if let Some(space_width) = space_widths.first().copied() {
                    let ordinary_gaps: Vec<_> = matched
                        .windows(2)
                        .filter(|pair| pair[0].7 == Some(' '))
                        .map(|pair| pair[1].2 - pair[0].3)
                        .filter(|gap| *gap >= 0.0 && *gap < space_width)
                        .collect();
                    if let Some(ordinary_gap) = ordinary_gaps.first().copied() {
                        let mut extra = 0;
                        let mut rebuilt = String::new();
                        for pair in matched.windows(2) {
                            rebuilt.push(pair[0].7.expect("matched character"));
                            if pair[0].7 == Some(' ') {
                                let gap = pair[1].2 - pair[0].3;
                                if gap > space_width * REPEATED_SPACE_GAP_FACTOR {
                                    let repeat =
                                        ((gap - ordinary_gap) / space_width).round() as usize;
                                    extra += repeat;
                                    rebuilt.push_str(&" ".repeat(repeat));
                                }
                            }
                        }
                        if let Some(last) = matched.last().and_then(|glyph| glyph.7) {
                            rebuilt.push(last);
                        }
                        if extra > 0 {
                            token.text = rebuilt.trim_end().to_owned();
                        }
                    }
                }
            }
        }
        token.order_native_glyph_frames();
        let last_native_index = token
            .glyph_frames
            .last()
            .and_then(|frame| frame.native_index);
        if let Some(glyph) = glyphs.iter().find(|glyph| {
            glyph.4 == token.source_order
                && glyph.7 == Some(' ')
                && ((token.char_codes.last() == Some(&glyph.14)
                    && last_native_index.is_some_and(|index| glyph.12 == index + 1))
                    || (token.rotation.abs() < ROTATION_TOLERANCE_DEGREES
                        && (glyph.2 - token.end_x()).abs() <= TRAILING_SPACE_GAP_PTS
                        && glyph.1 >= token.y - GLYPH_MATCH_MARGIN_PTS
                        && glyph.1 <= token.end_y() + GLYPH_MATCH_MARGIN_PTS))
        }) {
            // An owned encoded space follows native glyph order at every
            // rotation. Only the horizontal proximity fallback uses x bounds.
            if token.rotation.abs() < ROTATION_TOLERANCE_DEGREES {
                token.width = glyph.3 - token.x;
            }
            if let (Some((start, end)), Some(vertical)) = (glyph.8, glyph.11) {
                token.glyph_frames.push(GlyphFrame {
                    start,
                    end,
                    vertical,
                    native_index: Some(glyph.12),
                });
            }
        }
    }
}

#[derive(Clone)]
struct NativeGlyph {
    index: usize,
    code: u32,
    unicode: u32,
    start: [f64; 2],
}

impl NativeGlyph {
    fn continues_utf16(&self, previous: &Self) -> bool {
        if previous.index.checked_add(1) != Some(self.index)
            || self.code != previous.code
            || self.start != previous.start
        {
            return false;
        }
        let (Ok(high), Ok(low)) = (u16::try_from(previous.unicode), u16::try_from(self.unicode))
        else {
            return false;
        };
        let mut decoded = char::decode_utf16([high, low]);
        decoded.next().is_some_and(|scalar| scalar.is_ok()) && decoded.next().is_none()
    }
}

fn text_error(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn encoded_character(
    bytes: &[u8],
    paint: &SourceTextPaint,
    encoding: &str,
    native: &[NativeGlyph],
) -> Result<(u32, usize), std::io::Error> {
    if !paint.composite_font {
        return Ok((u32::from(bytes[0]), 1));
    }
    if !paint.code_spaces.is_empty() {
        let mut matched = None;
        for range in paint.code_spaces.iter() {
            let Some(encoded) = bytes.get(..range.length) else {
                continue;
            };
            let code = encoded
                .iter()
                .fold(0_u32, |value, byte| (value << u8::BITS) | u32::from(*byte));
            if (range.low..=range.high).contains(&code) {
                let candidate = (code, range.length);
                if matched.is_some_and(|previous| previous != candidate) {
                    return Err(text_error("overlapping source CMap codespace boundaries"));
                }
                matched = Some(candidate);
            }
        }
        return matched
            .ok_or_else(|| text_error("encoded character is outside source CMap codespaces"));
    }
    if encoding == "Identity-H" || encoding == "Identity-V" {
        let pair = bytes
            .get(..2)
            .ok_or_else(|| text_error("truncated Identity character"))?;
        return Ok((u32::from(u16::from_be_bytes([pair[0], pair[1]])), 2));
    }
    // Native char codes retain the encoded PDF value even when Unicode differs.
    // Accept a variable-length boundary only when that value proves it unique.
    let mut code = 0_u32;
    let mut matched = None;
    for (index, byte) in bytes.iter().take(MAX_ENCODED_CHARACTER_BYTES).enumerate() {
        code = (code << u8::BITS) | u32::from(*byte);
        if native.iter().any(|glyph| glyph.code == code) {
            if matched.is_some() {
                return Err(text_error(
                    "ambiguous source/native encoded character boundary",
                ));
            }
            matched = Some((code, index + 1));
        }
    }
    matched.ok_or_else(|| text_error("source character has no matching native encoded value"))
}

#[derive(Hash, PartialEq, Eq)]
struct PaintReplayKey {
    font: usize,
    glyphs: Vec<(u32, [u64; 6])>,
    marked_content: std::sync::Arc<[u8]>,
}

struct SourceGlyphRecovery {
    native: HashMap<usize, GlyphFrame>,
    native_spaces: HashSet<usize>,
    identical_paints: HashMap<usize, usize>,
    omitted: Vec<TextToken>,
    replacements: HashMap<usize, TextToken>,
}

fn source_text_token(
    paint: &SourceTextPaint,
    font: &Font,
    object: Option<usize>,
    frames: Vec<GlyphFrame>,
    codes: Vec<u32>,
    text: String,
) -> Result<TextToken, std::io::Error> {
    let metrics = font
        .ascent(1.0)
        .zip(font.descent(1.0))
        .ok_or_else(|| text_error("omitted glyph has no font metrics"))?;
    let bounds = glyph_frame_bounds(&frames, [f64::from(metrics.0), f64::from(metrics.1)]);
    let trm = paint.ctm.compose(paint.line.compose(Matrix([
        paint.font_size * paint.scale,
        0.0,
        0.0,
        paint.font_size,
        0.0,
        paint.rise,
    ])));
    let affine = pdfium::Matrix {
        a: trm.0[0],
        b: trm.0[1],
        c: trm.0[2],
        d: trm.0[3],
        e: trm.0[4],
        f: trm.0[5],
    };
    Ok(TextToken {
        char_codes: codes,
        text,
        x: bounds[0],
        y: bounds[1],
        width: bounds[2] - bounds[0],
        height: bounds[3] - bounds[1],
        rotation: f64::from(affine.b).atan2(f64::from(affine.a)).to_degrees(),
        font_name: paint.font_name.as_deref().map(str::to_owned),
        font_height: Some(f64::from(affine.scale_factors().1)),
        visual_font_size: Some(
            f64::from(affine.a * affine.d - affine.b * affine.c)
                .abs()
                .sqrt(),
        ),
        source_order: object,
        baseline_start: frames.first().map(|frame| frame.start),
        baseline_end: frames.last().map(|frame| frame.end),
        glyph_vertical_size: frames.first().map(|frame| frame.vertical[1].abs()),
        glyph_frames: frames,
        vertical_writing: false,
        font_metrics: None,
    })
}

fn source_glyph_frames(
    page: &Page<'_, '_>,
    text_page: &TextPage<'_, '_>,
    content: &SourcePageContent,
    order: &ObjectOrder,
    characters: &HashMap<usize, char>,
) -> Result<SourceGlyphRecovery, Box<dyn Error>> {
    let view = page
        .view_box()
        .ok_or_else(|| text_error("page has no view box"))?;
    let viewport = page.viewport_transform(&view);
    let mut native_by_object: HashMap<usize, Vec<NativeGlyph>> = HashMap::new();
    let mut font_characters = HashMap::new();
    for (index, glyph) in text_page.chars().enumerate() {
        if glyph.is_generated() {
            continue;
        }
        let Some(object) = glyph
            .text_object()
            .and_then(|object| order.all.get(&(object as usize)))
        else {
            continue;
        };
        let (x, y) = glyph
            .origin()
            .ok_or_else(|| text_error("native glyph has no origin"))?;
        let (x, y) = viewport.transform_point(x as f32, y as f32);
        if let Some(character) = characters
            .get(&index)
            .copied()
            .or_else(|| char::from_u32(glyph.unicode()).filter(|character| *character == ' '))
            && let Some(font) = order.fonts.get(object)
        {
            font_characters
                .entry((font.handle() as usize, glyph.char_code()))
                .and_modify(|known: &mut Option<char>| {
                    if *known != Some(character) {
                        *known = None;
                    }
                })
                .or_insert(Some(character));
        }
        native_by_object
            .entry(*object)
            .or_default()
            .push(NativeGlyph {
                index,
                code: glyph.char_code(),
                unicode: glyph.unicode(),
                start: [f64::from(x), f64::from(y)],
            });
    }
    let mut frames = HashMap::new();
    let mut native_spaces = HashSet::new();
    let mut omitted = Vec::new();
    let mut replacements = HashMap::new();
    let mut painted_glyphs: HashMap<PaintReplayKey, Vec<(usize, bool)>> = HashMap::new();
    let mut cursors = HashMap::new();
    let mut objects = order.text.iter();
    for paint in &content.text_paints {
        let mut matrix = if paint.show == 0 {
            paint.line
        } else {
            *cursors
                .get(&(paint.run, paint.show))
                .ok_or_else(|| text_error("source text cursor checkpoint is missing"))?
        };
        let object = paint.has_text.then(|| objects.next()).flatten();
        let font = object
            .map(|index| {
                order
                    .fonts
                    .get(index)
                    .ok_or_else(|| text_error("native text object has no font"))
            })
            .transpose()?;
        let native = object
            .and_then(|index| native_by_object.get(index))
            .map_or(&[][..], Vec::as_slice);
        let encoding = font.and_then(Font::encoding).unwrap_or_default();
        let vertical = paint.vertical;
        let mut source_codes = Vec::new();
        let mut source_spaces = HashSet::new();
        for element in &paint.elements {
            if let SourceTextElement::Bytes(bytes) = element {
                let mut offset = 0;
                while offset < bytes.len() {
                    let (code, length) =
                        encoded_character(&bytes[offset..], paint, &encoding, native)?;
                    if paint.space_codes.contains(&(code, length)) {
                        source_spaces.insert(source_codes.len());
                    }
                    source_codes.push(code);
                    offset += length;
                }
            }
        }
        let direct_order = source_codes.len() == native.len()
            && source_codes
                .iter()
                .zip(native)
                .all(|(code, glyph)| *code == glyph.code);
        let mut source_frames = Vec::new();
        for element in &paint.elements {
            match element {
                SourceTextElement::Adjustment(value) => {
                    let advance = -*value * paint.font_size * PDF_FONT_WIDTH_TO_EM;
                    matrix = translate_text(
                        matrix,
                        if vertical {
                            [0.0, advance]
                        } else {
                            [advance * paint.scale, 0.0]
                        },
                    );
                }
                SourceTextElement::Bytes(bytes) => {
                    let mut offset = 0;
                    while offset < bytes.len() {
                        let (code, length) =
                            encoded_character(&bytes[offset..], paint, &encoding, native)?;
                        let font =
                            font.ok_or_else(|| text_error("source text show has no native font"))?;
                        let nominal = font
                            .glyph_width_from_char_code(code, PDF_FONT_WIDTH_UNITS)
                            .ok_or_else(|| {
                                text_error(format!(
                                    "native font has no width for encoded character {code}"
                                ))
                            })?;
                        let width = nominal * PDF_FONT_WIDTH_TO_EM;
                        let trm = paint.ctm.compose(matrix.compose(Matrix([
                            paint.font_size * paint.scale,
                            0.0,
                            0.0,
                            paint.font_size,
                            0.0,
                            paint.rise,
                        ])));
                        let start = trm.apply([0.0, 0.0]);
                        if !vertical {
                            source_frames.push((
                                code,
                                GlyphFrame {
                                    start,
                                    end: trm.apply([f64::from(width), 0.0]),
                                    vertical: [f64::from(trm.0[2]), f64::from(trm.0[3])],
                                    native_index: None,
                                },
                            ));
                        }
                        let advance = width * paint.font_size + paint.char_space;
                        matrix = translate_text(
                            matrix,
                            if vertical {
                                [0.0, advance]
                            } else {
                                [advance * paint.scale, 0.0]
                            },
                        );
                        if length == 1 && code == u32::from(b' ') {
                            matrix = translate_text(
                                matrix,
                                if vertical {
                                    [0.0, paint.word_space]
                                } else {
                                    [paint.word_space * paint.scale, 0.0]
                                },
                            );
                        }
                        offset += length;
                    }
                }
            }
        }
        // PDFium can suppress an entire repeated paint, including a slightly
        // shifted word. Recover only codes observed consistently in this font.
        if !vertical
            && native.is_empty()
            && !source_frames.is_empty()
            && let Some(font) = font
            && let Some(text) = source_frames
                .iter()
                .map(|(code, _)| {
                    font_characters
                        .get(&(font.handle() as usize, *code))
                        .copied()
                        .flatten()
                })
                .collect::<Option<String>>()
            && text.chars().any(|character| !character.is_whitespace())
        {
            omitted.push(source_text_token(
                paint,
                font,
                object.copied(),
                source_frames
                    .iter()
                    .map(|(_, frame)| frame.clone())
                    .collect(),
                source_codes.clone(),
                text,
            )?);
        }
        if let Some(object) = object
            && !source_frames.is_empty()
        {
            let key = PaintReplayKey {
                font: font.expect("source frames require a font").handle() as usize,
                glyphs: source_frames
                    .iter()
                    .map(|(code, frame)| {
                        (
                            *code,
                            [
                                frame.start[0].to_bits(),
                                frame.start[1].to_bits(),
                                frame.end[0].to_bits(),
                                frame.end[1].to_bits(),
                                frame.vertical[0].to_bits(),
                                frame.vertical[1].to_bits(),
                            ],
                        )
                    })
                    .collect(),
                marked_content: paint.marked_content.clone(),
            };
            painted_glyphs
                .entry(key)
                .or_default()
                .push((*object, direct_order));
        }
        if !vertical {
            let mut matched_sources = HashSet::new();
            let mut native_sources = Vec::new();
            for (index, glyph) in native.iter().enumerate() {
                let continuation = index
                    .checked_sub(1)
                    .filter(|previous| glyph.continues_utf16(&native[*previous]))
                    .and_then(|previous| native_sources.get(previous).copied());
                let source = if let Some(index) = continuation {
                    source_frames.get(index).map(|source| (index, source))
                } else if direct_order {
                    source_frames.get(index).map(|source| (index, source))
                } else {
                    source_frames
                        .iter()
                        .enumerate()
                        .filter(|(index, (code, _))| {
                            *code == glyph.code && !matched_sources.contains(index)
                        })
                        .min_by(|(_, (_, left)), (_, (_, right))| {
                            squared_distance(left.start, glyph.start)
                                .total_cmp(&squared_distance(right.start, glyph.start))
                        })
                }
                .map(|(index, (_, frame))| (index, frame.clone()))
                .ok_or_else(|| text_error("native glyph has no matching source character"))?;
                matched_sources.insert(source.0);
                native_sources.push(source.0);
                let is_space = characters.get(&glyph.index).map_or_else(
                    || source_spaces.contains(&source.0) || glyph.unicode == u32::from(' '),
                    |character| *character == ' ',
                );
                if is_space {
                    native_spaces.insert(glyph.index);
                }
                let mut frame = source.1;
                frame.native_index = Some(glyph.index);
                source_frames[source.0].1.native_index = Some(glyph.index);
                frames.insert(glyph.index, frame);
            }
            // Partial suppression must retain the complete source order,
            // including a missing glyph between two surviving characters.
            // Rebuild only when every code has an authoritative, consistent
            // observed character for this exact native font.
            if !native.is_empty()
                && matched_sources.len() < source_frames.len()
                && let (Some(object), Some(font)) = (object, font)
                && let Some(text) = source_frames
                    .iter()
                    .map(|(code, _)| {
                        font_characters
                            .get(&(font.handle() as usize, *code))
                            .copied()
                            .flatten()
                    })
                    .collect::<Option<String>>()
            {
                replacements.insert(
                    *object,
                    source_text_token(
                        paint,
                        font,
                        Some(*object),
                        source_frames
                            .iter()
                            .map(|(_, frame)| frame.clone())
                            .collect(),
                        source_codes.clone(),
                        text,
                    )?,
                );
            }
            // Reading extraction discards paints containing only spaces.
            // Their actual frames still advance the structured-text line and
            // contribute its font metrics and bounds.
            if !source_frames.is_empty()
                && let (Some(object), Some(font)) = (object, font)
                && source_frames
                    .iter()
                    .enumerate()
                    .all(|(index, (code, _))| {
                        match font_characters.get(&(font.handle() as usize, *code)) {
                            Some(character) => *character == Some(' '),
                            None => {
                                source_spaces.contains(&index)
                                    || (!font.has_to_unicode()
                                        && font.char_glyph_name(*code).as_deref() == Some("space"))
                            }
                        }
                    })
            {
                replacements.insert(
                    *object,
                    source_text_token(
                        paint,
                        font,
                        Some(*object),
                        source_frames
                            .iter()
                            .map(|(_, frame)| frame.clone())
                            .collect(),
                        source_codes.clone(),
                        " ".repeat(source_frames.len()),
                    )?,
                );
            }
        }
        cursors.insert((paint.run, paint.show + 1), matrix);
    }
    if objects.next().is_some() {
        return Err(text_error("native text objects remain after source text replay").into());
    }
    let mut identical_paints = HashMap::new();
    for paints in painted_glyphs.into_values() {
        if let Some((exemplar, _)) = paints.iter().find(|(_, complete)| *complete) {
            for (object, complete) in &paints {
                if !complete {
                    identical_paints.insert(*object, *exemplar);
                }
            }
        }
    }
    Ok(SourceGlyphRecovery {
        native: frames,
        native_spaces,
        identical_paints,
        omitted,
        replacements,
    })
}

fn squared_distance(left: [f64; 2], right: [f64; 2]) -> f64 {
    (left[0] - right[0]).powi(2) + (left[1] - right[1]).powi(2)
}

fn translate_text(matrix: Matrix, offset: [f32; 2]) -> Matrix {
    matrix.compose(Matrix([1.0, 0.0, 0.0, 1.0, offset[0], offset[1]]))
}

/// Recover source line and glyph-matrix arithmetic before rebuilding bounds.
/// Unicode remains attached to its native glyph index and paint object.
pub fn restore_source_text_positions(
    page: &Page<'_, '_>,
    text_page: &TextPage<'_, '_>,
    content: &SourcePageContent,
    characters: &HashMap<usize, char>,
    tokens: &mut Vec<TextToken>,
) -> Result<(), Box<dyn Error>> {
    let mut order = ObjectOrder::default();
    let mut next = 0;
    for index in 0..page.object_count() {
        if let Some(object) = page.object(index) {
            collect_object_order(object, 0, &mut next, &mut order);
        }
    }
    if order.text.len() != content.text_line_corrections.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "source/native text-object alignment differs: {} source shows, {} native objects",
                content.text_line_corrections.len(),
                order.text.len(),
            ),
        )
        .into());
    }
    let SourceGlyphRecovery {
        native: recovered,
        native_spaces,
        identical_paints,
        omitted,
        replacements,
    } = source_glyph_frames(page, text_page, content, &order, characters)?;
    let source_names: HashMap<_, _> = order
        .text
        .iter()
        .copied()
        .zip(
            content
                .text_paints
                .iter()
                .filter(|paint| paint.has_text)
                .map(|paint| paint.font_name.as_deref()),
        )
        .collect();
    let corrections: HashMap<_, _> = order
        .text
        .into_iter()
        .zip(content.text_line_corrections.iter().copied())
        .collect();
    // Kerning can put a trailing space inside the next token's bounds. Its
    // encoded suffix owns the glyph; recovering it again as a prefix would
    // duplicate both the text and its painted extent.
    let mut suffix_spaces = HashSet::new();
    for token in tokens.iter() {
        let Some(last_visible) = token
            .glyph_frames
            .iter()
            .filter_map(|frame| frame.native_index)
            .filter(|index| !native_spaces.contains(index))
            .max()
        else {
            continue;
        };
        for index in token
            .glyph_frames
            .iter()
            .filter_map(|frame| frame.native_index)
        {
            if index > last_visible
                && native_spaces.contains(&index)
                && text_page
                    .char_at(index as i32)
                    .is_some_and(|glyph| token.char_codes.last() == Some(&glyph.char_code()))
            {
                suffix_spaces.insert(index);
            }
        }
    }
    for token in tokens.iter_mut() {
        // A space can overlap a neighbouring word after kerning. Select the
        // visible glyphs' native order before restoring the source baselines.
        let baseline_start_index = token
            .glyph_frames
            .iter()
            .filter_map(|frame| frame.native_index)
            .filter(|index| !native_spaces.contains(index))
            .min();
        let baseline_end_index = token
            .glyph_frames
            .iter()
            .filter_map(|frame| frame.native_index)
            .filter(|index| !native_spaces.contains(index))
            .max();
        let Some(delta) = token.source_order.and_then(|index| corrections.get(&index)) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "native text token has no source paint operation",
            )
            .into());
        };
        if let Some(Some(name)) = token
            .source_order
            .and_then(|index| source_names.get(&index))
        {
            token.font_name = Some((*name).to_owned());
        }
        let shift = |point: &mut [f64; 2]| {
            for axis in 0..2 {
                point[axis] = f64::from(point[axis] as f32 + delta[axis] as f32);
            }
        };
        if let Some(start) = &mut token.baseline_start {
            shift(start);
        }
        if let Some(end) = &mut token.baseline_end {
            shift(end);
        }
        for frame in &mut token.glyph_frames {
            if let Some(source) = frame.native_index.and_then(|index| recovered.get(&index)) {
                *frame = source.clone();
            } else {
                shift(&mut frame.start);
                shift(&mut frame.end);
            }
        }
        if let Some(first_visible) = baseline_start_index {
            // Reading tokens trim leading spaces. Restore only the contiguous
            // owned native prefix, not a prefix from an earlier token in the
            // same text object, so gap splitting can assign its painted bounds.
            let mut leading = Vec::new();
            let mut cursor = first_visible;
            while let Some(index) = cursor.checked_sub(1) {
                if !native_spaces.contains(&index)
                    || suffix_spaces.contains(&index)
                    || !token
                        .glyph_frames
                        .iter()
                        .any(|frame| frame.native_index == Some(index))
                {
                    break;
                }
                leading.push(index);
                cursor = index;
            }
            token.glyph_frames.retain(|frame| {
                frame.native_index.is_none_or(|index| {
                    index >= first_visible
                        || !native_spaces.contains(&index)
                        || leading.contains(&index)
                })
            });
            token.order_native_glyph_frames();
            if !leading.is_empty() {
                leading.reverse();
                let codes = leading
                    .iter()
                    .map(|index| {
                        Ok(text_page
                            .char_at(*index as i32)
                            .ok_or_else(|| text_error("owned native space is missing"))?
                            .char_code())
                    })
                    .collect::<Result<Vec<_>, std::io::Error>>()?;
                token.char_codes.splice(0..0, codes);
                token.text.insert_str(0, &" ".repeat(leading.len()));
            }
        }
        if let Some(first) = baseline_start_index.and_then(|index| recovered.get(&index)) {
            token.baseline_start = Some(first.start);
        }
        if let Some(last) = baseline_end_index.and_then(|index| recovered.get(&index)) {
            token.baseline_end = Some(last.end);
        }
        if let Some(last_visible) = baseline_end_index {
            let mut trailing: Vec<_> = token
                .glyph_frames
                .iter()
                .filter_map(|frame| {
                    let index = frame.native_index?;
                    (index > last_visible && native_spaces.contains(&index))
                        .then_some((index, frame))
                })
                .collect();
            trailing.sort_by_key(|(index, _)| *index);
            trailing.dedup_by_key(|(index, _)| *index);
            let codes: HashSet<_> = trailing
                .iter()
                .filter_map(|(index, _)| {
                    text_page
                        .char_at(*index as i32)
                        .map(|glyph| glyph.char_code())
                })
                .collect();
            let count = token
                .char_codes
                .iter()
                .rev()
                .take_while(|code| codes.contains(code))
                .count()
                .min(trailing.len());
            if count > 0 {
                token.text.push_str(&" ".repeat(count));
                token.baseline_end = Some(trailing[count - 1].1.end);
            }
        }
        token.x = f64::from(token.x as f32 + delta[0] as f32);
        token.y = f64::from(token.y as f32 + delta[1] as f32);
    }
    tokens.extend(omitted);
    tokens.retain(|token| {
        token
            .source_order
            .is_none_or(|order| !replacements.contains_key(&order))
    });
    tokens.extend(replacements.into_values());
    let mut by_order: HashMap<usize, Vec<usize>> = HashMap::new();
    for (index, token) in tokens.iter().enumerate() {
        if let Some(order) = token.source_order {
            by_order.entry(order).or_default().push(index);
        }
    }
    let mut repeated = Vec::new();
    for (target, exemplar) in identical_paints {
        if by_order.contains_key(&target) {
            continue;
        }
        if let Some(indices) = by_order.get(&exemplar) {
            for index in indices {
                let mut token = tokens[*index].clone();
                token.source_order = Some(target);
                for frame in &mut token.glyph_frames {
                    frame.native_index = None;
                }
                repeated.push(token);
            }
        }
    }
    tokens.extend(repeated);
    Ok(())
}

/// Restore embedded font names and source font metrics for geometry spans.
pub fn restore_source_font_metadata(
    path: &Path,
    tokens: &mut [TextToken],
) -> Result<(), Box<dyn Error>> {
    let source = SourceDocument::load(path)?;
    restore_loaded_source_font_metadata(&source, tokens);
    Ok(())
}

/// Restore font metadata without reparsing the PDF for every selected page.
pub fn restore_loaded_source_font_metadata(source: &SourceDocument, tokens: &mut [TextToken]) {
    let mut names: HashMap<String, Option<String>> = HashMap::new();
    let mut declared_names = HashSet::new();
    let mut metrics: HashMap<String, Option<[f64; 2]>> = HashMap::new();
    for object in source.objects.values() {
        let dictionary = match object {
            Object::Dictionary(dictionary) => dictionary,
            Object::Stream(stream) => &stream.dict,
            _ => continue,
        };
        let Ok(Object::Name(base)) = dictionary.get(b"BaseFont") else {
            continue;
        };
        let full = String::from_utf8_lossy(base).into_owned();
        declared_names.insert(full.clone());
        if let Some(values) = source_font_metrics(source, dictionary) {
            metrics
                .entry(full.clone())
                .and_modify(|found| {
                    if *found != Some(values) {
                        *found = None;
                    }
                })
                .or_insert(Some(values));
        }
        let Some((_, short)) = full.split_once('+') else {
            continue;
        };
        names
            .entry(short.to_owned())
            .and_modify(|found| {
                if found.as_ref() != Some(&full) {
                    *found = None;
                }
            })
            .or_insert(Some(full));
    }
    for token in tokens {
        if let Some(Some(full)) = token
            .font_name
            .as_ref()
            .filter(|name| !declared_names.contains(*name))
            .and_then(|name| names.get(name))
        {
            token.font_name = Some(full.clone());
        }
        let values = if token.vertical_writing {
            Some(VERTICAL_TEXT_CROSS_AXIS_BOUNDS)
        } else {
            token
                .font_name
                .as_ref()
                .and_then(|name| metrics.get(name))
                .copied()
                .flatten()
        };
        token.font_metrics = values;
        if let Some(values) = values
            && !token.glyph_frames.is_empty()
        {
            let bounds = glyph_frame_bounds(&token.glyph_frames, values);
            token.x = bounds[0];
            token.y = bounds[1];
            token.width = bounds[2] - bounds[0];
            token.height = bounds[3] - bounds[1];
        }
    }
}

fn glyph_frame_bounds(frames: &[GlyphFrame], metrics: [f64; 2]) -> [f64; 4] {
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for frame in frames {
        for baseline in [frame.start, frame.end] {
            for metric in metrics {
                let x = f64::from(baseline[0] as f32 + frame.vertical[0] as f32 * metric as f32);
                let y = f64::from(baseline[1] as f32 + frame.vertical[1] as f32 * metric as f32);
                bounds[0] = bounds[0].min(x);
                bounds[1] = bounds[1].min(y);
                bounds[2] = bounds[2].max(x);
                bounds[3] = bounds[3].max(y);
            }
        }
    }
    bounds
}

/// Keep text paint order while joining adjacent words on one baseline.
pub fn group_text_tokens(tokens: &[TextToken]) -> Vec<TextSpan> {
    let mut spans = Vec::new();
    let mut current: Vec<&TextToken> = Vec::new();
    let pieces: Vec<_> = tokens.iter().flat_map(split_glyph_gaps).collect();
    let mut ordered: Vec<_> = pieces.iter().map(Cow::as_ref).enumerate().collect();
    ordered.sort_by_key(|(index, token)| (token.source_order.unwrap_or(usize::MAX), *index));
    ordered.dedup_by(|(_, next), (_, previous)| {
        next.source_order.is_some()
            && (next.source_order == previous.source_order
                || (next.text.chars().count() == 1
                    && next.char_codes.len() == 1
                    && next.char_codes == previous.char_codes
                    && next.font_name == previous.font_name
                    && next.glyph_frames.len() == 1
                    && previous.glyph_frames.len() == 1
                    && next.vertical_writing == previous.vertical_writing
                    && next.normalized_rotation() == previous.normalized_rotation()))
            && next.text == previous.text
            && next.baseline_start == previous.baseline_start
            && next.baseline_end == previous.baseline_end
    });
    for (_, token) in ordered {
        if token.text.is_empty() {
            continue;
        }
        if current
            .last()
            .is_some_and(|previous| !can_join(current[0], previous, token))
        {
            let order = current
                .iter()
                .filter_map(|item| item.source_order)
                .min()
                .unwrap_or(usize::MAX);
            spans.push((order, spans.len(), make_span(&current)));
            current.clear();
        }
        current.push(token);
    }
    if !current.is_empty() {
        let order = current
            .iter()
            .filter_map(|item| item.source_order)
            .min()
            .unwrap_or(usize::MAX);
        spans.push((order, spans.len(), make_span(&current)));
    }
    spans.sort_by_key(|(order, sequence, _)| (*order, *sequence));
    spans
        .into_iter()
        .map(|(_, _, span)| span)
        .filter(|span| !span.text.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(text: &str, x: f64, y: f64) -> TextToken {
        TextToken {
            char_codes: Vec::new(),
            text: text.into(),
            x,
            y,
            width: text.len() as f64 * 5.0,
            height: 10.0,
            rotation: 0.0,
            font_name: Some("Example".into()),
            font_height: Some(10.0),
            visual_font_size: None,
            source_order: None,
            baseline_start: None,
            baseline_end: None,
            glyph_vertical_size: None,
            glyph_frames: Vec::new(),
            vertical_writing: false,
            font_metrics: None,
        }
    }

    #[test]
    fn joins_words_but_keeps_distant_columns_separate() {
        let spans = group_text_tokens(&[
            token("SHEET", 0.0, 0.0),
            token("TITLE", 27.0, 0.0),
            token("REVISION", 120.0, 0.0),
        ]);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "SHEET TITLE");
        assert_eq!(spans[1].text, "REVISION");
    }

    #[test]
    fn follows_baselines_and_preserves_first_line_direction() {
        const FIRST_ANGLE: f64 = 30.0;
        const ANGLE_STEP: f64 = 2.0;
        const ADVANCE: f64 = 5.0;
        const GAP: f64 = 2.0;
        let mut tokens = Vec::new();
        let mut end = [0.0, 0.0];
        for index in 0..3 {
            let angle = FIRST_ANGLE + index as f64 * ANGLE_STEP;
            let direction = [angle.to_radians().cos(), angle.to_radians().sin()];
            let start = [end[0] + GAP * direction[0], end[1] + GAP * direction[1]];
            end = [
                start[0] + ADVANCE * direction[0],
                start[1] + ADVANCE * direction[1],
            ];
            let mut item = token(">", start[0], start[1]);
            item.rotation = angle;
            item.source_order = Some(index);
            item.baseline_start = Some(start);
            item.baseline_end = Some(end);
            tokens.push(item);
        }
        let spans = group_text_tokens(&tokens);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "> >");
        assert_eq!(spans[0].rotation, FIRST_ANGLE);
        assert_eq!(spans[1].text, ">");
    }

    #[test]
    fn contiguous_rotated_digits_follow_paint_order_without_added_spaces() {
        const ROTATION: f64 = 90.0;
        const ADVANCE: f64 = 5.0;
        let mut tokens: Vec<_> = "11.0"
            .chars()
            .enumerate()
            .map(|(index, character)| {
                let start = index as f64 * ADVANCE;
                let mut item = token(&character.to_string(), 0.0, start);
                item.rotation = ROTATION;
                item.source_order = Some(index);
                item.baseline_start = Some([0.0, start]);
                item.baseline_end = Some([0.0, start + ADVANCE]);
                item
            })
            .collect();
        tokens.reverse();
        let spans = group_text_tokens(&tokens);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "11.0");
        assert_eq!(spans[0].rotation, ROTATION);
    }
}
