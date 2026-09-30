//! Paint-state recovery from PDF content streams. PDFium supplies the page
//! transform; the serialized stream supplies raw operands PDFium converts away.

use std::collections::BTreeSet;
use std::error::Error;
use std::io;
use std::path::Path;
use std::sync::Arc;

use lopdf::{Document as SourceDocument, Object, ObjectId, content::Content};
use pdfium::{Library, Page, PathSegment, SegmentKind, ViewportTransform};

use crate::classify::image_coverage;
use crate::model::{PaintStyle, Viewport};
use crate::path_capture::PathCapture;
use crate::viewports::read_page_viewports;

const MAX_FORM_DEPTH: usize = 16;
const MAX_PATTERN_DEPTH: usize = 16;
// ISO 32000-1 §8.7.3: a tiling pattern is a stream whose content paints the tile.
const PATTERN_TYPE_TILING: i64 = 1;
// ISO 32000-1 §8.7.4.2: a shading pattern is a plain dictionary with no content.
const PATTERN_TYPE_SHADING: i64 = 2;
const MAX_CMAP_DEPTH: usize = 16;
const MAX_COLOR_SPACE_DEPTH: usize = 32;
const MAX_GRAPHICS_STATE_DEPTH: usize = 64;
const MAX_MARKED_CONTENT_DEPTH: usize = 64;
const PDF_TEXT_SCALE_PERCENT: f32 = 100.0;
type ResourceValue<'a> = Option<(Option<ObjectId>, &'a Object)>;

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn cmap_code_space(low: &[u8], high: &[u8]) -> Result<CodeSpace, io::Error> {
    if low.is_empty() || low.len() > std::mem::size_of::<u32>() || low.len() != high.len() {
        return Err(invalid("CMap codespace range has invalid byte lengths"));
    }
    let code = |bytes: &[u8]| {
        bytes
            .iter()
            .fold(0_u32, |value, byte| (value << u8::BITS) | u32::from(*byte))
    };
    let (low_code, high_code) = (code(low), code(high));
    if low_code > high_code {
        return Err(invalid("CMap codespace range has descending bounds"));
    }
    Ok(CodeSpace {
        length: low.len(),
        low: low_code,
        high: high_code,
    })
}

