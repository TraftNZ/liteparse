use std::path::PathBuf;

use liteparse::extract::extract_page_text_objects;
use liteparse_geometry::content_paths::capture_page_content;
use liteparse_geometry::geometry::assemble_vector_page;
use liteparse_geometry::text_capture::{
    TextToken, annotate_text_tokens, group_text_tokens, restore_source_font_metadata,
    restore_source_text_positions,
};
use pdfium::Library;

const TEXT_POINT_TOLERANCE: f64 = 0.02;

#[test]
fn rotated_dingbat_paint_retains_its_encoded_inline_space() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let tokens = capture_tokens(&root.join("rotated_dingbat_space.pdf"), 1);
    let spans = group_text_tokens(&tokens);
    assert_eq!(spans.len(), 2);
    for span in &spans {
        assert_eq!(span.text, "★✡✭✢✵✲✧ ✐✑✒✓");
    }
    assert_eq!(spans[0].rotation, 0.0);
    assert_eq!(spans[1].rotation, -90.0);
}

#[test]
fn declared_plain_and_subset_fonts_keep_their_distinct_names() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("source_font_identity.geometry.json")).unwrap(),
    )
    .unwrap();
    let tokens = capture_tokens(&root.join("source_font_identity.pdf"), 1);
    assert_spans(
        "source font identity",
        &group_text_tokens(&tokens),
        expected[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn rotated_trailing_space_contributes_its_actual_glyph_bounds() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("rotated_trailing_space.geometry.json")).unwrap(),
    )
    .unwrap();
    let tokens = capture_tokens(&root.join("rotated_trailing_space.pdf"), 1);
    assert_spans(
        "rotated trailing space",
        &group_text_tokens(&tokens),
        expected[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn suppressed_offset_word_paints_keep_their_source_positions() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("suppressed_offset_text_paint.geometry.json")).unwrap(),
    )
    .unwrap();
    let tokens = capture_tokens(&root.join("suppressed_offset_text_paint.pdf"), 1);
    let spans = group_text_tokens(&tokens);
    assert_eq!(spans.len(), 3);
    assert_spans(
        "suppressed offset word paint",
        &spans,
        expected[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn geometry_retains_invisible_and_small_text_while_reading_filters_them() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = root.join("painted_text_policy.pdf");
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("painted_text_policy.geometry.json")).unwrap(),
    )
    .unwrap();
    let tokens = capture_tokens(&path, 1);
    assert_spans(
        "painted text policy",
        &group_text_tokens(&tokens),
        expected[0]["textSpans"].as_array().unwrap(),
    );
    let library = Library::try_init().unwrap();
    let document = library.load_document(path.to_str().unwrap(), None).unwrap();
    let page = document.page(0).unwrap();
    let text = page.text().unwrap();
    let items = liteparse::extract::extract_page_text_items(
        &page,
        &text,
        &page.view_box().unwrap(),
        None,
        false,
        true,
    )
    .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(
        items[0].text,
        "Visible text keeps the page dominated by readable characters."
    );
}

#[test]
fn pattern_clip_paths_preserve_paint_order_and_provenance() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let actual = capture_page_content(&root.join("pattern_clip_path.pdf"), 1).unwrap();
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("pattern_clip_path.geometry.json")).unwrap(),
    )
    .unwrap();
    assert_path_capture(&actual.paths, &expected[0]);
}

#[test]
fn pattern_fill_identity_survives_a_subsequent_stroke() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let actual = capture_page_content(&root.join("pattern_fill_then_stroke.pdf"), 1).unwrap();
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("pattern_fill_then_stroke.geometry.json")).unwrap(),
    )
    .unwrap();
    assert_path_capture(&actual.paths, &expected[0]);
}

#[test]
fn masked_images_preserve_reference_capture_clip_transitions() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let actual = capture_page_content(&root.join("soft_mask_clip_state.pdf"), 1).unwrap();
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("soft_mask_clip_state.geometry.json")).unwrap(),
    )
    .unwrap();
    assert_path_capture(&actual.paths, &expected[0]);
}

#[test]
fn named_color_spaces_preserve_declared_identity_and_raw_components() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let actual = capture_page_content(&root.join("named_color_spaces.pdf"), 1).unwrap();
    let expected: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("named_color_spaces.geometry.json")).unwrap(),
    )
    .unwrap();
    assert_path_capture(&actual.paths, &expected[0]);
}

fn assert_path_capture(
    actual: &liteparse_geometry::path_capture::PathCapture,
    expected: &serde_json::Value,
) {
    use liteparse_geometry::model::{Path, Polyline, Segment};
    assert_eq!(
        actual.path_ops,
        expected["pathOps"].as_u64().unwrap() as usize
    );
    let paths: Vec<Path> = serde_json::from_value(expected["paths"].clone()).unwrap();
    let polylines: Vec<Polyline> = serde_json::from_value(expected["polylines"].clone()).unwrap();
    let segments: Vec<Segment> = serde_json::from_value(expected["segments"].clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&actual.paths).unwrap(),
        serde_json::to_value(paths).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&actual.polylines).unwrap(),
        serde_json::to_value(polylines).unwrap()
    );
    assert_eq!(
        serde_json::to_value(&actual.segments).unwrap(),
        serde_json::to_value(segments).unwrap()
    );
}

