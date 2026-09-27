//! Machine-readable, one-request PDF operations for external consumers.

use std::error::Error;
use std::io::{self, Read};

use base64::Engine as _;
use image::{DynamicImage, ImageFormat, RgbImage, RgbaImage};
use lopdf::{Document as SourceDocument, Object};
use pdfium::{
    Bitmap, BitmapFormat, Library, Matrix, Page, PageObject, PageObjectKind, RectF, TextPage,
};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: u32 = 1;
const POINTS_PER_INCH: f64 = 72.0;
const MAX_PIXMAP_BYTES: u64 = 150 * 1024 * 1024;
const BYTES_PER_PIXEL: u64 = 4;
const MIN_DPI: u32 = 72;
const DEFAULT_RENDER_DPI: u32 = 300;
const DEFAULT_SCALE_DPI: u32 = 150;
const JPEG_QUALITY: u8 = 80;
const DIMENSION_EPSILON: f64 = 0.0001;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PdfOperation {
    Geometry,
    Measure,
    PageCount,
    RenderPages,
    PageInfo,
    RenderCrop,
    RenderScale,
    NativeClip,
    ExtractImages,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PdfRequest {
    version: u32,
    operation: PdfOperation,
    pdf_path: String,
    page_number: Option<u32>,
    page_numbers: Option<Vec<u32>>,
    dpi: Option<u32>,
    /// Top-left fractions [x, y, width, height].
    rect: Option<[f64; 4]>,
    geometry: Option<liteparse_geometry::operation::GeometryOptions>,
    measurement: Option<liteparse_geometry::measurement::MeasurementOptions>,
}

#[derive(Debug, Serialize)]
struct PageInfo {
    width_pt: f32,
    height_pt: f32,
}

#[derive(Debug, Serialize)]
struct PageImage {
    page_number: u32,
    data: String,
    mime_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    effective_dpi: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    x: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    y: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    width: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    height: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_alpha: Option<bool>,
}

#[derive(Debug, Serialize)]
struct PdfResponse {
    version: u32,
    total_pages: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_info: Option<PageInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<PageImage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<PageImage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    measurement: Option<liteparse_geometry::measurement::Measurement>,
}

fn capture_geometry_text(
    page: &Page<'_, '_>,
    text: &TextPage<'_, '_>,
    view: &RectF,
) -> Result<liteparse_geometry::text_capture::NativeTextCapture, Box<dyn Error>> {
    let tokens = liteparse::extract::extract_page_text_objects(page, text, view, None)?
        .into_iter()
        .map(|item| Ok(serde_json::from_value(serde_json::to_value(item)?)?))
        .collect::<Result<_, Box<dyn Error>>>()?;
    Ok(liteparse_geometry::text_capture::NativeTextCapture {
        tokens,
        characters: liteparse::extract::recover_page_glyph_characters(text),
    })
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn validate_rect(rect: [f64; 4]) -> Result<[f64; 4], io::Error> {
    let [x, y, width, height] = rect;
    if rect.iter().any(|value| !value.is_finite())
        || x < 0.0
        || y < 0.0
        || width <= 0.0
        || height <= 0.0
        || x + width > 1.0
        || y + height > 1.0
    {
        return Err(invalid(
            "rect must be positive, finite, and inside the unit page",
        ));
    }
    Ok(rect)
}

fn requested_dpi(request: &PdfRequest, default: u32) -> Result<u32, io::Error> {
    let dpi = request.dpi.unwrap_or(default);
    if dpi == 0 {
        return Err(invalid("dpi must be positive"));
    }
    Ok(dpi)
}

fn safe_dpi(requested: u32, width_pt: f64, height_pt: f64) -> Result<u32, io::Error> {
    if !width_pt.is_finite() || !height_pt.is_finite() || width_pt <= 0.0 || height_pt <= 0.0 {
        return Err(invalid("page dimensions must be positive and finite"));
    }
    let max_pixels = (MAX_PIXMAP_BYTES / BYTES_PER_PIXEL) as f64;
    let max_dpi = (POINTS_PER_INCH * (max_pixels / (width_pt * height_pt)).sqrt()).floor();
    if max_dpi < MIN_DPI as f64 {
        return Err(invalid(
            "page exceeds the render memory budget even at 72 DPI",
        ));
    }
    let effective = requested.min(max_dpi as u32);
    if effective != requested {
        eprintln!(
            "[lit pdf] reducing render DPI from {requested} to {effective} for memory budget"
        );
    }
    Ok(effective)
}

fn page_number(request: &PdfRequest, total: i32) -> Result<u32, io::Error> {
    let page = request
        .page_number
        .ok_or_else(|| invalid("page_number is required"))?;
    if page == 0 || page > total as u32 {
        return Err(invalid(format!("page_number {page} outside 1..={total}")));
    }
    Ok(page)
}

fn page_size(page: &Page<'_, '_>) -> Result<(f64, f64), io::Error> {
    let view = page
        .view_box()
        .ok_or_else(|| invalid("page has no bounding box"))?;
    let (width, height) = page.viewport_size(&view);
    if width <= 0.0 || height <= 0.0 {
        return Err(invalid("page has zero dimensions"));
    }
    Ok((width as f64, height as f64))
}

fn pixel_size(width_pt: f64, height_pt: f64, dpi: u32) -> Result<(i32, i32), io::Error> {
    let scale = dpi as f64 / POINTS_PER_INCH;
    let width = (width_pt * scale - DIMENSION_EPSILON).ceil();
    let height = (height_pt * scale - DIMENSION_EPSILON).ceil();
    if width < 1.0 || height < 1.0 || width > i32::MAX as f64 || height > i32::MAX as f64 {
        return Err(invalid("render dimensions are out of range"));
    }
    if width * height * BYTES_PER_PIXEL as f64 > MAX_PIXMAP_BYTES as f64 {
        return Err(invalid("render exceeds the pixel memory budget"));
    }
    Ok((width as i32, height as i32))
}

fn encode_jpeg(image: &RgbImage) -> Result<String, Box<dyn Error>> {
    let bytes = crate::pdf_jpeg::encode(image, JPEG_QUALITY)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn full_render(page: &Page<'_, '_>, dpi: u32) -> Result<RgbImage, Box<dyn Error>> {
    let (width_pt, height_pt) = page_size(page)?;
    let (width, height) = pixel_size(width_pt, height_pt, dpi)?;
    // The allocation is explicitly checked above, before PDFium sees it.
    let bitmap = unsafe { Bitmap::new(width, height) }?;
    bitmap.fill_rect(0, 0, width, height, 0xFFFF_FFFF);
    page.render_into_scaled(
        &bitmap,
        dpi as f32 / POINTS_PER_INCH as f32 * page.user_unit(),
        [0.0, 0.0],
        // Page contents exclude annotation appearances, matching content-only
        // renderers such as MuPDF's fz_run_page_contents.
        pdfium::pdfium_sys::FPDF_PRINTING as i32,
    )?;
    RgbImage::from_raw(width as u32, height as u32, bitmap.to_rgb())
        .ok_or_else(|| invalid("PDFium returned an invalid RGB raster").into())
}

fn native_clip_pixels(
    width_pt: f64,
    height_pt: f64,
    dpi: u32,
    rect: [f64; 4],
) -> Result<(i32, i32, i32, i32), io::Error> {
    let [x, y, width, height] = validate_rect(rect)?;
    let scale = dpi as f64 / POINTS_PER_INCH;
    let x0 = ((x * width_pt * scale) as f32).floor() as i32;
    let y0 = ((y * height_pt * scale) as f32).floor() as i32;
    let x1 = ((((x + width) * width_pt * scale) as f32) as f64 - DIMENSION_EPSILON).ceil() as i32;
    let y1 = ((((y + height) * height_pt * scale) as f32) as f64 - DIMENSION_EPSILON).ceil() as i32;
    Ok((x0, y0, x1, y1))
}

fn native_clip(page: &Page<'_, '_>, dpi: u32, rect: [f64; 4]) -> Result<RgbImage, Box<dyn Error>> {
    let (width_pt, height_pt) = page_size(page)?;
    let full_width = (width_pt * dpi as f64 / POINTS_PER_INCH - DIMENSION_EPSILON).ceil();
    let full_height = (height_pt * dpi as f64 / POINTS_PER_INCH - DIMENSION_EPSILON).ceil();
    if full_width > i32::MAX as f64 || full_height > i32::MAX as f64 {
        return Err(invalid("full-page viewport is out of range").into());
    }
    let (x0, y0, x1, y1) = native_clip_pixels(width_pt, height_pt, dpi, rect)?;
    let width = x1 - x0;
    let height = y1 - y0;
    if width <= 0
        || height <= 0
        || width as u64 * height as u64 * BYTES_PER_PIXEL > MAX_PIXMAP_BYTES
    {
        return Err(invalid("native clip exceeds the pixel memory budget").into());
    }
    let bitmap = unsafe { Bitmap::new(width, height) }?;
    bitmap.fill_rect(0, 0, width, height, 0xFFFF_FFFF);
    page.render_into_scaled(
        &bitmap,
        dpi as f32 / POINTS_PER_INCH as f32 * page.user_unit(),
        [-(x0 as f32), -(y0 as f32)],
        pdfium::pdfium_sys::FPDF_PRINTING as i32,
    )?;
    RgbImage::from_raw(width as u32, height as u32, bitmap.to_rgb())
        .ok_or_else(|| invalid("PDFium returned an invalid clipped raster").into())
}

fn jpeg_page(
    page_number: u32,
    image: &RgbImage,
    effective_dpi: u32,
) -> Result<PageImage, Box<dyn Error>> {
    Ok(PageImage {
        page_number,
        data: encode_jpeg(image)?,
        mime_type: "image/jpeg",
        effective_dpi: Some(effective_dpi),
        x: None,
        y: None,
        width: None,
        height: None,
        has_alpha: None,
    })
}

fn bitmap_rgba(bitmap: &Bitmap<'_>) -> Result<RgbaImage, io::Error> {
    let width = bitmap.width() as usize;
    let height = bitmap.height() as usize;
    let stride = bitmap.stride() as usize;
    let format = bitmap.format();
    let bpp = format
        .bytes_per_pixel()
        .ok_or_else(|| invalid("unknown embedded image bitmap format"))?;
    let mut pixels = Vec::with_capacity(width * height * 4);
    for row in bitmap.buffer().chunks_exact(stride).take(height) {
        for pixel in row[..width * bpp].chunks_exact(bpp) {
            let rgba = match format {
                BitmapFormat::Gray => [pixel[0], pixel[0], pixel[0], 255],
                BitmapFormat::Bgr => [pixel[2], pixel[1], pixel[0], 255],
                BitmapFormat::Bgrx => [pixel[2], pixel[1], pixel[0], 255],
                BitmapFormat::Bgra | BitmapFormat::BgraPremul => {
                    [pixel[2], pixel[1], pixel[0], pixel[3]]
                }
                BitmapFormat::Unknown => unreachable!(),
            };
            pixels.extend_from_slice(&rgba);
        }
    }
    RgbaImage::from_raw(width as u32, height as u32, pixels)
        .ok_or_else(|| invalid("invalid embedded image bitmap"))
}

fn image_soft_mask<'a>(
    source_doc: &'a SourceDocument,
    raw: &[u8],
) -> Result<Option<&'a lopdf::Stream>, Box<dyn Error>> {
    let mut matched = false;
    let mut mask_ref = None;
    for object in source_doc.objects.values() {
        let Object::Stream(stream) = object else {
            continue;
        };
        if stream.dict.get(b"Subtype").and_then(Object::as_name).ok() != Some(b"Image")
            || stream.content != raw
        {
            continue;
        }
        let candidate = stream
            .dict
            .get(b"SMask")
            .ok()
            .and_then(|value| value.as_reference().ok());
        if matched && candidate != mask_ref {
            return Err(invalid("identical image streams have different soft masks").into());
        }
        matched = true;
        mask_ref = candidate;
    }
    if !matched {
        return Err(invalid("source image could not be matched to its PDF image stream").into());
    }
    let Some(mask_ref) = mask_ref else {
        return Ok(None);
    };
    let mask = source_doc.get_object(mask_ref)?.as_stream()?;
    Ok(Some(mask))
}

fn apply_soft_mask(pixels: &mut RgbaImage, mask: &lopdf::Stream) -> Result<(), Box<dyn Error>> {
    let width = mask.dict.get(b"Width")?.as_i64()?;
    let height = mask.dict.get(b"Height")?.as_i64()?;
    let depth = mask.dict.get(b"BitsPerComponent")?.as_i64()?;
    if width != pixels.width() as i64 || height != pixels.height() as i64 || depth != 8 {
        return Err(invalid("soft mask dimensions or bit depth differ from source image").into());
    }
    let alpha = if mask.dict.has(b"Filter") {
        mask.decompressed_content()?
    } else {
        mask.content.clone()
    };
    if alpha.len() != pixels.width() as usize * pixels.height() as usize {
        return Err(invalid("soft mask decoded size does not match source image").into());
    }
    for (pixel, value) in pixels.pixels_mut().zip(alpha) {
        pixel[3] = value;
        if value == 0 {
            pixel[0] = 0;
            pixel[1] = 0;
            pixel[2] = 0;
        }
    }
    Ok(())
}

fn identity_matrix() -> Matrix {
    Matrix {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    }
}

fn compose_matrix(outer: Matrix, inner: Matrix) -> Matrix {
    Matrix {
        a: outer.a * inner.a + outer.c * inner.b,
        b: outer.b * inner.a + outer.d * inner.b,
        c: outer.a * inner.c + outer.c * inner.d,
        d: outer.b * inner.c + outer.d * inner.d,
        e: outer.a * inner.e + outer.c * inner.f + outer.e,
        f: outer.b * inner.e + outer.d * inner.f + outer.f,
    }
}

fn transform_rect(rect: RectF, matrix: Matrix) -> RectF {
    let points = [
        (rect.left, rect.bottom),
        (rect.left, rect.top),
        (rect.right, rect.bottom),
        (rect.right, rect.top),
    ];
    let mut out = RectF {
        left: f32::INFINITY,
        right: f32::NEG_INFINITY,
        top: f32::NEG_INFINITY,
        bottom: f32::INFINITY,
    };
    for (x, y) in points {
        let px = matrix.a * x + matrix.c * y + matrix.e;
        let py = matrix.b * x + matrix.d * y + matrix.f;
        out.left = out.left.min(px);
        out.right = out.right.max(px);
        out.bottom = out.bottom.min(py);
        out.top = out.top.max(py);
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn collect_images(
    page: &Page<'_, '_>,
    source: PageObject<'_, '_>,
    parent: Matrix,
    source_doc: &SourceDocument,
    page_number: u32,
    view_box: &RectF,
    page_width: f64,
    page_height: f64,
    images: &mut Vec<PageImage>,
) -> Result<(), Box<dyn Error>> {
    match source.kind() {
        PageObjectKind::Form => {
            let form_matrix = source
                .matrix()
                .ok_or_else(|| invalid("form matrix unavailable"))?;
            let transform = compose_matrix(parent, form_matrix);
            let count = source
                .form_object_count()
                .ok_or_else(|| invalid("form object count unavailable"))?;
            for index in 0..count {
                let child = source
                    .form_object(index)
                    .ok_or_else(|| invalid("form child unavailable"))?;
                collect_images(
                    page,
                    child,
                    transform,
                    source_doc,
                    page_number,
                    view_box,
                    page_width,
                    page_height,
                    images,
                )?;
            }
        }
        PageObjectKind::Image => {
            let bounds = source
                .bounds()
                .ok_or_else(|| invalid("image bounds unavailable"))?;
            let page_bounds = transform_rect(bounds, parent);
            let viewport = page.bounds_to_viewport(view_box, &page_bounds);
            let bitmap = source
                .image_bitmap()
                .ok_or_else(|| invalid("image source bitmap unavailable"))?;
            let mut pixels = bitmap_rgba(&bitmap)?;
            let raw = source
                .image_data_raw()
                .ok_or_else(|| invalid("source image data unavailable"))?;
            let soft_mask = image_soft_mask(source_doc, &raw)?;
            if let Some(mask) = soft_mask {
                apply_soft_mask(&mut pixels, mask)?;
            }
            let has_alpha = soft_mask.is_some() || pixels.pixels().any(|pixel| pixel[3] != 255);
            let mut bytes = std::io::Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(pixels).write_to(&mut bytes, ImageFormat::Png)?;
            images.push(PageImage {
                page_number,
                data: base64::engine::general_purpose::STANDARD.encode(bytes.into_inner()),
                mime_type: "image/png",
                effective_dpi: None,
                x: Some(viewport.left as f64 / page_width),
                y: Some(viewport.top as f64 / page_height),
                width: Some((viewport.right - viewport.left) as f64 / page_width),
                height: Some((viewport.bottom - viewport.top) as f64 / page_height),
                has_alpha: Some(has_alpha),
            });
        }
        _ => {}
    }
    Ok(())
}

fn embedded_images(
    page: &Page<'_, '_>,
    page_number: u32,
    source_doc: &SourceDocument,
) -> Result<Vec<PageImage>, Box<dyn Error>> {
    let (page_width, page_height) = page_size(page)?;
    let view_box = page
        .view_box()
        .ok_or_else(|| invalid("page has no bounding box"))?;
    let mut images = Vec::new();
    for index in 0..page.object_count() {
        let object = page
            .object(index)
            .ok_or_else(|| invalid("page object unavailable"))?;
        collect_images(
            page,
            object,
            identity_matrix(),
            source_doc,
            page_number,
            &view_box,
            page_width,
            page_height,
            &mut images,
        )?;
    }
    Ok(images)
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let request: PdfRequest = serde_json::from_str(&input)?;
    if request.version != SCHEMA_VERSION {
        return Err(invalid(format!(
            "unsupported PDF request version {}",
            request.version
        ))
        .into());
    }
    let library = Library::try_init()?;
    let document = library.load_document(&request.pdf_path, None)?;
    let total_pages = document.page_count();
    if total_pages <= 0 {
        return Err(invalid("PDF has no pages").into());
    }
    let mut response = PdfResponse {
        version: SCHEMA_VERSION,
        total_pages,
        page_info: None,
        image: None,
        images: None,
        measurement: None,
    };
    match request.operation {
        PdfOperation::Measure => {
            let number = page_number(&request, total_pages)?;
            if request.page_numbers.is_some() {
                return Err(invalid("measure requires one page_number").into());
            }
            let measurement = request
                .measurement
                .ok_or_else(|| invalid("measure requires measurement options"))?;
            let options = request.geometry.unwrap_or_default();
            if options.artifact_out.is_some() || options.summary {
                return Err(
                    invalid("measure does not write geometry artifacts or summaries").into(),
                );
            }
            drop(document);
            drop(library);
            liteparse_geometry::operation::run_geometry_with_consumer(
                std::path::Path::new(&request.pdf_path),
                Some(&[number]),
                &options,
                capture_geometry_text,
                |page| {
                    response.measurement = Some(liteparse_geometry::measurement::measure_page(
                        page,
                        &measurement,
                    )?);
                    Ok(())
                },
                &mut io::sink(),
            )?;
            println!("{}", serde_json::to_string(&response)?);
            return Ok(());
        }
        PdfOperation::Geometry => {
            // The geometry operation owns its PDFium library lock for the
            // whole streaming extraction. Release this operation's handles
            // before transferring control to it.
            drop(document);
            drop(library);
            if request.page_number.is_some() && request.page_numbers.is_some() {
                return Err(invalid("use page_number or page_numbers, not both").into());
            }
            let pages = request
                .page_numbers
                .or_else(|| request.page_number.map(|number| vec![number]));
            return liteparse_geometry::operation::run_geometry(
                std::path::Path::new(&request.pdf_path),
                pages.as_deref(),
                &request.geometry.unwrap_or_default(),
                capture_geometry_text,
                &mut io::stdout().lock(),
            );
        }
        PdfOperation::PageCount => {}
        PdfOperation::RenderPages => {
            let dpi = requested_dpi(&request, DEFAULT_RENDER_DPI)?;
            let pages = request
                .page_numbers
                .as_ref()
                .ok_or_else(|| invalid("page_numbers is required"))?;
            let mut images = Vec::new();
            for &number in pages {
                if number == 0 || number > total_pages as u32 {
                    eprintln!("[lit pdf] skipping invalid page {number}");
                    continue;
                }
                let page = match document.page(number as i32 - 1) {
                    Ok(page) => page,
                    Err(err) => {
                        eprintln!("[lit pdf] skipping unreadable page {number}: {err}");
                        continue;
                    }
                };
                let (width, height) = page_size(&page)?;
                let effective = safe_dpi(dpi, width, height)?;
                images.push(jpeg_page(
                    number,
                    &full_render(&page, effective)?,
                    effective,
                )?);
            }
            response.images = Some(images);
        }
        PdfOperation::PageInfo => {
            let number = page_number(&request, total_pages)?;
            let page = document.page(number as i32 - 1)?;
            let (width, height) = page_size(&page)?;
            response.page_info = Some(PageInfo {
                width_pt: width as f32,
                height_pt: height as f32,
            });
        }
        PdfOperation::RenderCrop | PdfOperation::RenderScale | PdfOperation::NativeClip => {
            let number = page_number(&request, total_pages)?;
            let page = document.page(number as i32 - 1)?;
            let (width, height) = page_size(&page)?;
            let default_dpi = if matches!(request.operation, PdfOperation::RenderScale) {
                DEFAULT_SCALE_DPI
            } else {
                DEFAULT_RENDER_DPI
            };
            let requested = requested_dpi(&request, default_dpi)?;
            let rect = if matches!(request.operation, PdfOperation::RenderScale) {
                [0.0, 0.0, 1.0, 1.0]
            } else {
                validate_rect(request.rect.ok_or_else(|| invalid("rect is required"))?)?
            };
            let effective = if matches!(request.operation, PdfOperation::NativeClip) {
                safe_dpi(requested, width * rect[2], height * rect[3])?
            } else {
                safe_dpi(requested, width, height)?
            };
            let image = if matches!(request.operation, PdfOperation::NativeClip) {
                native_clip(&page, effective, rect)?
            } else {
                let full = full_render(&page, effective)?;
                if matches!(request.operation, PdfOperation::RenderCrop) {
                    let x0 = (rect[0] * full.width() as f64) as u32;
                    let y0 = (rect[1] * full.height() as f64) as u32;
                    let crop_width = ((rect[2] * full.width() as f64) as u32)
                        .max(1)
                        .min(full.width() - x0);
                    let crop_height = ((rect[3] * full.height() as f64) as u32)
                        .max(1)
                        .min(full.height() - y0);
                    image::imageops::crop_imm(&full, x0, y0, crop_width, crop_height).to_image()
                } else {
                    full
                }
            };
            response.image = Some(jpeg_page(number, &image, effective)?);
        }
        PdfOperation::ExtractImages => {
            let number = page_number(&request, total_pages)?;
            let page = document.page(number as i32 - 1)?;
            let source_doc = SourceDocument::load(&request.pdf_path)?;
            response.images = Some(embedded_images(&page, number, &source_doc)?);
        }
    }
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    #[test]
    fn rejects_invalid_soft_mask_filters() {
        const WIDTH: u32 = 2;
        const HEIGHT: u32 = 2;
        const BITS_PER_COMPONENT: i64 = 8;
        const INVALID_FILTER: i64 = 7;
        const ALPHA_SAMPLES: [u8; 4] = [0, 85, 170, 255];
        let mask = lopdf::Stream::new(
            dictionary! {
                "Width" => i64::from(WIDTH),
                "Height" => i64::from(HEIGHT),
                "BitsPerComponent" => BITS_PER_COMPONENT,
                "Filter" => INVALID_FILTER,
            },
            ALPHA_SAMPLES.to_vec(),
        );
        let mut pixels = RgbaImage::new(WIDTH, HEIGHT);
        assert!(apply_soft_mask(&mut pixels, &mask).is_err());
    }

    #[test]
    fn rejects_invalid_fraction_rectangles() {
        for rect in [
            [0.0, 0.0, 0.0, 1.0],
            [-0.1, 0.0, 0.5, 0.5],
            [0.8, 0.0, 0.3, 0.5],
            [f64::NAN, 0.0, 0.1, 0.1],
        ] {
            assert!(validate_rect(rect).is_err());
        }
    }

    #[test]
    fn rounds_native_clip_outward_in_page_space() {
        assert_eq!(
            native_clip_pixels(1190.52, 841.92, 200, [0.4, 0.4, 0.1, 0.1]).unwrap(),
            (1322, 935, 1654, 1170),
        );
    }

    #[test]
    fn caps_dpi_with_an_explicit_72_dpi_floor() {
        const FLOOR_SIZED_PAGE_PT: f64 = 6270.0;
        assert_eq!(
            safe_dpi(300, FLOOR_SIZED_PAGE_PT, FLOOR_SIZED_PAGE_PT).unwrap(),
            72
        );
        assert!(safe_dpi(300, 20_000.0, 20_000.0).is_err());
    }

    #[test]
    fn native_clip_does_not_change_the_loaded_page() {
        const BASELINE_DPI: u32 = 72;
        const CLIP_DPI: u32 = 200;
        const CLIP_RECT: [f64; 4] = [0.1, 0.1, 0.7, 0.7];
        let library = Library::try_init().unwrap();
        let fixture = format!(
            "{}/tests/fixtures/rotated_user_unit.pdf",
            env!("CARGO_MANIFEST_DIR")
        );
        let document = library.load_document(&fixture, None).unwrap();
        let page = document.page(0).unwrap();
        let before = full_render(&page, BASELINE_DPI).unwrap();
        native_clip(&page, CLIP_DPI, CLIP_RECT).unwrap();
        let after = full_render(&page, BASELINE_DPI).unwrap();
        assert_eq!(before.as_raw(), after.as_raw());
    }
}
