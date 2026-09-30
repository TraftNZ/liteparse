//! PDF geometry source inspection for the PDFium and content-stream extractor.
//!
//! PDFium exposes painted objects and raw path points, while the source stream
//! retains resource names, raw color operands, clipping and marked content.
//! This probe records both views of one page so their ordering and provenance
//! can be matched before the extractor relies on either view alone.

use std::error::Error;
use std::io;
use std::path::Path;

use lopdf::{Dictionary, Document as SourceDocument, Object, ObjectId};
use pdfium::{Font, Library, PageObject, PageObjectKind};
use serde::Serialize;
use std::collections::HashSet;

pub mod artifact;
pub mod classify;
mod color_space;
mod compat_math;
mod compat_sort;
pub mod content_paths;
pub mod geometry;
mod inline_images;
pub mod measurement;
pub mod model;
pub mod operation;
pub mod path_capture;
pub mod preview;
pub mod raster;
pub mod relations;
pub mod text_capture;
pub mod viewports;

/// Resolve page resources in nearest-first order, including dictionaries
/// embedded directly on ancestor `/Pages` nodes.
fn page_resource_chain(
    source: &SourceDocument,
    page_id: ObjectId,
) -> Result<Vec<&Dictionary>, Box<dyn Error>> {
    let mut resources = Vec::new();
    let mut visited = HashSet::new();
    let mut current = Some(page_id);
    while let Some(id) = current {
        if !visited.insert(id) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "PDF page parent cycle").into());
        }
        let node = source.get_dictionary(id)?;
        if let Ok(value) = node.get(b"Resources") {
            let (_, resolved) = source.dereference(value)?;
            resources.push(resolved.as_dict()?);
        }
        current = node
            .get(b"Parent")
            .ok()
            .map(Object::as_reference)
            .transpose()?;
    }
    Ok(resources)
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    use lopdf::dictionary;

    #[test]
    fn includes_inline_ancestor_resources_after_page_resources() {
        let mut source = SourceDocument::new();
        source.objects.insert(
            (1, 0),
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Resources" => dictionary! { "XObject" => dictionary! { "IM7" => (4, 0) } },
            }),
        );
        source.objects.insert(
            (2, 0),
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => (1, 0),
                "Resources" => (3, 0),
            }),
        );
        source.objects.insert(
            (3, 0),
            Object::Dictionary(dictionary! { "Font" => dictionary! { "F1" => (5, 0) } }),
        );
        let chain = page_resource_chain(&source, (2, 0)).unwrap();
        assert_eq!(chain.len(), 2);
        assert!(chain[0].get(b"Font").is_ok());
        assert!(chain[1].get(b"XObject").is_ok());
    }
}

#[derive(Debug, Serialize)]
pub struct PageProbe {
    pub page: u32,
    pub width_pts: f32,
    pub height_pts: f32,
    pub rotation: i32,
    pub user_unit: f32,
    pub objects: Vec<ObjectProbe>,
    pub text_samples: Vec<TextSampleProbe>,
    pub text_rects_y_up: Vec<[f64; 4]>,
    pub streams: Vec<StreamProbe>,
    pub resources: Vec<ResourceProbe>,
}

#[derive(Debug, Serialize)]
pub struct ObjectProbe {
    pub kind: String,
    pub form_depth: usize,
    pub matrix: Option<[f32; 6]>,
    pub bounds: Option<[f32; 4]>,
    pub path_draw_mode: Option<[bool; 2]>,
    pub stroke_width: Option<f32>,
    pub stroke_rgba: Option<[u8; 4]>,
    pub fill_rgba: Option<[u8; 4]>,
    pub image_size: Option<[u32; 2]>,
    pub path_points: Vec<PathPointProbe>,
}

#[derive(Debug, Serialize)]
pub struct TextSampleProbe {
    pub character: Option<char>,
    pub font_name: Option<String>,
    pub font_size: f64,
    pub font_ascent: Option<f32>,
    pub font_descent: Option<f32>,
    pub angle: f32,
    pub box_y_up: Option<[f64; 4]>,
}

#[derive(Debug, Serialize)]
pub struct PathPointProbe {
    pub kind: String,
    pub point: Option<[f32; 2]>,
    pub close: bool,
}

#[derive(Debug, Serialize)]
pub struct StreamProbe {
    pub source: String,
    pub operations: Vec<OperationProbe>,
}