#[test]
#[ignore = "requires the real Farrelly PDF and saved geometry baseline"]
fn source_line_moves_explain_native_origin_drift() {
    use lopdf::{Document, Object, content::Content};

    const ORIGIN_MATCH_TOLERANCE_PTS: f32 = 0.001;

    fn number(value: &Object) -> f32 {
        match value {
            Object::Integer(value) => *value as f32,
            Object::Real(value) => *value,
            _ => panic!("text positioning operand is not numeric"),
        }
    }

    let path = PathBuf::from(std::env::var_os("FARRELLY_PDF").expect("Farrelly PDF"));
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("parity dir"));
    let source = Document::load(&path).unwrap();
    let page_id = source.get_pages()[&1];
    let content = Content::decode(&source.get_page_content(page_id).unwrap()).unwrap();
    let mut matrix = [1.0_f32, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut native_matrix = matrix;
    let mut native_offset = [0.0_f32; 2];
    let mut moves = 0;
    let mut origin = None;
    for operation in content.operations {
        match operation.operator.as_str() {
            "BT" => {
                matrix = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
                native_matrix = matrix;
                native_offset = [0.0; 2];
                moves = 0;
            }
            "Tm" => {
                matrix = operation
                    .operands
                    .iter()
                    .map(number)
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap();
                native_matrix = matrix;
                native_offset = [0.0; 2];
                moves = 0;
            }
            "Td" | "TD" => {
                let tx = number(&operation.operands[0]);
                let ty = number(&operation.operands[1]);
                matrix[4] += tx * matrix[0] + ty * matrix[2];
                matrix[5] += tx * matrix[1] + ty * matrix[3];
                native_offset[0] += tx;
                native_offset[1] += ty;
                moves += 1;
            }
            "TJ" => {
                let bytes: Vec<_> = operation.operands[0]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|value| value.as_str().ok())
                    .flatten()
                    .copied()
                    .collect();
                if bytes.starts_with(b"SPECIFICATION AND OTHER APPLICABLE") {
                    origin = Some((matrix, native_matrix, native_offset, moves));
                    break;
                }
            }
            _ => {}
        }
    }
    let (matrix, native_matrix, native_offset, moves) =
        origin.expect("specification label source operation");
    assert_eq!(moves, 8, "source line moves before affected label");
    let library = Library::try_init().unwrap();
    let document = library.load_document(path.to_str().unwrap(), None).unwrap();
    let page = document.page(0).unwrap();
    let viewport = page.viewport_transform(&page.view_box().unwrap());
    let source_origin = viewport.transform_point(matrix[4], matrix[5]);
    let predicted_native_origin = viewport.transform_point(
        native_offset[0] * native_matrix[0]
            + native_offset[1] * native_matrix[2]
            + native_matrix[4],
        native_offset[0] * native_matrix[1]
            + native_offset[1] * native_matrix[3]
            + native_matrix[5],
    );
    let reference: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("farrelly-p1.full.json")).unwrap()).unwrap();
    let label = reference[0]["textSpans"]
        .as_array()
        .unwrap()
        .iter()
        .find(|span| {
            span["text"]
                .as_str()
                .is_some_and(|text| text.starts_with("SPECIFICATION AND OTHER APPLICABLE"))
        })
        .expect("reference specification label");
    assert_eq!(f64::from(source_origin.0), label["x0"].as_f64().unwrap());
    let text = page.text().unwrap();
    let native_origin = text
        .chars()
        .find_map(|glyph| {
            let (x, y) = glyph.origin()?;
            let point = viewport.transform_point(x as f32, y as f32);
            (glyph.unicode() == u32::from('S')
                && (point.0 - source_origin.0).abs() < ORIGIN_MATCH_TOLERANCE_PTS
                && (point.1 - source_origin.1).abs() < ORIGIN_MATCH_TOLERANCE_PTS)
                .then_some(point)
        })
        .expect("native first specification glyph");
    assert_eq!(native_origin, predicted_native_origin);
    assert_ne!(native_origin.0, source_origin.0);
    drop(text);
    drop(page);
    drop(document);
    drop(library);
    let corrected = capture_tokens(&path, 1);
    let label = corrected
        .iter()
        .find(|token| token.text == "SPECIFICATION")
        .expect("corrected specification label");
    assert_eq!(
        label.baseline_start,
        Some([f64::from(source_origin.0), f64::from(source_origin.1)])
    );
}

