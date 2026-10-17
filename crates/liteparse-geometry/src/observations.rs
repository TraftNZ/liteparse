//! Source metadata observations, not accepted object identities or calibrations.
//!
//! ISO 32000-1 sections 12.5 and 14.3 define annotation and document-info
//! dictionaries. Contents are preserved separately from authoritative page text:
//! a person's measurement markup must never silently become a printed dimension.

use lopdf::{Dictionary, Document, Object, ObjectId, decode_text_string};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const MAX_ANNOTATIONS: usize = 10_000;
const MAX_OBSERVATION_TEXT_BYTES: usize = 64 * 1024;
const MAX_ERRORS: usize = 20;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageObservations {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub export_metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    pub annotations_checked: bool,
    pub annotations_total: usize,
    #[serde(default)]
    pub annotations: Vec<AnnotationObservation>,
    #[serde(default)]
    pub marked_groups: Vec<MarkedGroup>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnnotationObservation {
    /// Array slot and indirect object locator survive duplicate contents.
    pub index: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_ref: Option<String>,
    pub subtype: String,
    pub provenance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    pub contents: String,
    /// All four source corners transformed to top-left page points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bbox: Option<[f64; 4]>,
    pub has_measure: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkedGroup {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_matrix: Option<MarkedViewMatrix>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_matrix_error: Option<String>,
    pub index: usize,
    pub parent: usize,
    pub tag: String,
    /// Resolved BMC/BDC operands in PDF syntax, with no inferred Revit key.
    pub source: String,
    /// Lossless operands when canonical PDF syntax is not UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_hex: Option<String>,
    /// Source-user-space to page points at entry, not a model registration.
    pub ctm: [f64; 6],
    pub path_begin: usize,
    pub path_end: usize,
    pub text_begin: usize,
    pub text_end: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub form_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip_bbox: Option<[f64; 4]>,
}

/// Exporter's private VP array, retained as a statement without inferring a
/// building registration, matrix multiplication convention or model namespace.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkedViewMatrix {
    pub matrix: [f64; 16],
    pub units: Option<String>,
    pub precision: Option<i64>,
}

pub(crate) fn read_marked_view(
    doc: &Document,
    operation: &lopdf::content::Operation,
) -> Result<Option<MarkedViewMatrix>, String> {
    let tag = operation
        .operands
        .first()
        .and_then(|v| v.as_name().ok())
        .unwrap_or_default();
    if !tag.starts_with(b"ViewRegion") {
        return Ok(None);
    }
    let Some(Object::Dictionary(dict)) = operation.operands.get(1) else {
        return Ok(None);
    };
    let Ok(array) = dict.get(b"VP") else {
        return Ok(None);
    };
    let values = resolve(doc, array)?.as_array().map_err(|e| e.to_string())?;
    if values.len() != 16 {
        return Err("private view VP needs sixteen numbers".into());
    }
    let mut matrix = [0.0; 16];
    for (index, value) in values.iter().enumerate() {
        matrix[index] = match resolve(doc, value)? {
            Object::Real(v) => f64::from(*v),
            Object::Integer(v) => *v as f64,
            _ => return Err("private view VP is not numeric".into()),
        };
    }
    if !matrix.iter().all(|v| v.is_finite()) {
        return Err("private view VP is nonfinite".into());
    }
    Ok(Some(MarkedViewMatrix {
        matrix,
        units: text(doc, dict, b"UNITS")?,
        precision: dict
            .get(b"PRECISION")
            .ok()
            .map(|v| resolve(doc, v).and_then(|v| v.as_i64().map_err(|e| e.to_string())))
            .transpose()?,
    }))
}

fn resolve<'a>(doc: &'a Document, value: &'a Object) -> Result<&'a Object, String> {
    doc.dereference(value)
        .map(|(_, value)| value)
        .map_err(|e| e.to_string())
}

fn text(doc: &Document, dict: &Dictionary, key: &[u8]) -> Result<Option<String>, String> {
    let Ok(value) = dict.get(key) else {
        return Ok(None);
    };
    let value = resolve(doc, value)?;
    let text = match value.as_str() {
        // Revit's private metadata in the original Lavington export is UTF-16LE
        // with an FF FE BOM, unlike the standard PDF UTF-16BE convention.
        Ok(bytes) if bytes.starts_with(&[0xff, 0xfe]) => {
            if bytes.len() % 2 != 0 {
                return Err("UTF-16LE metadata has an odd byte length".into());
            }
            let units = bytes[2..]
                .chunks_exact(2)
                .map(|v| u16::from_le_bytes([v[0], v[1]]))
                .collect::<Vec<_>>();
            String::from_utf16(&units)
                .map_err(|e| e.to_string())?
                .trim_end_matches('\0')
                .to_owned()
        }
        _ => decode_text_string(value).map_err(|e| e.to_string())?,
    };
    if text.len() > MAX_OBSERVATION_TEXT_BYTES {
        return Err("metadata text exceeds extraction limit".into());
    }
    Ok((!text.trim().is_empty()).then_some(text))
}

fn bounds(
    doc: &Document,
    dict: &Dictionary,
    transform: &impl Fn(f64, f64) -> (f64, f64),
) -> Result<Option<[f64; 4]>, String> {
    let Ok(rect) = dict.get(b"Rect") else {
        return Ok(None);
    };
    let values = resolve(doc, rect)?.as_array().map_err(|e| e.to_string())?;
    if values.len() != 4 {
        return Err("annotation Rect needs four coordinates".into());
    }
    let mut coordinates = [0.0; 4];
    for (index, value) in values.iter().enumerate() {
        coordinates[index] = match resolve(doc, value)? {
            Object::Integer(value) => *value as f64,
            Object::Real(value) => f64::from(*value),
            _ => return Err("annotation Rect is not numeric".into()),
        };
    }
    let [x0, y0, x1, y1] = coordinates;
    let corners = [
        transform(x0, y0),
        transform(x1, y0),
        transform(x1, y1),
        transform(x0, y1),
    ];
    if corners
        .iter()
        .any(|(x, y)| !x.is_finite() || !y.is_finite())
    {
        return Err("annotation Rect is nonfinite".into());
    }
    Ok(Some([
        corners.iter().map(|v| v.0).fold(f64::INFINITY, f64::min),
        corners.iter().map(|v| v.1).fold(f64::INFINITY, f64::min),
        corners
            .iter()
            .map(|v| v.0)
            .fold(f64::NEG_INFINITY, f64::max),
        corners
            .iter()
            .map(|v| v.1)
            .fold(f64::NEG_INFINITY, f64::max),
    ]))
}

fn annotation(
    doc: &Document,
    entry: &Object,
    index: usize,
    transform: &impl Fn(f64, f64) -> (f64, f64),
) -> Result<AnnotationObservation, String> {
    let object_ref = entry
        .as_reference()
        .ok()
        .map(|id| format!("{} {} R", id.0, id.1));
    let dict = resolve(doc, entry)?.as_dict().map_err(|e| e.to_string())?;
    let subtype = resolve(doc, dict.get(b"Subtype").map_err(|e| e.to_string())?)?
        .as_name()
        .map_err(|e| e.to_string())?;
    let subtype = String::from_utf8(subtype.to_vec()).map_err(|e| e.to_string())?;
    let subject = text(doc, dict, b"Subj")?;
    // AutoCAD's SHX export convention is identified by its explicit subject;
    // arbitrary text annotations remain user markups even on AutoCAD pages.
    let shx = subtype == "Text"
        && subject
            .as_deref()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("AutoCAD SHX Text"));
    Ok(AnnotationObservation {
        index,
        object_ref,
        subtype,
        provenance: if shx { "shx-comment" } else { "pdf-annotation" }.into(),
        subject,
        author: text(doc, dict, b"T")?,
        contents: text(doc, dict, b"Contents")?.unwrap_or_default(),
        bbox: bounds(doc, dict, transform)?,
        has_measure: dict.has(b"Measure"),
    })
}