fn unicode_space_codes(
    source: &SourceDocument,
    cmap: &Object,
    depth: usize,
) -> Result<BTreeSet<(u32, usize)>, Box<dyn Error>> {
    if depth >= MAX_CMAP_DEPTH {
        return Err(invalid("ToUnicode inheritance exceeds the depth limit").into());
    }
    let (_, cmap) = source.dereference(cmap)?;
    let stream = match cmap {
        Object::Name(name) => {
            return Ok(if name == b"Identity-H" || name == b"Identity-V" {
                BTreeSet::from([(u32::from(b' '), 2)])
            } else {
                // Other predefined maps provide no locally proven space code.
                BTreeSet::new()
            });
        }
        Object::Null => return Ok(BTreeSet::new()),
        _ => cmap.as_stream()?,
    };
    let bytes = stream.get_plain_content()?;
    let content = Content::decode(&bytes)?;
    let named_base = content
        .operations
        .iter()
        .find(|op| op.operator == "usecmap")
        .and_then(|op| op.operands.first());
    let mut spaces = if let Some(base) = stream.dict.get(b"UseCMap").ok().or(named_base) {
        unicode_space_codes(source, base, depth + 1)?
    } else {
        BTreeSet::new()
    };
    for operation in content.operations {
        match operation.operator.as_str() {
            "endbfchar" => {
                if operation.operands.len() % 2 != 0 {
                    return Err(invalid("ToUnicode bfchar has an unmatched mapping").into());
                }
                for pair in operation.operands.as_chunks::<2>().0 {
                    let bytes = pair[0].as_str()?;
                    let source = cmap_code_space(bytes, bytes)?;
                    let key = (source.low, source.length);
                    spaces.remove(&key);
                    if pair[1].as_str()? == [0, b' '] {
                        spaces.insert(key);
                    }
                }
            }
            "endbfrange" => {
                if operation.operands.len() % 3 != 0 {
                    return Err(invalid("ToUnicode bfrange has an incomplete mapping").into());
                }
                for mapping in operation.operands.as_chunks::<3>().0 {
                    let source = cmap_code_space(mapping[0].as_str()?, mapping[1].as_str()?)?;
                    spaces.retain(|(code, length)| {
                        *length != source.length || !(source.low..=source.high).contains(code)
                    });
                    match &mapping[2] {
                        Object::String(target, _) => {
                            if let Ok(target) = <[u8; 2]>::try_from(target.as_slice()) {
                                let first = u16::from_be_bytes(target);
                                if let Some(offset) = u16::from(b' ').checked_sub(first)
                                    && let Some(code) = source.low.checked_add(u32::from(offset))
                                    && code <= source.high
                                {
                                    spaces.insert((code, source.length));
                                }
                            }
                        }
                        Object::Array(targets) => {
                            let count = u64::from(source.high) - u64::from(source.low) + 1;
                            if targets.len() as u64 != count {
                                return Err(invalid(
                                    "ToUnicode bfrange array length differs from its source range",
                                )
                                .into());
                            }
                            for (index, target) in targets.iter().enumerate() {
                                if target.as_str()? == [0, b' '] {
                                    let offset = u32::try_from(index)?;
                                    spaces.insert((source.low + offset, source.length));
                                }
                            }
                        }
                        _ => {
                            return Err(invalid(
                                "ToUnicode bfrange target is neither a string nor an array",
                            )
                            .into());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(spaces)
}

fn number(value: &Object) -> Result<f64, io::Error> {
    match value {
        Object::Integer(value) => Ok(*value as f64),
        Object::Real(value) => Ok(f64::from(*value)),
        _ => Err(invalid("expected a PDF number")),
    }
}

fn numbers(values: &[Object]) -> Result<Vec<f64>, io::Error> {
    values.iter().map(number).collect()
}

fn name(value: &Object) -> Result<String, io::Error> {
    match value {
        Object::Name(bytes) => Ok(String::from_utf8_lossy(bytes).into_owned()),
        _ => Err(invalid("expected a PDF name")),
    }
}

#[derive(Clone, Copy, Debug)]
// Compose the viewport before source operators; rounding an intermediate page
// point changes ties in downstream relation selection.
pub(crate) struct Matrix(pub(crate) [f32; 6]);

impl Matrix {
    const IDENTITY: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    pub(crate) fn compose(self, inner: Self) -> Self {
        let [a, b, c, d, e, f] = self.0;
        let [g, h, i, j, k, l] = inner.0;
        Self([
            a * g + c * h,
            b * g + d * h,
            a * i + c * j,
            b * i + d * j,
            a * k + c * l + e,
            b * k + d * l + f,
        ])
    }

    pub(crate) fn apply(self, point: [f64; 2]) -> [f64; 2] {
        let [a, b, c, d, e, f] = self.0;
        let [x, y] = point.map(|value| value as f32);
        [f64::from(a * x + c * y + e), f64::from(b * x + d * y + f)]
    }

    fn from_viewport(viewport: ViewportTransform) -> Self {
        let (a, b) = viewport.transform_vector(1.0, 0.0);
        let (c, d) = viewport.transform_vector(0.0, 1.0);
        let (e, f) = viewport.transform_point(0.0, 0.0);
        Self([a, b, c, d, e, f])
    }

    fn from_objects(values: &[Object]) -> Result<Self, io::Error> {
        let values = numbers(values)?;
        let components: [f64; 6] = values
            .try_into()
            .map_err(|_| invalid("expected six matrix numbers"))?;
        if !components.iter().all(|value| value.is_finite()) {
            return Err(invalid("nonfinite PDF matrix"));
        }
        Ok(Self(components.map(|value| value as f32)))
    }
}

#[derive(Clone, Debug)]
struct GraphicsState {
    ctm: Matrix,
    text_line: Matrix,
    native_text_matrix: Matrix,
    native_line_offset: [f32; 2],
    text_leading: f32,
    text_rise: f32,
    text_font_size: f32,
    text_font_name: Option<Arc<str>>,
    text_space_codes: Arc<[(u32, usize)]>,
    text_scale: f32,
    text_char_space: f32,
    text_word_space: f32,
    text_composite_font: bool,
    text_vertical: bool,
    text_code_spaces: Arc<[CodeSpace]>,
    text_run: usize,
    text_show: usize,
    stroke_width: f64,
    stroke_space: String,
    fill_space: String,
    stroke_color: Vec<f64>,
    fill_color: Vec<f64>,
    stroke_alpha: f64,
    fill_alpha: f64,
    dash_array: Vec<f64>,
    dash_phase: f64,
    clip_depth: usize,
    stroke_pattern: String,
    fill_pattern: String,
    form_name: String,
    form_ref: String,
    form_ctm: Vec<f64>,
    tile_pattern: String,
    tile_id: i32,
    tile_ctm: Vec<f64>,
}

impl Default for GraphicsState {
    fn default() -> Self {
        Self {
            ctm: Matrix::IDENTITY,
            text_line: Matrix::IDENTITY,
            native_text_matrix: Matrix::IDENTITY,
            native_line_offset: [0.0; 2],
            text_leading: 0.0,
            text_rise: 0.0,
            text_font_size: 0.0,
            text_font_name: None,
            text_space_codes: Arc::from([]),
            text_scale: 1.0,
            text_char_space: 0.0,
            text_word_space: 0.0,
            text_composite_font: false,
            text_vertical: false,
            text_code_spaces: Arc::from([]),
            text_run: 0,
            text_show: 0,
            stroke_width: 1.0,
            stroke_space: "DeviceGray".into(),
            fill_space: "DeviceGray".into(),
            stroke_color: vec![0.0],
            fill_color: vec![0.0],
            stroke_alpha: 1.0,
            fill_alpha: 1.0,
            dash_array: Vec::new(),
            dash_phase: 0.0,
            clip_depth: 0,
            stroke_pattern: String::new(),
            fill_pattern: String::new(),
            form_name: String::new(),
            form_ref: String::new(),
            form_ctm: Vec::new(),
            tile_pattern: String::new(),
            tile_id: 0,
            tile_ctm: Vec::new(),
        }
    }
}

impl GraphicsState {
    fn move_text_line(&mut self, offset: [f32; 2]) {
        self.text_line = self
            .text_line
            .compose(Matrix([1.0, 0.0, 0.0, 1.0, offset[0], offset[1]]));
        self.native_line_offset[0] += offset[0];
        self.native_line_offset[1] += offset[1];
    }

    fn text_line_correction(&self) -> [f64; 2] {
        let source = self
            .ctm
            .apply(self.text_line.apply([0.0, f64::from(self.text_rise)]));
        let native = self.ctm.apply(self.native_text_matrix.apply([
            f64::from(self.native_line_offset[0]),
            f64::from(self.native_line_offset[1] + self.text_rise),
        ]));
        [source[0] - native[0], source[1] - native[1]]
    }

    fn paint_style(&self, fill: bool, layer: &str) -> PaintStyle {
        let color_space = if fill {
            &self.fill_space
        } else {
            &self.stroke_space
        };
        let mut style = PaintStyle {
            stroke_width: if fill { 0.0 } else { self.stroke_width },
            paint: if fill { "fill" } else { "stroke" }.into(),
            color_space: if color_space == "Pattern" {
                String::new()
            } else {
                color_space.clone()
            },
            alpha: if fill {
                self.fill_alpha
            } else {
                self.stroke_alpha
            },
            form_x_object_id: self.form_name.clone(),
            form_x_object_ref: self.form_ref.clone(),
            form_ctm: self.form_ctm.clone(),
            tile_id: self.tile_id,
            tile_ctm: self.tile_ctm.clone(),
            layer: layer.into(),
            ..PaintStyle::default()
        };
        style.fill_pattern_id = if self.tile_pattern.is_empty() {
            self.fill_pattern.clone()
        } else {
            self.tile_pattern.clone()
        };
        if fill {
            style.fill_color = self.fill_color.clone();
        } else {
            style.stroke_color = self.stroke_color.clone();
            style.stroke_pattern_id = self.stroke_pattern.clone();
            style.dash_array = self.dash_array.clone();
            style.dash_phase = self.dash_phase;
        }
        style
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CurveCommand {
    Cubic,
    FirstControlAtCurrentPoint,
    LastControlAtEndpoint,
}

#[derive(Default)]
struct CurrentPath {
    commands: Vec<PathSegment>,
    first: Option<[f64; 2]>,
    current: Option<[f64; 2]>,
    bounds: Option<[f64; 4]>,
    pending_clip: bool,
    rectangle_endpoint_pending: bool,
}

impl CurrentPath {
    fn point(&mut self, point: [f64; 2], state: &GraphicsState) -> Result<[f32; 2], io::Error> {
        let [x, y] = state.ctm.apply(point).map(|value| value as f32);
        if !x.is_finite() || !y.is_finite() {
            return Err(invalid("nonfinite transformed path coordinate"));
        }
        let point = [f64::from(x), f64::from(y)];
        self.bounds = Some(if let Some(bounds) = self.bounds {
            [
                bounds[0].min(point[0]),
                bounds[1].min(point[1]),
                bounds[2].max(point[0]),
                bounds[3].max(point[1]),
            ]
        } else {
            [point[0], point[1], point[0], point[1]]
        });
        Ok([x, y])
    }

    fn command(
        &mut self,
        kind: SegmentKind,
        point: [f64; 2],
        close: bool,
        state: &GraphicsState,
    ) -> Result<(), io::Error> {
        self.point(point, state)?;
        self.rectangle_endpoint_pending = false;
        self.commands.push(PathSegment {
            kind,
            x: point[0] as f32,
            y: point[1] as f32,
            close,
        });
        Ok(())
    }

    fn move_to(&mut self, point: [f64; 2], state: &GraphicsState) -> Result<(), io::Error> {
        self.first = Some(point);
        self.current = Some(point);
        self.command(SegmentKind::MoveTo, point, false, state)
    }

    fn line_to(&mut self, point: [f64; 2], state: &GraphicsState) -> Result<(), io::Error> {
        if self.current.is_none() {
            return Err(invalid("lineto without moveto"));
        }
        // The capture contract retains the initial zero-length line (a round
        // cap can paint it), but omits repeated endpoints after other commands.
        if self.current.is_some_and(|current| {
            current.map(|value| value as f32) == point.map(|value| value as f32)
        }) && !self.rectangle_endpoint_pending
            && self
                .commands
                .last()
                .is_some_and(|command| command.kind != SegmentKind::MoveTo)
        {
            return Ok(());
        }
        self.current = Some(point);
        self.command(SegmentKind::LineTo, point, false, state)
    }

    fn curve_to(
        &mut self,
        mut points: [[f64; 2]; 3],
        command: CurveCommand,
        state: &GraphicsState,
    ) -> Result<(), io::Error> {
        let current = self
            .current
            .ok_or_else(|| invalid("curveto without moveto"))?
            .map(|value| value as f32);
        let [first, second, end] = points.map(|point| point.map(|value| value as f32));
        // Coincident control points use the source path's line representation.
        // Compare before the paint transform, just as for explicit endpoints.
        let line = match command {
            CurveCommand::Cubic => {
                (first == current && (second == current || second == end))
                    || (first == second && second == end)
            }
            CurveCommand::FirstControlAtCurrentPoint => second == current || second == end,
            CurveCommand::LastControlAtEndpoint => first == end,
        };
        if line {
            self.line_to(points[2], state)?;
            if command == CurveCommand::Cubic {
                return Ok(());
            }
            // Shorthand commands retain the cubic after the line. Its implicit
            // first control uses the current point following that line.
            if command == CurveCommand::FirstControlAtCurrentPoint {
                points[0] = points[2];
            }
        }
        for point in points {
            self.command(SegmentKind::BezierTo, point, false, state)?;
        }
        self.current = Some(points[2]);
        Ok(())
    }

    fn close(&mut self, state: &GraphicsState) -> Result<(), io::Error> {
        if self.commands.last().is_some_and(|command| command.close) {
            return Ok(());
        }
        let first = self
            .first
            .ok_or_else(|| invalid("closepath without moveto"))?;
        self.command(SegmentKind::LineTo, first, true, state)?;
        self.current = Some(first);
        Ok(())
    }
}

struct StreamReader<'a> {
    source: &'a SourceDocument,
    capture: PathCapture,
    capture_clips: Vec<[f64; 4]>,
    layers: Vec<String>,
    marked_content: Vec<Arc<[u8]>>,
    active_forms: Vec<ObjectId>,
    active_patterns: Vec<ObjectId>,
    page_bounds: [f64; 4],
    image_ops: usize,
    image_area: f64,
    text_line_corrections: Vec<[f64; 2]>,
    text_paints: Vec<SourceTextPaint>,
    next_text_run: usize,
}

#[derive(Clone, Debug)]
pub(crate) enum SourceTextElement {
    Bytes(Vec<u8>),
    Adjustment(f32),
}

#[derive(Clone, Debug)]
pub(crate) struct CodeSpace {
    pub length: usize,
    pub low: u32,
    pub high: u32,
}

/// Source text-state checkpoints retain cursor identity across graphics saves.
#[derive(Clone, Debug)]
pub struct SourceTextPaint {
    pub(crate) line: Matrix,
    pub(crate) ctm: Matrix,
    pub(crate) font_size: f32,
    pub(crate) font_name: Option<Arc<str>>,
    pub(crate) space_codes: Arc<[(u32, usize)]>,
    pub(crate) scale: f32,
    pub(crate) rise: f32,
    pub(crate) char_space: f32,
    pub(crate) word_space: f32,
    pub(crate) composite_font: bool,
    pub(crate) vertical: bool,
    pub(crate) code_spaces: Arc<[CodeSpace]>,
    pub(crate) run: usize,
    pub(crate) show: usize,
    pub(crate) elements: Vec<SourceTextElement>,
    pub(crate) has_text: bool,
    pub(crate) marked_content: Arc<[u8]>,
}

pub struct SourcePageContent {
    pub paths: PathCapture,
    pub width_pts: f64,
    pub height_pts: f64,
    pub image_ops: usize,
    pub image_coverage: f64,
    /// Line-origin precision corrections in native text-object paint order.
    pub text_line_corrections: Vec<[f64; 2]>,
    pub text_paints: Vec<SourceTextPaint>,
    pub viewports: Vec<Viewport>,
    pub viewports_error: Option<String>,
}

impl<'a> StreamReader<'a> {
    fn push_clip(&mut self, state: &mut GraphicsState, bounds: [f64; 4]) {
        state.clip_depth += 1;
        let bounds = self.capture_clips.last().map_or(bounds, |current| {
            [
                current[0].max(bounds[0]),
                current[1].max(bounds[1]),
                current[2].min(bounds[2]),
                current[3].min(bounds[3]),
            ]
        });
        self.capture_clips.push(bounds);
    }

    fn paint_style(&self, state: &GraphicsState, fill: bool, layer: &str) -> PaintStyle {
        let mut style = state.paint_style(fill, layer);
        style.clip_depth = self.capture_clips.len();
        style.clip_bbox = self
            .capture_clips
            .last()
            .map_or_else(Vec::new, |bbox| bbox.to_vec());
        style
    }

    fn color_space_name(
        &self,
        resources: &[&lopdf::Dictionary],
        object: &Object,
        depth: usize,
    ) -> Result<String, Box<dyn Error>> {
        if depth >= MAX_COLOR_SPACE_DEPTH {
            return Err(invalid("colorspace nesting limit exceeded").into());
        }
        let (_, object) = self.source.dereference(object)?;
        if let Object::Name(value) = object {
            let key = String::from_utf8_lossy(value);
            match key.as_ref() {
                "DeviceGray" | "DeviceRGB" | "DeviceCMYK" | "Pattern" => {
                    return Ok(key.into_owned());
                }
                "G" => return Ok("DeviceGray".into()),
                "RGB" => return Ok("DeviceRGB".into()),
                "CMYK" => return Ok("DeviceCMYK".into()),
                _ => {}
            }
            let (_, alias) = self
                .resource(resources, b"ColorSpace", value)?
                .ok_or_else(|| invalid(format!("missing colorspace {key}")))?;
            return self.color_space_name(resources, alias, depth + 1);
        }
        let values = object.as_array()?;
        let kind = name(operand(values, 0)?)?;
        match kind.as_str() {
            "DeviceGray" | "DeviceRGB" | "DeviceCMYK" | "CalGray" | "CalRGB" | "Lab"
            | "Pattern" => Ok(kind),
            "ICCBased" => {
                let (_, profile) = self.source.dereference(operand(values, 1)?)?;
                crate::color_space::profile_name(profile.as_stream()?)
            }
            "Separation" => {
                let colorant = name(operand(values, 1)?)?;
                let base = self.color_space_name(resources, operand(values, 2)?, depth + 1)?;
                Ok(format!("Separation({base},{colorant})"))
            }
            "DeviceN" => {
                let colorants = operand(values, 1)?.as_array()?;
                if colorants.is_empty() {
                    return Err(invalid("DeviceN has no colorants").into());
                }
                let base = self.color_space_name(resources, operand(values, 2)?, depth + 1)?;
                let names = colorants.iter().map(name).collect::<Result<Vec<_>, _>>()?;
                Ok(format!(
                    "DeviceN({},{base},{})",
                    names.len(),
                    names.join(",")
                ))
            }
            "Indexed" | "I" => {
                let base = self.color_space_name(resources, operand(values, 1)?, depth + 1)?;
                let highest = operand(values, 2)?.as_i64()?;
                Ok(format!("Indexed({highest},{base})"))
            }
            _ => Err(invalid(format!("invalid colorspace family {kind}")).into()),
        }
    }

    fn reset_text_cursor(&mut self, state: &mut GraphicsState) {
        state.text_run = self.next_text_run;
        self.next_text_run += 1;
        state.text_show = 0;
    }

    fn set_text_font(
        &self,
        resources: &[&lopdf::Dictionary],
        state: &mut GraphicsState,
        key: &str,
        size: f32,
    ) -> Result<(), Box<dyn Error>> {
        let (_, font) = self
            .resource(resources, b"Font", key.as_bytes())?
            .ok_or_else(|| invalid(format!("missing text font {key}")))?;
        self.apply_text_font(state, font.as_dict()?)?;
        state.text_font_size = size;
        Ok(())
    }

    fn apply_text_font(
        &self,
        state: &mut GraphicsState,
        font: &lopdf::Dictionary,
    ) -> Result<(), Box<dyn Error>> {
        state.text_font_name = font
            .get(b"BaseFont")
            .ok()
            .and_then(|value| value.as_name().ok())
            .map(|name| Arc::from(String::from_utf8_lossy(name).as_ref()));
        state.text_composite_font = font.get(b"Subtype")?.as_name()? == b"Type0";
        state.text_space_codes = if let Ok(cmap) = font.get(b"ToUnicode") {
            unicode_space_codes(self.source, cmap, 0)?
                .into_iter()
                .collect::<Vec<_>>()
                .into()
        } else if !state.text_composite_font
            && font.get(b"ToUnicode").is_err()
            && font.get(b"Encoding").is_ok_and(|value| {
                value.as_name().is_ok_and(|name| {
                    matches!(
                        name,
                        b"WinAnsiEncoding" | b"MacRomanEncoding" | b"StandardEncoding"
                    )
                })
            })
        {
            Arc::from([(u32::from(b' '), 1)])
        } else {
            Arc::from([])
        };
        state.text_vertical = false;
        state.text_code_spaces = Arc::from([]);
        if !state.text_composite_font {
            return Ok(());
        }
        self.apply_text_encoding(state, font.get(b"Encoding")?, 0)
    }

    fn apply_text_encoding(
        &self,
        state: &mut GraphicsState,
        encoding: &Object,
        depth: usize,
    ) -> Result<(), Box<dyn Error>> {
        if depth >= MAX_CMAP_DEPTH {
            return Err(invalid("CMap inheritance exceeds the depth limit").into());
        }
        let (_, encoding) = self.source.dereference(encoding)?;
        state.text_vertical = false;
        state.text_code_spaces = Arc::from([]);
        match encoding {
            Object::Name(name) => {
                state.text_vertical = name.ends_with(b"-V");
                if name == b"Identity-H" || name == b"Identity-V" {
                    state.text_code_spaces = Arc::from([CodeSpace {
                        length: 2,
                        low: 0,
                        high: u32::from(u16::MAX),
                    }]);
                }
            }
            Object::Stream(stream) => {
                let bytes = if stream.dict.get(b"Filter").is_ok() {
                    stream.decompressed_content()?
                } else {
                    stream.content.clone()
                };
                let mut ranges = Vec::new();
                let mut named_base = None;
                for operation in Content::decode(&bytes)?.operations {
                    if operation.operator == "usecmap" {
                        named_base = Some(Object::Name(
                            operation
                                .operands
                                .first()
                                .ok_or_else(|| invalid("usecmap has no base name"))?
                                .as_name()?
                                .to_vec(),
                        ));
                    }
                    if operation.operator == "def"
                        && operation
                            .operands
                            .first()
                            .is_some_and(|value| value.as_name().is_ok_and(|name| name == b"WMode"))
                    {
                        state.text_vertical = operand_number(&operation.operands, 1)? == 1.0;
                    }
                    if operation.operator == "endcodespacerange" {
                        if operation.operands.len() % 2 != 0 {
                            return Err(
                                invalid("CMap codespace range has an unmatched bound").into()
                            );
                        }
                        for bounds in operation.operands.as_chunks::<2>().0 {
                            ranges.push(cmap_code_space(bounds[0].as_str()?, bounds[1].as_str()?)?);
                        }
                    }
                }
                if let Ok(mode) = stream.dict.get(b"WMode") {
                    state.text_vertical = number(mode)? == 1.0;
                }
                if ranges.is_empty() {
                    let base = stream.dict.get(b"UseCMap").ok().or(named_base.as_ref());
                    if let Some(base) = base {
                        let mut inherited = state.clone();
                        self.apply_text_encoding(&mut inherited, base, depth + 1)?;
                        state.text_code_spaces = inherited.text_code_spaces;
                    }
                } else {
                    state.text_code_spaces = ranges.into();
                }
            }
            _ => {
                return Err(
                    invalid("Type0 font encoding is neither a name nor a CMap stream").into(),
                );
            }
        }
        Ok(())
    }

    fn layer(&self) -> &str {
        self.layers
            .iter()
            .rev()
            .find(|value| !value.is_empty())
            .map_or("", String::as_str)
    }

    fn process(
        &mut self,
        bytes: &[u8],
        resources: &[&lopdf::Dictionary],
        mut state: GraphicsState,
    ) -> Result<(), Box<dyn Error>> {
        let default_ctm = state.ctm;
        let initial_clip_depth = state.clip_depth;
        let operations = crate::inline_images::decode(bytes)?;
        let mut saved = Vec::new();
        let mut path = CurrentPath::default();
        for operation in operations {
            let operands = &operation.operands;
            match operation.operator.as_str() {
                "q" => {
                    if saved.len() >= MAX_GRAPHICS_STATE_DEPTH {
                        return Err(invalid("graphics state nesting limit exceeded").into());
                    }
                    saved.push(state.clone());
                }
                "Q" => {
                    let restored = saved
                        .pop()
                        .ok_or_else(|| invalid("unbalanced graphics state restore"))?;
                    for _ in restored.clip_depth..state.clip_depth {
                        self.capture_clips.pop();
                    }
                    state = restored;
                }
                "cm" => state.ctm = state.ctm.compose(Matrix::from_objects(operands)?),
                "BT" => {
                    state.text_line = Matrix::IDENTITY;
                    state.native_text_matrix = Matrix::IDENTITY;
                    state.native_line_offset = [0.0; 2];
                    self.reset_text_cursor(&mut state);
                }
                "Tm" => {
                    state.text_line = Matrix::from_objects(operands)?;
                    state.native_text_matrix = state.text_line;
                    state.native_line_offset = [0.0; 2];
                    self.reset_text_cursor(&mut state);
                }
                "Td" | "TD" => {
                    let offset = pair(operands, 0)?.map(|value| value as f32);
                    state.move_text_line(offset);
                    self.reset_text_cursor(&mut state);
                    if operation.operator == "TD" {
                        state.text_leading = -offset[1];
                    }
                }
                "TL" => state.text_leading = operand_number(operands, 0)? as f32,
                "Ts" => state.text_rise = operand_number(operands, 0)? as f32,
                "Tf" => self.set_text_font(
                    resources,
                    &mut state,
                    &name(operand(operands, 0)?)?,
                    operand_number(operands, 1)? as f32,
                )?,
                "Tz" => {
                    state.text_scale = operand_number(operands, 0)? as f32 / PDF_TEXT_SCALE_PERCENT
                }
                "Tc" => state.text_char_space = operand_number(operands, 0)? as f32,
                "Tw" => state.text_word_space = operand_number(operands, 0)? as f32,
                "T*" => {
                    state.move_text_line([0.0, -state.text_leading]);
                    self.reset_text_cursor(&mut state);
                }
                "Tj" | "TJ" | "'" | "\"" => {
                    if operation.operator == "'" || operation.operator == "\"" {
                        state.move_text_line([0.0, -state.text_leading]);
                        self.reset_text_cursor(&mut state);
                    }
                    if operation.operator == "\"" {
                        state.text_word_space = operand_number(operands, 0)? as f32;
                        state.text_char_space = operand_number(operands, 1)? as f32;
                    }
                    let has_text = match operation.operator.as_str() {
                        "TJ" => operand(operands, 0)?
                            .as_array()?
                            .iter()
                            .any(|value| value.as_str().is_ok_and(|bytes| !bytes.is_empty())),
                        "\"" => !operand(operands, 2)?.as_str()?.is_empty(),
                        _ => !operand(operands, 0)?.as_str()?.is_empty(),
                    };
                    if has_text && state.tile_pattern.is_empty() {
                        self.text_line_corrections
                            .push(state.text_line_correction());
                    }
                    if state.tile_pattern.is_empty() {
                        let values = match operation.operator.as_str() {
                            "TJ" => operand(operands, 0)?.as_array()?.as_slice(),
                            "\"" => &operands[2..3],
                            _ => &operands[..1],
                        };
                        let elements = values
                            .iter()
                            .map(|value| match value {
                                Object::String(bytes, _) => {
                                    Ok(SourceTextElement::Bytes(bytes.clone()))
                                }
                                _ => number(value)
                                    .map(|value| SourceTextElement::Adjustment(value as f32)),
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        self.text_paints.push(SourceTextPaint {
                            line: state.text_line,
                            ctm: state.ctm,
                            font_size: state.text_font_size,
                            font_name: state.text_font_name.clone(),
                            space_codes: state.text_space_codes.clone(),
                            scale: state.text_scale,
                            rise: state.text_rise,
                            char_space: state.text_char_space,
                            word_space: state.text_word_space,
                            composite_font: state.text_composite_font,
                            vertical: state.text_vertical,
                            code_spaces: state.text_code_spaces.clone(),
                            run: state.text_run,
                            show: state.text_show,
                            elements,
                            has_text,
                            marked_content: self.marked_content.last().cloned().unwrap_or_default(),
                        });
                        state.text_show += 1;
                    }
                }
                "w" => state.stroke_width = operand_number(operands, 0)?,
                "RG" | "rg" | "K" | "k" | "G" | "g" => {
                    set_device_color(&mut state, &operation.operator, operands)?
                }
                "CS" => {
                    state.stroke_space =
                        self.color_space_name(resources, operand(operands, 0)?, 0)?;
                    state.stroke_color = initial_color(&state.stroke_space)?;
                    state.stroke_pattern.clear();
                }
                "cs" => {
                    state.fill_space =
                        self.color_space_name(resources, operand(operands, 0)?, 0)?;
                    state.fill_color = initial_color(&state.fill_space)?;
                    state.fill_pattern.clear();
                }
                "SC" | "SCN" => set_color(&mut state, false, operands)?,
                "sc" | "scn" => set_color(&mut state, true, operands)?,
                "d" => {
                    let array = operand(operands, 0)?.as_array()?;
                    state.dash_array = numbers(array)?;
                    state.dash_phase = operand_number(operands, 1)?;
                }
                "gs" => {
                    self.apply_ext_gstate(resources, &mut state, &name(operand(operands, 0)?)?)?
                }
                "m" => path.move_to(pair(operands, 0)?, &state)?,
                "l" => path.line_to(pair(operands, 0)?, &state)?,
                "c" => path.curve_to(
                    [pair(operands, 0)?, pair(operands, 2)?, pair(operands, 4)?],
                    CurveCommand::Cubic,
                    &state,
                )?,
                "v" => path.curve_to(
                    [
                        path.current
                            .ok_or_else(|| invalid("curveto without moveto"))?,
                        pair(operands, 0)?,
                        pair(operands, 2)?,
                    ],
                    CurveCommand::FirstControlAtCurrentPoint,
                    &state,
                )?,
                "y" => {
                    let end = pair(operands, 2)?;
                    path.curve_to(
                        [pair(operands, 0)?, end, end],
                        CurveCommand::LastControlAtEndpoint,
                        &state,
                    )?;
                }
                "h" => path.close(&state)?,
                "re" => {
                    let values = numbers(operands)?;
                    if values.len() != 4 {
                        return Err(invalid("rectangle needs four numbers").into());
                    }
                    let [x, y, width, height] = [values[0], values[1], values[2], values[3]];
                    path.move_to([x, y], &state)?;
                    // A rectangle retains all three edges even when its width
                    // or height is zero; explicit lineto elision does not apply.
                    for point in [[x + width, y], [x + width, y + height], [x, y + height]] {
                        path.command(SegmentKind::LineTo, point, false, &state)?;
                        path.current = Some(point);
                    }
                    path.close(&state)?;
                    // The first explicit line after a packed rectangle is
                    // retained, including a line back to its initial endpoint.
                    path.rectangle_endpoint_pending = true;
                }
                "W" | "W*" => path.pending_clip = true,
                "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*" | "n" => {
                    if matches!(operation.operator.as_str(), "s" | "b" | "b*") {
                        path.close(&state)?;
                    }
                    let fill = matches!(
                        operation.operator.as_str(),
                        "f" | "F" | "f*" | "B" | "B*" | "b" | "b*"
                    );
                    let stroke = matches!(
                        operation.operator.as_str(),
                        "S" | "s" | "B" | "B*" | "b" | "b*"
                    );
                    if fill || stroke {
                        let layer = self.layer().to_owned();
                        let fill_style = fill.then(|| self.paint_style(&state, true, &layer));
                        let stroke_style = stroke.then(|| self.paint_style(&state, false, &layer));
                        self.capture
                            .paint_transformed_commands(
                                &path.commands,
                                fill_style.as_ref(),
                                stroke_style.as_ref(),
                                |point| state.ctm.apply(point),
                            )
                            .map_err(invalid)?;
                        if fill && !state.fill_pattern.is_empty() {
                            self.process_pattern(
                                resources,
                                &state,
                                &path,
                                &state.fill_pattern,
                                default_ctm,
                            )?;
                        }
                    }
                    if path.pending_clip {
                        if !state.fill_pattern.is_empty() {
                            let mut style = self.paint_style(&state, true, self.layer());
                            style.color_space.clear();
                            style.fill_color.clear();
                            style.alpha = 1.0;
                            self.capture
                                .paint_transformed_commands(
                                    &path.commands,
                                    Some(&style),
                                    None,
                                    |point| state.ctm.apply(point),
                                )
                                .map_err(invalid)?;
                        }
                        let bounds = path.bounds.ok_or_else(|| invalid("clip without a path"))?;
                        self.push_clip(&mut state, bounds);
                    }
                    path = CurrentPath::default();
                }
                "BMC" => {
                    if self.layers.len() >= MAX_MARKED_CONTENT_DEPTH {
                        return Err(invalid("marked content nesting limit exceeded").into());
                    }
                    self.push_marked_content(resources, &operation)?;
                    self.layers.push(String::new());
                }
                "BDC" => {
                    if self.layers.len() >= MAX_MARKED_CONTENT_DEPTH {
                        return Err(invalid("marked content nesting limit exceeded").into());
                    }
                    self.push_marked_content(resources, &operation)?;
                    self.layers
                        .push(self.resolve_layer(resources, operands)?.unwrap_or_default());
                }
                "EMC" => {
                    self.layers
                        .pop()
                        .ok_or_else(|| invalid("unbalanced marked content end"))?;
                    self.marked_content.pop();
                }
                "BI" => self.record_image(&state),
                "Do" => self.process_xobject(resources, &state, &name(operand(operands, 0)?)?)?,
                _ => {}
            }
        }
        if !saved.is_empty() {
            return Err(invalid("unbalanced graphics state save").into());
        }
        for _ in initial_clip_depth..state.clip_depth {
            self.capture_clips.pop();
        }
        Ok(())
    }

    fn resource<'b>(
        &'b self,
        resources: &[&'b lopdf::Dictionary],
        category: &[u8],
        key: &[u8],
    ) -> Result<ResourceValue<'b>, Box<dyn Error>> {
        for dictionary in resources {
            let Ok(group) = dictionary.get(category) else {
                continue;
            };
            let (_, group) = self.source.dereference(group)?;
            let Ok(value) = group.as_dict()?.get(key) else {
                continue;
            };
            return Ok(Some(self.source.dereference(value)?));
        }
        Ok(None)
    }

    fn apply_ext_gstate(
        &self,
        resources: &[&lopdf::Dictionary],
        state: &mut GraphicsState,
        key: &str,
    ) -> Result<(), Box<dyn Error>> {
        let (_, value) = self
            .resource(resources, b"ExtGState", key.as_bytes())?
            .ok_or_else(|| invalid(format!("missing ExtGState {key}")))?;
        let dict = value.as_dict()?;
        if let Ok(value) = dict.get(b"CA") {
            state.stroke_alpha = number(value)?;
        }
        if let Ok(value) = dict.get(b"ca") {
            state.fill_alpha = number(value)?;
        }
        if let Ok(value) = dict.get(b"Font") {
            let values = value.as_array()?;
            let (_, font) = self.source.dereference(operand(values, 0)?)?;
            self.apply_text_font(state, font.as_dict()?)?;
            state.text_font_size = operand_number(values, 1)? as f32;
        }
        Ok(())
    }

    fn push_marked_content(
        &mut self,
        resources: &[&lopdf::Dictionary],
        operation: &lopdf::content::Operation,
    ) -> Result<(), Box<dyn Error>> {
        let mut resolved = operation.clone();
        if let Some(Object::Name(key)) = resolved.operands.get(1)
            && let Some((_, value)) = self.resource(resources, b"Properties", key)?
        {
            resolved.operands[1] = value.clone();
        }
        let mut context = self
            .marked_content
            .last()
            .map_or_else(Vec::new, |value| value.to_vec());
        context.extend(
            Content {
                operations: vec![resolved],
            }
            .encode()?,
        );
        self.marked_content.push(context.into());
        Ok(())
    }

    fn resolve_layer(
        &self,
        resources: &[&lopdf::Dictionary],
        operands: &[Object],
    ) -> Result<Option<String>, Box<dyn Error>> {
        if operands.len() < 2 || name(&operands[0])? != "OC" {
            return Ok(None);
        }
        let value = match &operands[1] {
            Object::Name(key) => self
                .resource(resources, b"Properties", key)?
                .map(|(_, value)| value),
            value => Some(value),
        };
        let Some(value) = value else {
            return Ok(None);
        };
        let dict = value.as_dict()?;
        let Some(name) = dict.get(b"Name").ok() else {
            return Ok(None);
        };
        let (_, name) = self.source.dereference(name)?;
        Ok(Some(String::from_utf8_lossy(name.as_str()?).into_owned()))
    }

    fn record_image(&mut self, state: &GraphicsState) {
        self.image_ops += 1;
        let box_points = self.transform_box(state.ctm, [0.0, 0.0, 1.0, 1.0]);
        let bounds = [
            box_points[0].max(self.page_bounds[0]),
            box_points[1].max(self.page_bounds[1]),
            box_points[2].min(self.page_bounds[2]),
            box_points[3].min(self.page_bounds[3]),
        ];
        if bounds[2] > bounds[0] && bounds[3] > bounds[1] {
            self.image_area += (bounds[2] - bounds[0]) * (bounds[3] - bounds[1]);
        }
    }

    fn process_xobject(
        &mut self,
        resources: &[&lopdf::Dictionary],
        state: &GraphicsState,
        key: &str,
    ) -> Result<(), Box<dyn Error>> {
        let Some((id, object)) = self.resource(resources, b"XObject", key.as_bytes())? else {
            return Err(invalid(format!("missing XObject {key}")).into());
        };
        let stream = object.as_stream()?.clone();
        match stream.dict.get(b"Subtype")?.as_name()? {
            b"Image" => {
                self.record_image(state);
                // The reference capture device counts path clips and pop_clip,
                // but has no clip_image_mask callback. A masked image therefore
                // pops its captured clip without changing PDF graphics state.
                for key in [b"SMask".as_slice(), b"Mask".as_slice()] {
                    if let Ok(mask) = stream.dict.get(key) {
                        let (_, mask) = self.source.dereference(mask)?;
                        if matches!(mask, Object::Stream(_)) {
                            self.capture_clips.pop();
                            break;
                        }
                    }
                }
                return Ok(());
            }
            b"Form" => {}
            _ => return Ok(()),
        }
        if self.active_forms.len() >= MAX_FORM_DEPTH {
            return Err(invalid("Form nesting limit exceeded").into());
        }
        if let Some(id) = id {
            if self.active_forms.contains(&id) {
                return Err(invalid("recursive Form XObject").into());
            }
            self.active_forms.push(id);
        }
        let mut nested = state.clone();
        if let Ok(matrix) = stream.dict.get(b"Matrix") {
            nested.ctm = state.ctm.compose(Matrix::from_objects(matrix.as_array()?)?);
        }
        nested.form_name = key.into();
        nested.form_ref = id.map_or_else(String::new, |(number, generation)| {
            format!("{number} {generation} R")
        });
        nested.form_ctm = Self::matrix_components(nested.ctm);
        if let Ok(box_object) = stream.dict.get(b"BBox") {
            let box_values = numbers(box_object.as_array()?)?;
            if box_values.len() != 4 {
                return Err(invalid("Form BBox needs four numbers").into());
            }
            let bounds = self.transform_box(
                nested.ctm,
                [box_values[0], box_values[1], box_values[2], box_values[3]],
            );
            self.push_clip(&mut nested, bounds);
        }
        let mut nested_resources = Vec::new();
        if let Ok(value) = stream.dict.get(b"Resources") {
            let (_, value) = self.source.dereference(value)?;
            nested_resources.push(value.as_dict()?);
        }
        nested_resources.extend_from_slice(resources);
        let bytes = if stream.dict.get(b"Filter").is_ok() {
            stream.decompressed_content()?
        } else {
            stream.content.clone()
        };
        let added_clips = nested.clip_depth - state.clip_depth;
        let result = self.process(&bytes, &nested_resources, nested);
        for _ in 0..added_clips {
            self.capture_clips.pop();
        }
        if id.is_some() {
            self.active_forms.pop();
        }
        result
    }

    fn process_pattern(
        &mut self,
        resources: &[&lopdf::Dictionary],
        state: &GraphicsState,
        path: &CurrentPath,
        key: &str,
        default_ctm: Matrix,
    ) -> Result<(), Box<dyn Error>> {
        let (id, object) = self
            .resource(resources, b"Pattern", key.as_bytes())?
            .ok_or_else(|| invalid(format!("missing Pattern {key}")))?;
        let dict = match object {
            lopdf::Object::Stream(stream) => &stream.dict,
            lopdf::Object::Dictionary(dict) => dict,
            _ => {
                return Err(invalid(format!(
                    "Pattern {key} is neither a stream nor a dictionary"
                ))
                .into());
            }
        };
        match dict.get(b"PatternType").and_then(lopdf::Object::as_i64) {
            Ok(PATTERN_TYPE_TILING) => {}
            // A shading fills the path with colour alone; the path's outline
            // was already captured by the caller, and there is no content
            // stream holding further vector paths to walk.
            Ok(PATTERN_TYPE_SHADING) => return Ok(()),
            Ok(other) => {
                return Err(
                    invalid(format!("Pattern {key} has unknown PatternType {other}")).into(),
                );
            }
            Err(_) => return Err(invalid(format!("Pattern {key} has no PatternType")).into()),
        }
        let stream = object.as_stream()?.clone();
        if let Some(id) = id {
            if self.active_patterns.len() >= MAX_PATTERN_DEPTH {
                return Err(invalid("pattern nesting limit exceeded").into());
            }
            if self.active_patterns.contains(&id) {
                return Err(invalid("recursive tiling pattern").into());
            }
            self.active_patterns.push(id);
        }
        let mut tile = state.clone();
        tile.ctm = default_ctm;
        if let Ok(matrix) = stream.dict.get(b"Matrix") {
            tile.ctm = default_ctm.compose(Matrix::from_objects(matrix.as_array()?)?);
        }
        tile.tile_pattern = key.into();
        tile.tile_id = i32::try_from(id.map_or(0, |reference| reference.0))?;
        tile.tile_ctm = Self::matrix_components(tile.ctm);
        self.push_clip(
            &mut tile,
            path.bounds
                .ok_or_else(|| invalid("pattern fill without path bounds"))?,
        );
        let mut nested_resources = Vec::new();
        if let Ok(value) = stream.dict.get(b"Resources") {
            let (_, value) = self.source.dereference(value)?;
            nested_resources.push(value.as_dict()?);
        }
        nested_resources.extend_from_slice(resources);
        let bytes = if stream.dict.get(b"Filter").is_ok() {
            stream.decompressed_content()?
        } else {
            stream.content.clone()
        };
        let result = self.process(&bytes, &nested_resources, tile);
        self.capture_clips.pop();
        if id.is_some() {
            self.active_patterns.pop();
        }
        result
    }

    fn transform_box(&self, matrix: Matrix, bounds: [f64; 4]) -> [f64; 4] {
        let corners = [
            [bounds[0], bounds[1]],
            [bounds[2], bounds[1]],
            [bounds[0], bounds[3]],
            [bounds[2], bounds[3]],
        ];
        let mut result = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for corner in corners {
            let [x, y] = matrix.apply(corner);
            result[0] = result[0].min(x);
            result[1] = result[1].min(y);
            result[2] = result[2].max(x);
            result[3] = result[3].max(y);
        }
        result
    }

    fn matrix_components(matrix: Matrix) -> Vec<f64> {
        matrix.0.map(f64::from).to_vec()
    }
}

fn operand(values: &[Object], index: usize) -> Result<&Object, io::Error> {
    values
        .get(index)
        .ok_or_else(|| invalid("missing PDF operator operand"))
}

fn initial_color(space: &str) -> Result<Vec<f64>, io::Error> {
    if space == "Pattern" {
        Ok(Vec::new())
    } else if space == "DeviceCMYK" || space.starts_with("ICCBased(CMYK,") {
        Ok(vec![0.0, 0.0, 0.0, 1.0])
    } else if matches!(space, "DeviceRGB" | "CalRGB" | "Lab")
        || space.starts_with("ICCBased(RGB,")
        || space.starts_with("ICCBased(Lab,")
    {
        Ok(vec![0.0; 3])
    } else if matches!(space, "DeviceGray" | "CalGray")
        || space.starts_with("ICCBased(Gray,")
        || space.starts_with("Indexed(")
    {
        Ok(vec![0.0])
    } else if space.starts_with("Separation(") {
        Ok(vec![1.0])
    } else if let Some(rest) = space.strip_prefix("DeviceN(") {
        let count: usize = rest
            .split(',')
            .next()
            .and_then(|value| value.parse().ok())
            .filter(|count| *count > 0)
            .ok_or_else(|| invalid("DeviceN component count is invalid"))?;
        Ok(vec![1.0; count])
    } else if let Some(rest) = space.strip_prefix("ICCBased(") {
        let count: usize = rest
            .split(',')
            .next()
            .and_then(|value| value.parse().ok())
            .filter(|count| *count > 0)
            .ok_or_else(|| invalid("ICC component count is invalid"))?;
        Ok(vec![0.0; count])
    } else {
        Err(invalid("colorspace has no initial component values"))
    }
}

fn operand_number(values: &[Object], index: usize) -> Result<f64, io::Error> {
    number(operand(values, index)?)
}

fn pair(values: &[Object], index: usize) -> Result<[f64; 2], io::Error> {
    Ok([
        operand_number(values, index)?,
        operand_number(values, index + 1)?,
    ])
}

fn set_device_color(
    state: &mut GraphicsState,
    operator: &str,
    values: &[Object],
) -> Result<(), io::Error> {
    let color = numbers(values)?;
    let expected = match operator {
        "G" | "g" => 1,
        "RG" | "rg" => 3,
        "K" | "k" => 4,
        _ => unreachable!(),
    };
    if color.len() != expected {
        return Err(invalid("wrong number of color components"));
    }
    let space = match expected {
        1 => "DeviceGray",
        3 => "DeviceRGB",
        4 => "DeviceCMYK",
        _ => unreachable!(),
    };
    if operator.as_bytes()[0].is_ascii_uppercase() {
        state.stroke_space = space.into();
        state.stroke_color = color;
        state.stroke_pattern.clear();
    } else {
        state.fill_space = space.into();
        state.fill_color = color;
        state.fill_pattern.clear();
    }
    Ok(())
}

fn set_color(state: &mut GraphicsState, fill: bool, values: &[Object]) -> Result<(), io::Error> {
    let (last, colors) = values
        .split_last()
        .ok_or_else(|| invalid("empty color operation"))?;
    let pattern = if let Object::Name(_) = last {
        name(last)?
    } else {
        String::new()
    };
    let components = if pattern.is_empty() {
        numbers(values)?
    } else {
        numbers(colors)?
    };
    if fill {
        state.fill_color = components;
        state.fill_pattern = pattern;
    } else {
        state.stroke_color = components;
        state.stroke_pattern = pattern;
    }
    Ok(())
}

/// Capture raw painted paths and image coverage using PDFium's page transform
/// and the PDF's content operators.
pub fn capture_page_content(
    path: &Path,
    page_number: u32,
) -> Result<SourcePageContent, Box<dyn Error>> {
    let path_str = path
        .to_str()
        .ok_or_else(|| invalid("PDF path is not UTF-8"))?;
    let library = Library::try_init()?;
    let document = library.load_document(path_str, None)?;
    if page_number == 0 || page_number > document.page_count() as u32 {
        return Err(invalid("page number is outside the document").into());
    }
    let page = document.page(page_number as i32 - 1)?;
    let source = SourceDocument::load(path)?;
    capture_loaded_page_content(&source, &page, page_number)
}

/// Reuse an open document for streaming multi-page extraction. The caller owns
/// the PDFium library lock, so this does not initialize a second library.
pub fn capture_loaded_page_content(
    source: &SourceDocument,
    page: &Page<'_, '_>,
    page_number: u32,
) -> Result<SourcePageContent, Box<dyn Error>> {
    let view_box = page
        .view_box()
        .ok_or_else(|| invalid("page has no view box"))?;
    let page_id = *source
        .get_pages()
        .get(&page_number)
        .ok_or_else(|| invalid("source page unavailable"))?;
    let resources = crate::page_resource_chain(source, page_id)?;
    let viewport = page.viewport_transform(&view_box);
    let (width_pts, height_pts) = page.viewport_size(&view_box);
    let mut reader = StreamReader {
        source,
        capture: PathCapture::default(),
        capture_clips: Vec::new(),
        layers: Vec::new(),
        marked_content: Vec::new(),
        active_forms: Vec::new(),
        active_patterns: Vec::new(),
        page_bounds: [0.0, 0.0, f64::from(width_pts), f64::from(height_pts)],
        image_ops: 0,
        image_area: 0.0,
        text_line_corrections: Vec::new(),
        text_paints: Vec::new(),
        next_text_run: 1,
    };
    reader.process(
        &source.get_page_content(page_id)?,
        &resources,
        GraphicsState {
            ctm: Matrix::from_viewport(viewport),
            ..GraphicsState::default()
        },
    )?;
    // A viewport array the page cannot be read from states nothing about the
    // page's drawings; the line work does not depend on it.
    let (viewports, viewports_error) = match read_page_viewports(source, page_id, |x, y| {
        let (x, y) = viewport.transform_point(x as f32, y as f32);
        (f64::from(x), f64::from(y))
    }) {
        Ok(viewports) => (viewports, None),
        Err(error) => (Vec::new(), Some(error.to_string())),
    };
    Ok(SourcePageContent {
        paths: reader.capture,
        viewports,
        viewports_error,
        width_pts: f64::from(width_pts),
        height_pts: f64::from(height_pts),
        image_ops: reader.image_ops,
        text_line_corrections: reader.text_line_corrections,
        text_paints: reader.text_paints,
        image_coverage: image_coverage(
            reader.image_area,
            f64::from(width_pts),
            f64::from(height_pts),
        ),
    })
}

/// Capture only the path fields of [`capture_page_content`].
pub fn capture_page_paths(path: &Path, page_number: u32) -> Result<PathCapture, Box<dyn Error>> {
    Ok(capture_page_content(path, page_number)?.paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_space_mappings_preserve_code_lengths_ranges_and_overrides() {
        use lopdf::{Stream, dictionary};

        let mut source = SourceDocument::with_version("1.7");
        let base = source.add_object(Stream::new(
            dictionary! {},
            b"3 beginbfchar <01> <0020> <0001> <0020> <03> <0020> endbfchar".to_vec(),
        ));
        let child = source.add_object(Stream::new(
            dictionary! {"UseCMap" => base},
            b"1 beginbfchar <01> <0041> endbfchar 2 beginbfrange <0001> <0003> [<0041> <0020> <0042>] <10> <30> <000f> endbfrange".to_vec(),
        ));
        assert_eq!(
            unicode_space_codes(&source, &Object::Reference(child), 0).unwrap(),
            BTreeSet::from([(2, 2), (3, 1), (0x21, 1)])
        );
        let named = Object::Stream(Stream::new(
            dictionary! {},
            b"/Identity-H usecmap 2 beginbfchar <0020> <0041> <0001> <0020> endbfchar".to_vec(),
        ));
        assert_eq!(
            unicode_space_codes(&source, &named, 0).unwrap(),
            BTreeSet::from([(1, 2)])
        );
    }

    #[test]
    fn unicode_space_mappings_reject_invalid_ranges_arrays_and_cycles() {
        use lopdf::{Stream, dictionary};

        let mut source = SourceDocument::with_version("1.7");
        for bytes in [
            b"1 beginbfrange <03> <01> <0020> endbfrange".as_slice(),
            b"1 beginbfrange <01> <03> [<0020>] endbfrange".as_slice(),
            b"1 beginbfchar <01> endbfchar".as_slice(),
        ] {
            let cmap = Object::Stream(Stream::new(dictionary! {}, bytes.to_vec()));
            assert!(unicode_space_codes(&source, &cmap, 0).is_err());
        }
        let cycle = source.new_object_id();
        source.objects.insert(
            cycle,
            Object::Stream(Stream::new(
                dictionary! {"UseCMap" => cycle},
                b"1 beginbfchar <01> <0020> endbfchar".to_vec(),
            )),
        );
        assert!(unicode_space_codes(&source, &Object::Reference(cycle), 0).is_err());
    }

    #[test]
    fn cmap_inheritance_preserves_local_ranges_mode_and_rejects_recursion() {
        use lopdf::{Stream, dictionary};

        let mut source = SourceDocument::with_version("1.7");
        let base = source.add_object(Stream::new(
            dictionary! {},
            b"1 begincodespacerange <0000> <ffff> endcodespacerange".to_vec(),
        ));
        let inherited = source.add_object(Stream::new(
            dictionary! {"UseCMap" => base, "WMode" => 1},
            b"/WMode 0 def".to_vec(),
        ));
        let local = source.add_object(Stream::new(
            dictionary! {"UseCMap" => base},
            b"1 begincodespacerange <20> <7f> endcodespacerange".to_vec(),
        ));
        let named = source.add_object(Stream::new(dictionary! {}, b"/Identity-H usecmap".to_vec()));
        let recursive = source.new_object_id();
        source.objects.insert(
            recursive,
            Object::Stream(Stream::new(
                dictionary! {"UseCMap" => recursive},
                Vec::new(),
            )),
        );
        let reader = StreamReader {
            source: &source,
            capture: PathCapture::default(),
            capture_clips: Vec::new(),
            layers: Vec::new(),
            marked_content: Vec::new(),
            active_forms: Vec::new(),
            active_patterns: Vec::new(),
            page_bounds: [0.0; 4],
            image_ops: 0,
            image_area: 0.0,
            text_line_corrections: Vec::new(),
            text_paints: Vec::new(),
            next_text_run: 1,
        };
        let mut state = GraphicsState::default();
        reader
            .apply_text_encoding(&mut state, &Object::Reference(inherited), 0)
            .unwrap();
        assert!(
            state.text_vertical,
            "stream dictionary overrides body writing mode"
        );
        assert_eq!(state.text_code_spaces.len(), 1);
        assert_eq!(state.text_code_spaces[0].length, 2);
        assert_eq!(state.text_code_spaces[0].low, 0);
        assert_eq!(state.text_code_spaces[0].high, u32::from(u16::MAX));
        reader
            .apply_text_encoding(&mut state, &Object::Reference(local), 0)
            .unwrap();
        assert!(!state.text_vertical);
        assert_eq!(state.text_code_spaces.len(), 1);
        assert_eq!(state.text_code_spaces[0].length, 1);
        assert_eq!(state.text_code_spaces[0].low, u32::from(b' '));
        assert_eq!(state.text_code_spaces[0].high, u32::from(b'\x7f'));
        reader
            .apply_text_encoding(&mut state, &Object::Reference(named), 0)
            .unwrap();
        assert_eq!(state.text_code_spaces[0].length, 2);
        let error = reader
            .apply_text_encoding(&mut state, &Object::Reference(recursive), 0)
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "CMap inheritance exceeds the depth limit"
        );
    }

    fn pattern_fill_reader(source: &SourceDocument) -> StreamReader<'_> {
        StreamReader {
            source,
            capture: PathCapture::default(),
            capture_clips: Vec::new(),
            layers: Vec::new(),
            marked_content: Vec::new(),
            active_forms: Vec::new(),
            active_patterns: Vec::new(),
            page_bounds: [0.0, 0.0, 100.0, 100.0],
            image_ops: 0,
            image_area: 0.0,
            text_line_corrections: Vec::new(),
            text_paints: Vec::new(),
            next_text_run: 1,
        }
    }

    const PATTERN_FILL_CONTENT: &[u8] = b"/Pattern cs /P0 scn 10 10 30 20 re f";

    #[test]
    fn shading_pattern_fill_keeps_the_filled_path() {
        use lopdf::dictionary;

        let source = SourceDocument::with_version("1.7");
        let resources = dictionary! {
            "Pattern" => dictionary! {
                "P0" => dictionary! {
                    "Type" => "Pattern",
                    "PatternType" => PATTERN_TYPE_SHADING,
                    "Shading" => dictionary! {
                        "ShadingType" => 2,
                        "ColorSpace" => "DeviceRGB",
                        "Coords" => vec![0.into(), 0.into(), 1.into(), 0.into()],
                    },
                },
            },
        };
        let mut reader = pattern_fill_reader(&source);
        reader
            .process(
                PATTERN_FILL_CONTENT,
                &[&resources],
                GraphicsState::default(),
            )
            .expect("a shading-pattern fill is valid PDF and must not fail extraction");
        assert_eq!(reader.capture.paths.len(), 1);
    }

    #[test]
    fn pattern_without_a_known_type_is_rejected() {
        use lopdf::dictionary;

        let source = SourceDocument::with_version("1.7");
        let resources = dictionary! {
            "Pattern" => dictionary! { "P0" => dictionary! { "Type" => "Pattern" } },
        };
        let mut reader = pattern_fill_reader(&source);
        let error = reader
            .process(
                PATTERN_FILL_CONTENT,
                &[&resources],
                GraphicsState::default(),
            )
            .expect_err("a pattern with no PatternType cannot be walked");
        assert!(error.to_string().contains("no PatternType"), "{error}");
    }

    #[test]
    fn rotated_translations_preserve_reference_path_coordinates() {
        let viewport = Matrix([0.0, -1.0, -1.0, 0.0, 1684.0, 1191.0]);
        for (translation, point, expected) in [
            (
                [494.76, 361.121],
                [-3.54, -116.64],
                [1439.51904296875, 699.7799682617188],
            ),
            (
                [390.12, 621.941],
                [0.0, -7.14],
                [1069.1990966796875, 800.8800048828125],
            ),
        ] {
            let source = Matrix([1.0, 0.0, 0.0, 1.0, translation[0], translation[1]]);
            assert_eq!(viewport.compose(source).apply(point), expected);
            assert_ne!(viewport.apply(source.apply(point)), expected);
        }
    }
}