#[test]
#[ignore = "requires the selected real PDFs and a writable parity directory"]
fn export_real_page_raw_text_observations() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("parity dir"));
    let mut cases = vec![
        ("BSG_PDF", "bsg-p1", 0),
        ("FARRELLY_PDF", "farrelly-p1", 0),
        ("MANGAWHAI_PDF", "mangawhai-p12", 11),
    ];
    if std::env::var_os("REPORT_PDF").is_some() {
        let number: u32 = std::env::var("REPORT_PAGE_NUMBER")
            .expect("report page number")
            .parse()
            .expect("positive report page number");
        let index = number
            .checked_sub(1)
            .expect("positive report page number")
            .try_into()
            .expect("report page index");
        cases.push(("REPORT_PDF", "report-page", index));
    }
    for (source_key, name, page_index) in cases {
        if std::env::var("GEOMETRY_TEXT_CASE").is_ok_and(|requested| requested != name) {
            continue;
        }
        let path = PathBuf::from(std::env::var_os(source_key).expect("real PDF path"));
        let library = Library::try_init().unwrap();
        let document = library.load_document(path.to_str().unwrap(), None).unwrap();
        let page = document.page(page_index).unwrap();
        let view = page.view_box().unwrap();
        let text = page.text().unwrap();
        let viewport = page.viewport_transform(&view);
        let object_indices: std::collections::HashMap<_, _> = (0..page.object_count())
            .filter_map(|index| page.object(index).map(|object| (object.id(), index)))
            .collect();
        let glyphs: Vec<_> = text
            .chars()
            .enumerate()
            .map(|(index, glyph)| {
                let origin_page = glyph.origin();
                let origin_affine =
                    origin_page.map(|(x, y)| viewport.transform_point(x as f32, y as f32));
                let origin = glyph
                    .origin()
                    .map(|(x, y)| page.page_to_viewport(&view, x as f32, y as f32));
                let bounds = glyph
                    .loose_char_box()
                    .map(|bounds| page.viewport_transform(&view).transform_bounds(&bounds));
                let matrix = glyph
                    .matrix()
                    .map(|matrix| [matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f]);
                serde_json::json!({"index":index,"character":char::from_u32(glyph.unicode()),
            "char_code":glyph.char_code(),"source_object":glyph.text_object().and_then(|object|
                object_indices.get(&(object as usize)).copied()),"origin":origin,
            "origin_page":origin_page,"origin_affine":origin_affine,
            "bounds":bounds.map(|bounds| [bounds.left,bounds.top,bounds.right,bounds.bottom]),
            "char_bounds":glyph.char_box().map(|bounds| [bounds.left,bounds.bottom,bounds.right,bounds.top]),
            "font":glyph.font_name(),"font_size":glyph.font_size(),"matrix":matrix,
            "angle":glyph.angle(),"generated":glyph.is_generated(),
            "unicode_map_error":glyph.has_unicode_map_error()})
            })
            .collect();
        let objects: Vec<_> = (0..page.object_count())
            .filter_map(|index| {
                let object = page.object(index)?;
                (object.kind() == pdfium::PageObjectKind::Text).then(|| {
                    let matrix = object
                        .matrix()
                        .map(|matrix| [matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f]);
                    let bounds = object
                        .bounds()
                        .map(|bounds| page.viewport_transform(&view).transform_bounds(&bounds));
                    serde_json::json!({"index":index,"matrix":matrix,
                "bounds":bounds.map(|bounds| [bounds.left,bounds.top,bounds.right,bounds.bottom])})
                })
            })
            .collect();
        assert!(!glyphs.is_empty() && !objects.is_empty());
        let source = lopdf::Document::load(&path).expect("source PDF");
        let content = liteparse_geometry::content_paths::capture_loaded_page_content(
            &source,
            &page,
            page_index as u32 + 1,
        )
        .expect("source text paints");
        let source_paints: Vec<_> = content
            .text_paints
            .iter()
            .map(|paint| format!("{paint:?}"))
            .collect();
        std::fs::write(
            dir.join(format!("{name}.raw-text.json")),
            serde_json::to_vec(&serde_json::json!({"page":page_index+1,"source_pdf":path,"glyphs":glyphs,"objects":objects,"source_paints":source_paints})).unwrap(),
        )
        .unwrap();
    }
}

fn capture_tokens(path: &std::path::Path, page_number: u32) -> Vec<TextToken> {
    let library = Library::try_init().expect("PDFium");
    let document = library
        .load_document(path.to_str().expect("UTF-8 path"), None)
        .expect("open document");
    let page = document
        .page(page_number as i32 - 1)
        .expect("requested page");
    let view = page.view_box().expect("view box");
    let text = page.text().expect("text page");
    let items = extract_page_text_objects(&page, &text, &view, None).expect("native text objects");
    let mut tokens: Vec<TextToken> = items
        .iter()
        .map(|item| {
            serde_json::from_value(serde_json::to_value(item).expect("encode item"))
                .expect("decode token")
        })
        .collect();
    let source = lopdf::Document::load(path).expect("source PDF");
    let content =
        liteparse_geometry::content_paths::capture_loaded_page_content(&source, &page, page_number)
            .expect("source page content");
    let characters = liteparse::extract::recover_page_glyph_characters(&text);
    annotate_text_tokens(&page, &text, &view, &content, &characters, &mut tokens);
    restore_source_text_positions(&page, &text, &content, &characters, &mut tokens)
        .unwrap_or_else(|error| panic!("source text alignment: {error}; tokens: {tokens:#?}"));
    restore_source_font_metadata(path, &mut tokens).expect("source font names");
    tokens
}

