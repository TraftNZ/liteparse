//! Stream complete page extraction to an artifact and bounded stdout preview.

use std::error::Error;
use std::io::{self, Write};
use std::path::PathBuf;

use lopdf::Document as SourceDocument;
use pdfium::{Library, Page, RectF, TextPage};
use serde::Deserialize;

use crate::artifact::{ArtifactWriter, LimitedWriter, MAX_PREVIEW_BYTES};
use crate::content_paths::capture_loaded_page_content;
use crate::geometry::assemble_vector_page;
use crate::preview::{PreviewOptions, filter_page};
use crate::raster::trace_raster_segments;
use crate::relations::analyze_page;
use crate::text_capture::{
    NativeTextCapture, annotate_text_tokens, group_text_tokens,
    restore_loaded_source_font_metadata, restore_source_text_positions,
};

const DEFAULT_RASTER_DPI: f64 = 300.0;
const MAX_RASTER_BYTES: f64 = 150.0 * 1024.0 * 1024.0;
const POINTS_PER_INCH: f64 = 72.0;
const MAX_RASTER_DIMENSION: f64 = i32::MAX as f64;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GeometryOptions {
    pub preview: PreviewOptions,
    pub raster_dpi: f64,
    pub no_raster: bool,
    pub force_raster: bool,
    pub relations: bool,
    pub artifact_out: Option<PathBuf>,
    pub summary: bool,
}