/// Read only this page's annotation array; failures stay visible beside usable observations.
pub fn read_page_observations(
    doc: &Document,
    page_id: ObjectId,
    transform: impl Fn(f64, f64) -> (f64, f64),
) -> PageObservations {
    let mut output = PageObservations::default();
    if let Ok(page) = doc.get_dictionary(page_id) {
        for key in [
            b"revit_metadata_view_type".as_slice(),
            b"revit_metadata_view_sheet_number",
            b"revit_metadata_view_name",
        ] {
            match text(doc, page, key) {
                Ok(Some(value)) => {
                    output
                        .export_metadata
                        .insert(String::from_utf8_lossy(key).into_owned(), value);
                }
                Ok(None) => {}
                Err(error) => output.errors.push(format!("export metadata: {error}")),
            }
        }
    }
    if let Ok(info) = doc.trailer.get(b"Info") {
        match resolve(doc, info).and_then(|v| v.as_dict().map_err(|e| e.to_string())) {
            Ok(dict) => {
                for (key, target) in [
                    (b"Producer".as_slice(), &mut output.producer),
                    (b"Creator".as_slice(), &mut output.creator),
                ] {
                    match text(doc, dict, key) {
                        Ok(value) => *target = value,
                        Err(error) => output.errors.push(format!("document info: {error}")),
                    }
                }
            }
            Err(error) => output.errors.push(format!("document info: {error}")),
        }
    }
    let entries = doc
        .get_dictionary(page_id)
        .map_err(|e| e.to_string())
        .and_then(|dict| {
            let Ok(entries) = dict.get(b"Annots") else {
                return Ok(&[] as &[Object]);
            };
            resolve(doc, entries)?
                .as_array()
                .map(Vec::as_slice)
                .map_err(|e| e.to_string())
        });
    match entries {
        Err(error) => output.errors.push(format!("annotations: {error}")),
        Ok(entries) => {
            output.annotations_total = entries.len();
            output.annotations_checked = entries.len() <= MAX_ANNOTATIONS;
            if !output.annotations_checked {
                output
                    .errors
                    .push("annotation count exceeds extraction limit".into());
            }
            for (index, entry) in entries.iter().take(MAX_ANNOTATIONS).enumerate() {
                match annotation(doc, entry, index, &transform) {
                    Ok(value) => output.annotations.push(value),
                    Err(error) => {
                        output.annotations_checked = false;
                        if output.errors.len() < MAX_ERRORS {
                            output.errors.push(format!("annotation {index}: {error}"));
                        }
                    }
                }
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::{Object, dictionary};

    #[test]
    fn actual_exporter_utf16le_metadata_decodes_without_glyph_code_prefix() {
        let doc = Document::with_version("1.7");
        let dict = lopdf::dictionary! {"Name"=>Object::String(vec![0xff,0xfe,0x41,0,0x31,0,0,0],lopdf::StringFormat::Hexadecimal)};
        assert_eq!(text(&doc, &dict, b"Name").unwrap().as_deref(), Some("A1"));
    }

    #[test]
    fn comments_preserve_authority_scope_rotation_and_errors() {
        let mut doc = Document::with_version("1.7");
        let info = doc.add_object(dictionary! {"Producer"=>Object::string_literal("AutoCAD 2025")});
        doc.trailer.set("Info", info);
        let shx=doc.add_object(dictionary! {"Subtype"=>"Text", "Subj"=>Object::string_literal("AutoCAD SHX Text"),"Contents"=>Object::string_literal("MIN 2000 DEEP"),"Rect"=>vec![10.into(),20.into(),30.into(),40.into()]});
        let markup=doc.add_object(dictionary! {"Subtype"=>"Line", "Contents"=>Object::string_literal("2400 mm"),"Measure"=>dictionary!{},"Rect"=>vec![50.into(),20.into(),60.into(),40.into()]});
        let page=doc.add_object(dictionary! {"Annots"=>vec![Object::Reference(shx),Object::Reference(markup),Object::Integer(7)]});
        let observed = read_page_observations(&doc, page, |x, y| (100.0 - y, x));
        assert_eq!(observed.producer.as_deref(), Some("AutoCAD 2025"));
        assert_eq!(observed.annotations_total, 3);
        assert!(!observed.annotations_checked);
        assert_eq!(observed.annotations[0].provenance, "shx-comment");
        assert_eq!(observed.annotations[0].contents, "MIN 2000 DEEP");
        assert_eq!(observed.annotations[0].bbox, Some([60.0, 10.0, 80.0, 30.0]));
        assert_eq!(observed.annotations[1].provenance, "pdf-annotation");
        assert!(observed.annotations[1].has_measure);
        assert_eq!(observed.errors.len(), 1);
    }
}