fn assert_spans(
    basename: &str,
    actual: &[liteparse_geometry::model::TextSpan],
    expected: &[serde_json::Value],
) {
    assert_eq!(actual.len(), expected.len(), "{basename} text span count");
    for (index, (span, previous)) in actual.iter().zip(expected).enumerate() {
        assert_eq!(
            span.text,
            previous["text"].as_str().unwrap(),
            "{basename} text[{index}]"
        );
        assert_eq!(
            span.font_name,
            previous["fontName"].as_str().unwrap(),
            "{basename} font[{index}]"
        );
        for (field, value) in [
            ("x0", span.x0),
            ("y0", span.y0),
            ("x1", span.x1),
            ("y1", span.y1),
            ("fontSize", span.font_size),
        ] {
            assert!(
                (value - previous[field].as_f64().unwrap()).abs() < TEXT_POINT_TOLERANCE,
                "{basename} text[{index}].{field}: {value} != {}",
                previous[field]
            );
        }
        let expected_rotation = previous["rotation"].as_f64().unwrap_or(0.0);
        assert!(
            (span.rotation - expected_rotation).abs() < TEXT_POINT_TOLERANCE,
            "{basename} text[{index}].rotation: {} != {expected_rotation}",
            span.rotation
        );
    }
}

#[test]
#[ignore = "requires the real Mangawhai PDF and saved text baseline"]
fn marker_lines_and_horizontal_labels_match_reference_text_and_bounds() {
    const PAGE_NUMBER: u32 = 12;
    const LABELS: [&str; 4] = [
        "TRIM ISLAND TO SUIT",
        "PARKING ORIENTATION",
        "CLEAN WATER RUNOFF",
        "DIVERSION BUND (REFER DETAIL ON SHT 108)",
    ];
    let path = PathBuf::from(std::env::var_os("MANGAWHAI_PDF").expect("real PDF path"));
    let dir =
        PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("geometry parity dir"));
    let baseline: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("mangawhai-p12.text.json")).expect("baseline"),
    )
    .unwrap();
    let expected: Vec<_> = baseline[0]["textSpans"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|span| {
            let text = span["text"].as_str().unwrap();
            text.starts_with('>') || LABELS.contains(&text)
        })
        .cloned()
        .collect();
    let actual: Vec<_> = group_text_tokens(&capture_tokens(&path, PAGE_NUMBER))
        .into_iter()
        .filter(|span| span.text.starts_with('>') || LABELS.contains(&span.text.as_str()))
        .collect();
    assert!(!expected.is_empty());
    assert_spans("Mangawhai markers and labels", &actual, &expected);
}

#[test]
#[ignore = "requires the real Mangawhai PDF"]
fn overprinted_glyphs_keep_their_own_font_size_and_paint_order() {
    const PAGE_NUMBER: u32 = 12;
    const OVERPRINT_BOX: [f64; 4] = [810.0, 310.0, 819.0, 316.0];
    const EXPECTED_TOKEN_COUNT: usize = 6;
    const EXPECTED_PAINT_OBJECT_COUNT: usize = 6;
    const FONT_SIZE_TOLERANCE: f64 = 0.001;
    let path = PathBuf::from(std::env::var_os("MANGAWHAI_PDF").expect("real PDF path"));
    let tokens = capture_tokens(&path, PAGE_NUMBER);
    let overprints: Vec<_> = tokens
        .iter()
        .filter(|token| {
            token.text == "P"
                && token.x > OVERPRINT_BOX[0]
                && token.y > OVERPRINT_BOX[1]
                && token.x < OVERPRINT_BOX[2]
                && token.y < OVERPRINT_BOX[3]
        })
        .collect();
    assert_eq!(overprints.len(), EXPECTED_TOKEN_COUNT);
    let mut orders = std::collections::HashSet::new();
    for token in overprints {
        assert!(
            (token.visual_font_size.expect("glyph size")
                - token.font_height.expect("native item size"))
            .abs()
                < FONT_SIZE_TOLERANCE,
            "overprint matched another glyph's size: {token:?}"
        );
        orders.insert(token.source_order.expect("paint order"));
    }
    assert_eq!(orders.len(), EXPECTED_PAINT_OBJECT_COUNT);
}