#[derive(Debug, Serialize)]
pub struct OperationProbe {
    pub operator: String,
    pub operands: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct ResourceProbe {
    pub owner: String,
    pub category: String,
    pub name: String,
    pub object_number: Option<u32>,
    pub generation: Option<u16>,
    pub layer_name: Option<String>,
    pub properties: Vec<(String, String)>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn collect_object(
    object: PageObject<'_, '_>,
    depth: usize,
    out: &mut Vec<ObjectProbe>,
) -> Result<(), io::Error> {
    const MAX_FORM_DEPTH: usize = 16;
    if depth > MAX_FORM_DEPTH {
        return Err(invalid("form nesting exceeds the supported depth"));
    }
    let kind = object.kind();
    let matrix = object.matrix().map(|m| [m.a, m.b, m.c, m.d, m.e, m.f]);
    let mut path_points = Vec::new();
    if kind == PageObjectKind::Path {
        let count = object
            .path_segment_count()
            .ok_or_else(|| invalid("path segment count unavailable"))?;
        for index in 0..count {
            let segment = object
                .path_segment(index)
                .ok_or_else(|| invalid("path segment unavailable"))?;
            path_points.push(PathPointProbe {
                kind: format!("{:?}", segment.kind),
                point: segment.point.map(|(x, y)| [x, y]),
                close: segment.close,
            });
        }
    }
    out.push(ObjectProbe {
        kind: format!("{kind:?}"),
        form_depth: depth,
        matrix,
        bounds: object.bounds().map(|b| [b.left, b.bottom, b.right, b.top]),
        path_draw_mode: object
            .path_draw_mode()
            .map(|mode| [mode.filled, mode.stroked]),
        stroke_width: object.stroke_width(),
        stroke_rgba: object.stroke_color().map(|c| [c.r, c.g, c.b, c.a]),
        fill_rgba: object.fill_color().map(|c| [c.r, c.g, c.b, c.a]),
        image_size: object.image_metadata().map(|m| [m.width, m.height]),
        path_points,
    });
    if kind == PageObjectKind::Form {
        let count = object
            .form_object_count()
            .ok_or_else(|| invalid("form object count unavailable"))?;
        for index in 0..count {
            let child = object
                .form_object(index)
                .ok_or_else(|| invalid("form child unavailable"))?;
            collect_object(child, depth + 1, out)?;
        }
    }
    Ok(())
}

fn collect_stream(
    source: String,
    bytes: &[u8],
    out: &mut Vec<StreamProbe>,
) -> Result<(), Box<dyn Error>> {
    let operations = inline_images::decode(bytes)?;
    out.push(StreamProbe {
        source,
        operations: operations
            .into_iter()
            .map(|op| OperationProbe {
                operator: op.operator,
                operands: op
                    .operands
                    .iter()
                    .map(|value| format!("{value:?}"))
                    .collect(),
            })
            .collect(),
    });
    Ok(())
}

fn inspect_resource_dictionary(
    source: &SourceDocument,
    dictionary: &lopdf::Dictionary,
    owner: &str,
    active_refs: &mut Vec<lopdf::ObjectId>,
    streams: &mut Vec<StreamProbe>,
    resources: &mut Vec<ResourceProbe>,
) -> Result<(), Box<dyn Error>> {
    const MAX_RESOURCE_DEPTH: usize = 16;
    if active_refs.len() > MAX_RESOURCE_DEPTH {
        return Err(invalid("resource nesting exceeds the supported depth").into());
    }
    for category in [
        b"XObject".as_slice(),
        b"Pattern",
        b"Properties",
        b"ExtGState",
        b"Font",
    ] {
        let Ok(group) = dictionary.get(category) else {
            continue;
        };
        let (_, group) = source.dereference(group)?;
        for (name, value) in group.as_dict()?.iter() {
            let (id, object) = source.dereference(value)?;
            let (object_number, generation) = id
                .map(|(number, generation)| (Some(number), Some(generation)))
                .unwrap_or((None, None));
            let name = String::from_utf8_lossy(name).into_owned();
            let category_name = String::from_utf8_lossy(category).into_owned();
            let resource_owner = if owner == "page" {
                format!("{category_name}/{name}")
            } else {
                format!("{owner}/{category_name}/{name}")
            };
            let resource_dict = match object {
                Object::Dictionary(dict) => Some(dict),
                Object::Stream(stream) => Some(&stream.dict),
                _ => None,
            };
            let layer_name = if category == b"Properties" {
                resource_dict
                    .and_then(|dict| dict.get(b"Name").ok())
                    .and_then(|value| value.as_str().ok())
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            } else {
                None
            };
            let mut properties = Vec::new();
            if let Some(dict) = resource_dict {
                for key in [
                    b"Subtype".as_slice(),
                    b"BaseFont",
                    b"BBox",
                    b"Matrix",
                    b"OC",
                    b"PatternType",
                    b"PaintType",
                    b"XStep",
                    b"YStep",
                    b"CA",
                    b"ca",
                ] {
                    if let Ok(value) = dict.get(key) {
                        properties.push((
                            String::from_utf8_lossy(key).into_owned(),
                            format!("{value:?}"),
                        ));
                    }
                }
            }
            resources.push(ResourceProbe {
                owner: owner.to_owned(),
                category: category_name.clone(),
                name: name.clone(),
                object_number,
                generation,
                layer_name,
                properties,
            });
            if let Object::Stream(stream) = object {
                let is_form =
                    category == b"XObject" && stream.dict.get(b"Subtype")?.as_name()? == b"Form";
                if is_form || category == b"Pattern" {
                    let bytes = stream.get_plain_content()?;
                    collect_stream(resource_owner.clone(), &bytes, streams)?;
                    if let Ok(nested) = stream.dict.get(b"Resources") {
                        if id.is_some_and(|reference| active_refs.contains(&reference)) {
                            continue;
                        }
                        if let Some(reference) = id {
                            active_refs.push(reference);
                        }
                        let (_, nested) = source.dereference(nested)?;
                        inspect_resource_dictionary(
                            source,
                            nested.as_dict()?,
                            &resource_owner,
                            active_refs,
                            streams,
                            resources,
                        )?;
                        if id.is_some() {
                            active_refs.pop();
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn inspect_resources(
    source: &SourceDocument,
    page_id: lopdf::ObjectId,
    streams: &mut Vec<StreamProbe>,
    resources: &mut Vec<ResourceProbe>,
) -> Result<(), Box<dyn Error>> {
    for dict in page_resource_chain(source, page_id)? {
        inspect_resource_dictionary(source, dict, "page", &mut Vec::new(), streams, resources)?;
    }
    Ok(())
}

/// Inspect one page through PDFium and its serialized content streams.
///
/// The result contains raw observations rather than inferred style: callers
/// can compare paint order and source operands against a baseline before
/// assigning them to final geometry primitives.
pub fn inspect_page(path: &Path, page_number: u32) -> Result<PageProbe, Box<dyn Error>> {
    let path_str = path
        .to_str()
        .ok_or_else(|| invalid("PDF path is not UTF-8"))?;
    let library = Library::try_init()?;
    let document = library.load_document(path_str, None)?;
    if page_number == 0 || page_number > document.page_count() as u32 {
        return Err(invalid("page number is outside the document").into());
    }
    let page = document.page(page_number as i32 - 1)?;
    let view = page
        .view_box()
        .ok_or_else(|| invalid("page has no view box"))?;
    let (width_pts, height_pts) = page.viewport_size(&view);
    let mut objects = Vec::new();
    for index in 0..page.object_count() {
        let object = page
            .object(index)
            .ok_or_else(|| invalid("page object unavailable"))?;
        collect_object(object, 0, &mut objects)?;
    }
    const MAX_TEXT_SAMPLES: usize = 128;
    let text_page = page.text()?;
    let text_samples = text_page
        .chars()
        .take(MAX_TEXT_SAMPLES)
        .map(|character| {
            let font_size = character.font_size();
            let font = character
                .text_object()
                .and_then(|object| unsafe { Font::from_text_object(object) });
            TextSampleProbe {
                character: char::from_u32(character.unicode()),
                font_name: character.font_name(),
                font_size,
                font_ascent: font.as_ref().and_then(|font| font.ascent(font_size as f32)),
                font_descent: font
                    .as_ref()
                    .and_then(|font| font.descent(font_size as f32)),
                angle: character.angle(),
                box_y_up: character
                    .char_box()
                    .map(|b| [b.left, b.bottom, b.right, b.top]),
            }
        })
        .collect();
    const MAX_TEXT_RECTS: i32 = 128;
    let rect_count = text_page.count_rects(0, -1).min(MAX_TEXT_RECTS);
    let text_rects_y_up = (0..rect_count)
        .filter_map(|index| text_page.rect(index))
        .map(|rect| [rect.left, rect.bottom, rect.right, rect.top])
        .collect();

    let source = SourceDocument::load(path)?;
    let page_id = *source
        .get_pages()
        .get(&page_number)
        .ok_or_else(|| invalid("source page unavailable"))?;
    let mut streams = Vec::new();
    collect_stream(
        "page".into(),
        &source.get_page_content(page_id)?,
        &mut streams,
    )?;
    let mut resources = Vec::new();
    inspect_resources(&source, page_id, &mut streams, &mut resources)?;
    Ok(PageProbe {
        page: page_number,
        width_pts,
        height_pts,
        rotation: page.rotation(),
        user_unit: page.user_unit(),
        objects,
        text_samples,
        text_rects_y_up,
        streams,
        resources,
    })
}