impl Default for GeometryOptions {
    fn default() -> Self {
        Self {
            preview: PreviewOptions::default(),
            raster_dpi: DEFAULT_RASTER_DPI,
            no_raster: false,
            force_raster: false,
            relations: false,
            artifact_out: None,
            summary: false,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// Native text capture stays in liteparse; geometry assembly, raster tracing,
/// graph analysis, preview filtering and artifact I/O stay in this crate.
pub fn run_geometry(
    path: &std::path::Path,
    page_numbers: Option<&[u32]>,
    options: &GeometryOptions,
    capture_text: impl Fn(
        &Page<'_, '_>,
        &TextPage<'_, '_>,
        &RectF,
    ) -> Result<NativeTextCapture, Box<dyn Error>>,
    output: &mut impl Write,
) -> Result<(), Box<dyn Error>> {
    run_geometry_with_consumer(
        path,
        page_numbers,
        options,
        capture_text,
        |_| Ok(()),
        output,
    )
}

/// Consume each complete page before preview filtering, without reparsing a
/// capped geometry response. Consumers share the same extraction and limits.
pub fn run_geometry_with_consumer(
    path: &std::path::Path,
    page_numbers: Option<&[u32]>,
    options: &GeometryOptions,
    capture_text: impl Fn(
        &Page<'_, '_>,
        &TextPage<'_, '_>,
        &RectF,
    ) -> Result<NativeTextCapture, Box<dyn Error>>,
    mut consume: impl FnMut(&crate::model::PageGeometry) -> Result<(), Box<dyn Error>>,
    output: &mut impl Write,
) -> Result<(), Box<dyn Error>> {
    if !options.raster_dpi.is_finite() || options.raster_dpi <= 0.0 {
        return Err(invalid("raster_dpi must be finite and positive").into());
    }
    if !options.preview.min_length_pts.is_finite() || options.preview.min_length_pts < 0.0 {
        return Err(invalid("min_length_pts must be finite and nonnegative").into());
    }
    if options.preview.bbox_frac.is_some_and(|bounds| {
        bounds
            .iter()
            .any(|value| !value.is_finite() || !(0.0..=1.0).contains(value))
    }) {
        return Err(
            invalid("bbox_frac must contain four finite fractions between zero and one").into(),
        );
    }
    if options.artifact_out.as_deref().is_some_and(|destination| {
        destination == path
            || std::fs::canonicalize(destination)
                .ok()
                .zip(std::fs::canonicalize(path).ok())
                .is_some_and(|(destination, source)| destination == source)
    }) {
        return Err(invalid("artifact_out must not overwrite the input PDF").into());
    }
    let library = Library::try_init()?;
    let document = library.load_document(
        path.to_str()
            .ok_or_else(|| invalid("PDF path is not UTF-8"))?,
        None,
    )?;
    let total = document.page_count() as u32;
    let pages = page_numbers.map_or_else(|| (1..=total).collect::<Vec<_>>(), <[u32]>::to_vec);
    if pages.iter().any(|number| *number == 0 || *number > total) {
        return Err(invalid("page number is outside the document").into());
    }
    let source = SourceDocument::load(path)?;
    let mut artifact = options
        .artifact_out
        .as_deref()
        .map(ArtifactWriter::create)
        .transpose()?;
    // Buffer only the capped preview. A failed extraction cannot leave partial
    // JSON on stdout; the full artifact is serialized one page at a time.
    let mut preview_bytes = Vec::new();
    let mut preview = LimitedWriter::new(&mut preview_bytes, MAX_PREVIEW_BYTES);
    if !options.summary {
        preview.write_all(b"[")?;
    }
    for (index, number) in pages.into_iter().enumerate() {
        let page = document.page(number as i32 - 1)?;
        let view = page
            .view_box()
            .ok_or_else(|| invalid("page has no view box"))?;
        let text = page.text()?;
        let NativeTextCapture {
            mut tokens,
            characters,
        } = capture_text(&page, &text, &view)?;
        let content = capture_loaded_page_content(&source, &page, number)?;
        annotate_text_tokens(&page, &text, &view, &content, &characters, &mut tokens);
        // The geometry does not depend on where the text is placed. A page whose
        // text cannot be matched to its content stream keeps PDFium's positions
        // and says why, rather than losing its geometry and every other page.
        let mut placed = tokens.clone();
        let text_positions_error =
            match restore_source_text_positions(&page, &text, &content, &characters, &mut placed) {
                Ok(()) => {
                    tokens = placed;
                    None
                }
                Err(error) => Some(error.to_string()),
            };
        restore_loaded_source_font_metadata(&source, &mut tokens);
        let mut geometry = assemble_vector_page(number, content, group_text_tokens(&tokens));
        geometry.text_positions_error = text_positions_error;
        if (geometry.class == "raster" || options.force_raster) && !options.no_raster {
            let width = (geometry.width_pts * options.raster_dpi / POINTS_PER_INCH).ceil();
            let height = (geometry.height_pts * options.raster_dpi / POINTS_PER_INCH).ceil();
            let stride = (width / 4.0).ceil() * 4.0;
            if width <= 0.0
                || height <= 0.0
                || width > MAX_RASTER_DIMENSION
                || height > MAX_RASTER_DIMENSION
                || stride * height > MAX_RASTER_BYTES
            {
                return Err(invalid(
                    "geometry raster exceeds the render memory budget at requested DPI",
                )
                .into());
            }
            let bitmap = page.render_gray(options.raster_dpi as f32)?;
            let (segments, confidence) = trace_raster_segments(
                bitmap.buffer(),
                bitmap.width() as usize,
                bitmap.height() as usize,
                bitmap.stride() as usize,
                options.raster_dpi,
            );
            geometry.segments = (!segments.is_empty()).then_some(segments);
            geometry.polylines = None;
            geometry.paths = None;
            geometry.source = "raster-traced".into();
            geometry.confidence = confidence;
        }
        if options.relations {
            analyze_page(&mut geometry);
        }
        consume(&geometry)?;
        if let Some(artifact) = &mut artifact {
            artifact.append(&geometry)?;
        }
        if options.summary {
            writeln!(
                preview,
                "p{}: class={} source={} confidence={:.2} paths={} segments={} polylines={} images={} text={} size={:.0}x{:.0}pt",
                geometry.page,
                geometry.class,
                geometry.source,
                geometry.confidence,
                geometry.path_ops,
                geometry.segments.as_ref().map_or(0, Vec::len),
                geometry.polylines.as_ref().map_or(0, Vec::len),
                geometry.image_ops,
                geometry.text_spans.as_ref().map_or(0, Vec::len),
                geometry.width_pts,
                geometry.height_pts
            )?;
        } else {
            if index > 0 {
                preview.write_all(b",")?;
            }
            serde_json::to_writer(&mut preview, &filter_page(&geometry, &options.preview))?;
        }
    }
    if !options.summary {
        preview.write_all(b"]\n")?;
    }
    if let Some(artifact) = artifact {
        artifact.finish()?;
    }
    output.write_all(&preview_bytes)?;
    output.flush()?;
    Ok(())
}