#[test]
#[ignore = "requires real PDF geometry parity corpus"]
fn native_text_spans_match_geometry_baselines() {
    let dir =
        PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("geometry parity dir"));
    for (key, basename, page_number, artifact_kind) in [
        ("BSG_PDF", "bsg-p1", 1, "full"),
        ("FARRELLY_PDF", "farrelly-p1", 1, "full"),
        ("MANGAWHAI_PDF", "mangawhai-p12", 12, "text"),
    ] {
        if std::env::var("GEOMETRY_TEXT_CASE").is_ok_and(|requested| requested != basename) {
            continue;
        }
        let path = PathBuf::from(std::env::var_os(key).expect("real PDF path"));
        let baseline: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{basename}.{artifact_kind}.json")))
                .expect("read baseline"),
        )
        .expect("decode baseline");
        let expected = baseline[0]["textSpans"].as_array().expect("baseline spans");
        let tokens = capture_tokens(&path, page_number);
        let actual = group_text_tokens(&tokens);
        std::fs::write(
            dir.join(format!("{basename}.lit-spans.json")),
            serde_json::to_vec(&actual).unwrap(),
        )
        .expect("save candidate spans");
        std::fs::write(
            dir.join(format!("{basename}.lit-tokens.json")),
            serde_json::to_vec(&tokens).unwrap(),
        )
        .expect("save candidate tokens");
        assert_spans(basename, &actual, expected);
        if basename == "farrelly-p1" {
            let content = capture_page_content(&path, 1).expect("source page content");
            let page_geometry = assemble_vector_page(1, content, actual);
            let previous = &baseline[0];
            assert_eq!(
                page_geometry.version,
                previous["version"].as_u64().unwrap() as u32
            );
            assert_eq!(page_geometry.class, previous["class"].as_str().unwrap());
            assert_eq!(page_geometry.source, previous["source"].as_str().unwrap());
            assert_eq!(
                page_geometry.path_ops,
                previous["pathOps"].as_u64().unwrap() as usize
            );
            assert_eq!(
                page_geometry.image_ops,
                previous["imageOps"].as_u64().unwrap() as usize
            );
            assert_eq!(
                page_geometry.segments.as_ref().unwrap().len(),
                previous["segments"].as_array().unwrap().len()
            );
            assert_eq!(
                page_geometry.paths.as_ref().unwrap().len(),
                previous["paths"].as_array().unwrap().len()
            );
            assert_eq!(
                page_geometry.text_spans.as_ref().unwrap().len(),
                expected.len()
            );
            for (field, value) in [
                ("widthPts", page_geometry.width_pts),
                ("heightPts", page_geometry.height_pts),
                ("imageCoverage", page_geometry.image_coverage),
            ] {
                assert!(
                    (value - previous[field].as_f64().unwrap()).abs() < TEXT_POINT_TOLERANCE,
                    "{field}: {value} != {}",
                    previous[field]
                );
            }
        }
    }
}

#[test]
#[ignore = "requires saved synthetic PDF geometry corpus"]
fn synthetic_text_spans_match_geometry_baselines() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_PARITY_DIR").expect("synthetic parity dir"));
    for fixture in [
        "styled",
        "layered-curve",
        "resource-identity",
        "combined-gap",
        "form-oc-only",
        "transparency",
        "tile-positive",
    ] {
        let baseline: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.full.json"))).expect("baseline"),
        )
        .unwrap();
        let expected = baseline[0]["textSpans"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let actual = group_text_tokens(&capture_tokens(&dir.join(format!("{fixture}.pdf")), 1));
        assert_spans(fixture, &actual, &expected);
    }
}

#[test]
#[ignore = "requires the exported standard-font PDF geometry corpus"]
fn base14_text_spans_match_geometry_baselines() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_BASE14_PARITY_DIR").expect("base14 corpus"));
    for font in [
        "Helvetica",
        "Helvetica-Bold",
        "Helvetica-Oblique",
        "Helvetica-BoldOblique",
        "Times-Roman",
        "Times-Bold",
        "Times-Italic",
        "Times-BoldItalic",
        "Courier",
        "Courier-Bold",
        "Courier-Oblique",
        "Courier-BoldOblique",
        "Symbol",
        "ZapfDingbats",
    ] {
        let baseline: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join(format!("{font}.json"))).unwrap())
                .unwrap();
        let actual = group_text_tokens(&capture_tokens(&dir.join(format!("{font}.pdf")), 1));
        assert_spans(font, &actual, baseline["textSpans"].as_array().unwrap());
    }
}

#[test]
fn vertical_text_and_following_horizontal_cursor_match_reference() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/vertical_cursor.geometry.json")).unwrap();
    let tokens = capture_tokens(&dir.join("vertical_cursor.pdf"), 1);
    let actual = group_text_tokens(&tokens);
    assert_spans(
        "vertical-cursor",
        &actual,
        baseline[0]["textSpans"]
            .as_array()
            .expect("reference spans"),
    );
}

