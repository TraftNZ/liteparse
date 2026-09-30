//! The viewports a page declares for itself (`/VP`, ISO 32000-2 §12.9).
//!
//! A CAD layout exported to PDF often records each drawing's rectangle on the
//! sheet with a `/Measure` dictionary saying how many model units one point of
//! that rectangle stands for. That is the file's own statement of the drawing's
//! scale. It is recorded here as read; whether it is believed is decided by the
//! consumer against the drawing's printed dimensions, because a writer may copy
//! one layout's viewports onto every page of a set.

use std::io;

use lopdf::{Dictionary, Document as SourceDocument, Object, ObjectId, decode_text_string};

use crate::model::{Viewport, ViewportScale};

/// Read a page's `/VP` array. `to_page` maps default user space onto the
/// viewport space the rest of the page geometry is reported in.
pub fn read_page_viewports(
    source: &SourceDocument,
    page_id: ObjectId,
    to_page: impl Fn(f64, f64) -> (f64, f64),
) -> Result<Vec<Viewport>, io::Error> {
    let page = source.get_dictionary(page_id).map_err(invalid_pdf)?;
    let Ok(entries) = page.get(b"VP") else {
        return Ok(Vec::new());
    };
    resolved(source, entries)?
        .as_array()
        .map_err(invalid_pdf)?
        .iter()
        .map(|entry| {
            read_viewport(
                source,
                resolved(source, entry)?.as_dict().map_err(invalid_pdf)?,
                &to_page,
            )
        })
        .collect()
}

fn read_viewport(
    source: &SourceDocument,
    entry: &Dictionary,
    to_page: &impl Fn(f64, f64) -> (f64, f64),
) -> Result<Viewport, io::Error> {
    let corners = resolved(source, entry.get(b"BBox").map_err(invalid_pdf)?)?
        .as_array()
        .map_err(invalid_pdf)?
        .iter()
        .map(|value| number(resolved(source, value)?))
        .collect::<Result<Vec<_>, _>>()?;
    let [x0, y0, x1, y1]: [f64; 4] = corners
        .try_into()
        .map_err(|_| invalid("a viewport BBox has four numbers"))?;
    let points = [
        to_page(x0, y0),
        to_page(x1, y0),
        to_page(x1, y1),
        to_page(x0, y1),
    ];
    let (xs, ys): (Vec<f64>, Vec<f64>) = points.into_iter().unzip();
    let name = entry
        .get(b"Name")
        .ok()
        .map(|value| {
            resolved(source, value).and_then(|value| decode_text_string(value).map_err(invalid_pdf))
        })
        .transpose()?
        .and_then(non_blank);
    let scale = entry
        .get(b"Measure")
        .ok()
        .map(|value| {
            read_scale(
                source,
                resolved(source, value)?.as_dict().map_err(invalid_pdf)?,
            )
        })
        .transpose()?
        .flatten();
    Ok(Viewport {
        bbox: [min(&xs), min(&ys), max(&xs), max(&ys)],
        name,
        scale,
    })
}

/// The rectilinear measure's first x-axis number format. Its `/C` converts one
/// point of the viewport into model units. A geospatial measure states no such
/// factor, so it records no scale.
fn read_scale(
    source: &SourceDocument,
    measure: &Dictionary,
) -> Result<Option<ViewportScale>, io::Error> {
    let rectilinear = measure
        .get(b"Subtype")
        .ok()
        .map(|value| resolved(source, value))
        .transpose()?
        .is_none_or(|subtype| matches!(subtype, Object::Name(name) if name == b"RL"));
    if !rectilinear {
        return Ok(None);
    }
    let formats = resolved(source, measure.get(b"X").map_err(invalid_pdf)?)?
        .as_array()
        .map_err(invalid_pdf)?;
    let first = formats
        .first()
        .ok_or_else(|| invalid("a rectilinear measure has at least one x number format"))?;
    let format = resolved(source, first)?.as_dict().map_err(invalid_pdf)?;
    let units_per_point = number(resolved(source, format.get(b"C").map_err(invalid_pdf)?)?)?;
    if !units_per_point.is_finite() || units_per_point <= 0.0 {
        return Err(invalid("a number format's conversion factor is positive"));
    }
    Ok(Some(ViewportScale {
        units_per_point,
        units: text(source, format, b"U")?,
        ratio: text(source, measure, b"R")?,
    }))
}