#[test]
#[ignore = "requires a real report PDF and complete saved reference"]
fn report_text_spans_match_saved_baseline() {
    let path = PathBuf::from(std::env::var_os("REPORT_PDF").expect("report PDF"));
    let baseline_path = PathBuf::from(
        std::env::var_os("REPORT_GEOMETRY_REFERENCE").expect("complete report reference"),
    );
    let page_number: u32 = std::env::var("REPORT_PAGE_NUMBER")
        .expect("report page number")
        .parse()
        .expect("positive page number");
    let baseline: serde_json::Value =
        serde_json::from_slice(&std::fs::read(baseline_path).expect("saved reference"))
            .expect("reference JSON");
    let expected = baseline
        .as_array()
        .expect("page array")
        .iter()
        .find(|page| page["page"].as_u64() == Some(u64::from(page_number)))
        .expect("requested reference page");
    let tokens = capture_tokens(&path, page_number);
    let actual = group_text_tokens(&tokens);
    if let Some(output) = std::env::var_os("REPORT_TEXT_OBSERVATION_OUT") {
        std::fs::write(
            output,
            serde_json::to_vec(
                &serde_json::json!({"page":page_number,"tokens":tokens,"spans":actual}),
            )
            .expect("encode text observation"),
        )
        .expect("save text observation");
    }
    assert_spans(
        "report",
        &actual,
        expected["textSpans"]
            .as_array()
            .expect("reference text spans"),
    );
}

#[test]
fn overprinted_scaled_glyphs_keep_available_native_matches() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let tokens = capture_tokens(&dir.join("overprinted_scaled_glyphs.pdf"), 1);
    const EXPECTED_PAINT_COUNT: usize = 7;
    assert_eq!(tokens.len(), EXPECTED_PAINT_COUNT);
    let orders: std::collections::HashSet<_> = tokens
        .iter()
        .map(|token| token.source_order.expect("matched paint operation"))
        .collect();
    assert_eq!(orders.len(), EXPECTED_PAINT_COUNT, "{tokens:#?}");
    let native_indices: Vec<_> = tokens
        .iter()
        .flat_map(|token| &token.glyph_frames)
        .filter_map(|frame| frame.native_index)
        .collect();
    let unique_indices: std::collections::HashSet<_> = native_indices.iter().copied().collect();
    assert_eq!(
        native_indices.len(),
        unique_indices.len(),
        "each retained native glyph belongs to one paint token"
    );
    let library = Library::try_init().unwrap();
    let document = library
        .load_document(
            dir.join("overprinted_scaled_glyphs.pdf").to_str().unwrap(),
            None,
        )
        .unwrap();
    let page = document.page(0).unwrap();
    let text = page.text().unwrap();
    let expected_native: std::collections::HashSet<_> = text
        .chars()
        .enumerate()
        .filter(|(_, glyph)| !glyph.is_generated())
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        unique_indices, expected_native,
        "every available native glyph is retained"
    );
    assert!(
        tokens
            .iter()
            .all(|token| token.visual_font_size.is_some() && !token.glyph_frames.is_empty())
    );
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/overprinted_scaled_glyphs.geometry.json"
    ))
    .unwrap();
    assert_spans(
        "overprinted-scaled-glyphs",
        &group_text_tokens(&tokens),
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn painted_punctuation_matches_reference_while_reading_items_fold_to_ascii() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = dir.join("typographic_punctuation.pdf");
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/typographic_punctuation.geometry.json"
    ))
    .expect("reference JSON");
    let actual = group_text_tokens(&capture_tokens(&path, 1));
    assert_spans(
        "typographic punctuation",
        &actual,
        baseline[0]["textSpans"]
            .as_array()
            .expect("reference spans"),
    );
    let library = Library::try_init().expect("PDFium");
    let document = library
        .load_document(path.to_str().expect("UTF-8 path"), None)
        .expect("fixture");
    let page = document.page(0).expect("fixture page");
    let text = page.text().expect("fixture text");
    let view = page.view_box().expect("fixture view");
    let reading =
        liteparse::extract::extract_page_text_items(&page, &text, &view, None, false, true)
            .expect("reading items");
    let normalized: String = reading
        .iter()
        .flat_map(|item| item.text.chars())
        .filter(|character| !character.is_whitespace())
        .collect();
    assert_eq!(normalized, "''--");
}

#[test]
fn painted_unmapped_controls_keep_replacement_characters() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.join("unmapped_control.geometry.json")).unwrap(),
    )
    .unwrap();
    let spans = group_text_tokens(&capture_tokens(&directory.join("unmapped_control.pdf"), 1));
    assert_eq!(spans[0].text, "Ac\u{fffd}ve");
    assert_spans(
        "unmapped-control",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn rotated_unmapped_characters_keep_their_native_glyph_indices() {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rotated_unmapped_codes.pdf");
    let tokens = capture_tokens(&path, 1);
    assert_eq!(tokens.len(), 3);
    for (token, (code, index)) in tokens.iter().zip([(27, 0), (8, 1), (38, 4)]) {
        assert_eq!(token.text, "\u{fffd}");
        assert_eq!(token.char_codes, [code]);
        assert_eq!(token.glyph_frames.len(), 1);
        assert_eq!(token.glyph_frames[0].native_index, Some(index));
    }
    assert_ne!(tokens[0].source_order, tokens[1].source_order);
}

#[test]
fn painted_offpage_text_matches_reference_while_reading_items_clip_it() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = directory.join("offpage_text.pdf");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/offpage_text.geometry.json")).unwrap();
    let spans = group_text_tokens(&capture_tokens(&path, 1));
    assert_spans(
        "offpage-text",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
    let library = Library::try_init().expect("PDFium");
    let document = library.load_document(path.to_str().unwrap(), None).unwrap();
    let page = document.page(0).unwrap();
    let text = page.text().unwrap();
    let reading = liteparse::extract::extract_page_text_items(
        &page,
        &text,
        &page.view_box().unwrap(),
        None,
        false,
        false,
    )
    .unwrap();
    assert!(reading.is_empty());
}

#[test]
fn stretched_text_keeps_its_paint_among_overlapping_rotated_glyphs() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/stretched_text_overlap.geometry.json"
    ))
    .unwrap();
    let tokens = capture_tokens(&directory.join("stretched_text_overlap.pdf"), 1);
    assert_eq!(tokens[0].glyph_frames.len(), 11);
    let spans = group_text_tokens(&tokens);
    assert_spans(
        "stretched-text-overlap",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn omitted_control_paint_uses_observed_font_character_and_source_frame() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/suppressed_control_paint.geometry.json"
    ))
    .unwrap();
    let tokens = capture_tokens(&directory.join("suppressed_control_paint.pdf"), 1);
    let recovered: Vec<_> = tokens
        .iter()
        .filter(|token| {
            token.char_codes == [20]
                && token
                    .glyph_frames
                    .iter()
                    .all(|frame| frame.native_index.is_none())
        })
        .collect();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].text, "\u{fffd}");
    let spans = group_text_tokens(&tokens);
    assert_spans(
        "suppressed-control-paint",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn consecutive_identical_glyph_paints_do_not_duplicate_text() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/consecutive_identical_glyph.geometry.json"
    ))
    .unwrap();
    let tokens = capture_tokens(&directory.join("consecutive_identical_glyph.pdf"), 1);
    let spans = group_text_tokens(&tokens);
    assert_eq!(spans.len(), 5);
    assert_spans(
        "consecutive-identical-glyph",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn recovered_trailing_space_keeps_its_separate_line_bounds() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/recovered_trailing_space_gap.geometry.json"
    ))
    .unwrap();
    let tokens = capture_tokens(&directory.join("recovered_trailing_space_gap.pdf"), 1);
    let recovered = tokens
        .iter()
        .find(|token| token.text.starts_with("Page | i"))
        .unwrap();
    assert_eq!(recovered.text, "Page | i  ");
    assert_eq!(recovered.text.chars().count(), recovered.glyph_frames.len());
    let spans = group_text_tokens(&tokens);
    assert_spans(
        "recovered-space-gap",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn glyph_gaps_within_a_native_token_keep_table_columns_separate() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/internal_glyph_gap.geometry.json")).unwrap();
    let tokens = capture_tokens(&directory.join("internal_glyph_gap.pdf"), 1);
    let spans = group_text_tokens(&tokens);
    assert_eq!(spans.len(), 8);
    assert_spans(
        "internal-glyph-gap",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn trailing_real_space_preserves_bounds_when_kerning_overlaps_previous_glyph() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/trailing_space_overlap.geometry.json"
    ))
    .unwrap();
    let tokens = capture_tokens(&directory.join("trailing_space_overlap.pdf"), 1);
    assert_eq!(tokens[0].text, "\u{fffd}\u{fffd}\u{fffd} ");
    assert_eq!(tokens[0].char_codes, [18, 21, 31, 32]);
    assert_eq!(tokens[0].glyph_frames.len(), 4);
    let spans = group_text_tokens(&tokens);
    assert_spans(
        "trailing-space-overlap",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn unmapped_glyph_gap_does_not_infer_a_word_space() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/unmapped_glyph_gap.geometry.json")).unwrap();
    let tokens = capture_tokens(&directory.join("unmapped_glyph_gap.pdf"), 1);
    assert_eq!(tokens.len(), 10);
    let spans = group_text_tokens(&tokens);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].text, "\u{fffd}".repeat(10));
    assert_spans(
        "unmapped-glyph-gap",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn generated_space_does_not_contribute_a_painted_glyph_frame() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/generated_space_bounds.geometry.json"
    ))
    .unwrap();
    let tokens = capture_tokens(&directory.join("generated_space_bounds.pdf"), 1);
    assert_eq!(
        tokens
            .iter()
            .map(|token| token.glyph_frames.len())
            .sum::<usize>(),
        2
    );
    let spans = group_text_tokens(&tokens);
    assert_spans(
        "generated-space-bounds",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn inferred_space_after_superscript_preserves_bounds_and_font_weight() {
    const FONT_SIZE_TOLERANCE: f64 = 0.0001;
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/superscript_space.geometry.json")).unwrap();
    let tokens = capture_tokens(&directory.join("superscript_space.pdf"), 1);
    let spans = group_text_tokens(&tokens);
    assert_spans(
        "superscript-space",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
    assert!(
        (spans[0].font_size - baseline[0]["textSpans"][0]["fontSize"].as_f64().unwrap()).abs()
            < FONT_SIZE_TOLERANCE
    );
}

#[test]
fn partially_suppressed_paints_recover_trailing_and_middle_glyphs() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/partial_paint.geometry.json")).unwrap();
    let tokens = capture_tokens(&directory.join("partial_paint.pdf"), 1);
    let recovered: Vec<_> = tokens
        .iter()
        .filter(|token| {
            token
                .glyph_frames
                .iter()
                .any(|frame| frame.native_index.is_none())
        })
        .collect();
    assert_eq!(recovered.len(), 2);
    assert_eq!(
        recovered[0].text.chars().count(),
        recovered[0].glyph_frames.len()
    );
    assert_eq!(
        recovered[1].text.chars().count(),
        recovered[1].glyph_frames.len()
    );
    let spans = group_text_tokens(&tokens);
    assert_spans(
        "partial-paint",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn kerned_native_word_spaces_preserve_text_and_font_weight() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = directory.join("kerned_interior_space.pdf");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/kerned_interior_space.geometry.json")).unwrap();
    let tokens = capture_tokens(&path, 1);
    let spans = group_text_tokens(&tokens);
    assert_eq!(spans[0].text, "Summary .... 1");
    assert_spans(
        "kerned-interior-space",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
    assert!(
        (spans[0].font_size - baseline[0]["textSpans"][0]["fontSize"].as_f64().unwrap()).abs()
            < 1e-5
    );
}

#[test]
fn to_unicode_space_preserves_its_font_weight_and_bounds() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = directory.join("tounicode_space_paint.pdf");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/tounicode_space_paint.geometry.json")).unwrap();
    let tokens = capture_tokens(&path, 1);
    assert_eq!(tokens.iter().filter(|token| token.text == " ").count(), 1);
    let spans = group_text_tokens(&tokens);
    assert_spans(
        "to-unicode-space",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
    assert!(
        (spans[0].font_size - baseline[0]["textSpans"][0]["fontSize"].as_f64().unwrap()).abs()
            < 1e-5
    );
}

#[test]
fn declared_encoding_recovers_a_space_without_a_native_glyph_or_glyph_name() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = directory.join("encoded_space_paint.pdf");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/encoded_space_paint.geometry.json")).unwrap();
    let tokens = capture_tokens(&path, 1);
    let space = tokens.iter().find(|token| token.text == " ").unwrap();
    assert!(
        space
            .glyph_frames
            .iter()
            .all(|frame| frame.native_index.is_none())
    );
    let spans = group_text_tokens(&tokens);
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].text, "4.2.1 SK 1");
    assert_spans(
        "encoded-space-paint",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn standalone_space_paints_preserve_line_advance_and_bounds() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = directory.join("standalone_space_paint.pdf");
    let baseline: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/standalone_space_paint.geometry.json"
    ))
    .unwrap();
    let tokens = capture_tokens(&path, 1);
    assert_eq!(tokens.iter().filter(|token| token.text == " ").count(), 3);
    let spans = group_text_tokens(&tokens);
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].text, "4.2.1 SK 1");
    assert_spans(
        "standalone-space-paint",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn font_changes_preserve_the_complete_line_and_its_first_font() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = directory.join("mixed_font_line.pdf");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/mixed_font_line.geometry.json")).unwrap();
    let tokens = capture_tokens(&path, 1);
    assert!(
        tokens
            .windows(2)
            .any(|pair| pair[0].font_name != pair[1].font_name)
    );
    let spans = group_text_tokens(&tokens);
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].text, "Mixed-fonts");
    assert_eq!(spans[0].font_name, "Helvetica");
    assert_eq!(spans[1].text, "Next line");
    assert_spans(
        "mixed-font-line",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
}