fn text(
    source: &SourceDocument,
    dictionary: &Dictionary,
    key: &[u8],
) -> Result<Option<String>, io::Error> {
    let Ok(value) = dictionary.get(key) else {
        return Ok(None);
    };
    Ok(non_blank(
        decode_text_string(resolved(source, value)?).map_err(invalid_pdf)?,
    ))
}

fn non_blank(value: String) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn resolved<'a>(source: &'a SourceDocument, value: &'a Object) -> Result<&'a Object, io::Error> {
    source
        .dereference(value)
        .map(|(_, value)| value)
        .map_err(invalid_pdf)
}

fn number(value: &Object) -> Result<f64, io::Error> {
    match value {
        Object::Integer(value) => Ok(*value as f64),
        Object::Real(value) => Ok(f64::from(*value)),
        _ => Err(invalid("expected a PDF number")),
    }
}

fn min(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::INFINITY, f64::min)
}

fn max(values: &[f64]) -> f64 {
    values.iter().copied().fold(f64::NEG_INFINITY, f64::max)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

fn invalid_pdf(error: lopdf::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{StringFormat, dictionary};

    fn page_with(viewports: Object) -> (SourceDocument, ObjectId) {
        let mut source = SourceDocument::new();
        let page_id = (1, 0);
        source.objects.insert(
            page_id,
            Object::Dictionary(dictionary! { "Type" => "Page", "VP" => viewports }),
        );
        (source, page_id)
    }

    fn rectilinear(units_per_point: f32) -> Object {
        Object::Dictionary(dictionary! {
            "Type" => "Measure",
            "Subtype" => "RL",
            "R" => Object::String(b" ".to_vec(), StringFormat::Literal),
            "X" => vec![Object::Dictionary(dictionary! {
                "C" => Object::Real(units_per_point),
                "U" => Object::String(b"mm".to_vec(), StringFormat::Literal),
            })],
        })
    }

    fn flip(height: f64) -> impl Fn(f64, f64) -> (f64, f64) {
        move |x, y| (x, height - y)
    }

    #[test]
    fn reads_each_viewport_box_and_its_stated_scale() {
        let (source, page_id) = page_with(Object::Array(vec![
            Object::Dictionary(dictionary! {
                "Type" => "Viewport",
                "BBox" => vec![2.into(), 2.into(), 2381.into(), 1681.into()],
                "Measure" => rectilinear(0.35279),
            }),
            Object::Dictionary(dictionary! {
                "Type" => "Viewport",
                "Name" => Object::String(b"Section A".to_vec(), StringFormat::Literal),
                "BBox" => vec![67.into(), 198.into(), 2318.into(), 915.into()],
                "Measure" => rectilinear(10.58285),
            }),
        ]));

        let viewports = read_page_viewports(&source, page_id, flip(1684.0)).unwrap();

        assert_eq!(viewports.len(), 2);
        assert_eq!(viewports[1].bbox, [67.0, 769.0, 2318.0, 1486.0]);
        assert_eq!(viewports[1].name.as_deref(), Some("Section A"));
        let scale = viewports[1].scale.as_ref().unwrap();
        assert!((scale.units_per_point - 10.58285).abs() < 1e-4);
        assert_eq!(scale.units.as_deref(), Some("mm"));
        assert_eq!(scale.ratio, None, "a blank ratio label states nothing");
    }

    #[test]
    fn a_page_without_viewports_reads_none() {
        let mut source = SourceDocument::new();
        source
            .objects
            .insert((1, 0), Object::Dictionary(dictionary! { "Type" => "Page" }));
        assert!(
            read_page_viewports(&source, (1, 0), flip(100.0))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_geospatial_measure_states_no_scale() {
        let (source, page_id) = page_with(Object::Array(vec![Object::Dictionary(dictionary! {
            "BBox" => vec![0.into(), 0.into(), 10.into(), 10.into()],
            "Measure" => dictionary! { "Type" => "Measure", "Subtype" => "GEO" },
        })]));
        let viewports = read_page_viewports(&source, page_id, flip(10.0)).unwrap();
        assert_eq!(viewports.len(), 1);
        assert!(viewports[0].scale.is_none());
    }

    #[test]
    fn a_malformed_box_is_an_error_not_a_guess() {
        let (source, page_id) = page_with(Object::Array(vec![Object::Dictionary(dictionary! {
            "BBox" => vec![0.into(), 0.into(), 10.into()],
        })]));
        assert!(read_page_viewports(&source, page_id, flip(10.0)).is_err());
    }
}