#[test]
fn supplementary_math_characters_use_one_scalar_per_source_glyph() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let path = directory.join("supplementary_math.pdf");
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/supplementary_math.geometry.json")).unwrap();
    let tokens = capture_tokens(&path, 1);
    assert_eq!(
        tokens
            .iter()
            .map(|token| token.glyph_frames.len())
            .sum::<usize>(),
        5
    );
    let spans = group_text_tokens(&tokens);
    assert_eq!(spans[0].text, "𝑉ℎ𝑜𝑙𝑒");
    assert_spans(
        "supplementary-math",
        &spans,
        baseline[0]["textSpans"].as_array().unwrap(),
    );
    let library = Library::try_init().unwrap();
    let document = library.load_document(path.to_str().unwrap(), None).unwrap();
    let page = document.page(0).unwrap();
    let text = page.text().unwrap();
    assert_eq!(
        text.chars()
            .filter_map(|glyph| glyph.unicode_scalar())
            .collect::<String>(),
        "𝑉ℎ𝑜𝑙𝑒"
    );
    let reading = liteparse::extract::extract_page_text_items(
        &page,
        &text,
        &page.view_box().unwrap(),
        None,
        false,
        false,
    )
    .unwrap();
    assert_eq!(
        reading
            .iter()
            .map(|item| item.text.as_str())
            .collect::<String>(),
        "𝑉ℎ𝑜𝑙𝑒"
    );
}
