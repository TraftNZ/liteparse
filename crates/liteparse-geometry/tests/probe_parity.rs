use std::path::PathBuf;

use liteparse_geometry::{
    content_paths::{capture_page_content, capture_page_paths},
    inspect_page,
    model::PaintStyle,
    path_capture::PathCapture,
    raster::{RasterTrace, trace_raster_segments},
    relations::analyze_page,
};
use pdfium::{BitmapFormat, Library};

fn run_geometry_cli(
    source: &std::path::Path,
    dir: &std::path::Path,
    name: &str,
    options: serde_json::Value,
) -> (serde_json::Value, serde_json::Value) {
    run_geometry_cli_page(source, dir, name, 1, options)
}

fn run_geometry_cli_page(
    source: &std::path::Path,
    dir: &std::path::Path,
    name: &str,
    page_number: u32,
    mut options: serde_json::Value,
) -> (serde_json::Value, serde_json::Value) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let binary = std::env::var_os("GEOMETRY_CLI_BINARY").expect("built lit CLI");
    let artifact = dir.join(format!("{name}-cli-parity.{}.json", std::process::id()));
    options["artifact_out"] = serde_json::json!(artifact);
    let request = serde_json::json!({"version":1,"operation":"geometry","pdf_path":source,
        "page_numbers":[page_number],"geometry":options});
    let mut child = Command::new(binary)
        .arg("pdf")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start lit");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let full = serde_json::from_slice(&std::fs::read(&artifact).expect("full artifact")).unwrap();
    let preview = serde_json::from_slice(&result.stdout).unwrap();
    std::fs::remove_file(artifact).unwrap();
    (full, preview)
}

#[test]
#[ignore = "requires the built lit CLI and saved raster parity corpus"]
fn geometry_cli_matches_complete_raster_artifact_and_preview() {
    let source = PathBuf::from(std::env::var_os("RASTER_PDF").expect("raster PDF"));
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("parity dir"));
    let (full, preview) = run_geometry_cli(
        &source,
        &dir,
        "raster",
        serde_json::json!({"force_raster":true,"raster_dpi":72}),
    );
    for (kind, actual) in [("full", full), ("preview", preview)] {
        let expected: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("raster-p1.{kind}.json"))).expect("baseline"),
        )
        .unwrap();
        if let Some(difference) = first_geometry_difference(kind, &actual, &expected) {
            panic!("{difference}");
        }
    }
}

#[test]
#[ignore = "requires the built lit CLI and saved synthetic parity corpus"]
fn geometry_cli_matches_complete_synthetic_artifacts() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_PARITY_DIR").expect("synthetic corpus"));
    for fixture in [
        "styled",
        "layered-curve",
        "resource-identity",
        "combined-gap",
        "form-oc-only",
        "transparency",
        "tile-positive",
    ] {
        let expected: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.full.json"))).expect("baseline"),
        )
        .unwrap();
        let (actual, _) = run_geometry_cli(
            &dir.join(format!("{fixture}.pdf")),
            &dir,
            fixture,
            serde_json::json!({"no_raster":true,"relations":expected[0]["relationsAnalyzed"].as_bool().unwrap_or(false)}),
        );
        if let Some(difference) = first_geometry_difference(fixture, &actual, &expected) {
            panic!("{difference}");
        }
    }
}

#[test]
#[ignore = "requires the built lit CLI and saved real-page geometry corpus"]
fn geometry_cli_matches_complete_real_artifacts() {
    const PATH_TOLERANCE: f64 = 0.01;
    const TEXT_TOLERANCE: f64 = 0.02;
    const GRAPH_TOLERANCE: f64 = 0.00001;
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("real corpus"));
    let mut matched_cases = 0;
    for (fixture, source_key, page_number, no_raster) in [
        ("bsg-p1", "BSG_PDF", 1, false),
        ("farrelly-p1", "FARRELLY_PDF", 1, true),
        ("mangawhai-p12", "MANGAWHAI_PDF", 12, true),
    ] {
        if std::env::var("GEOMETRY_CLI_CASE").is_ok_and(|name| name != fixture) {
            continue;
        }
        matched_cases += 1;
        let source = PathBuf::from(std::env::var_os(source_key).expect("source PDF"));
        let (actual, preview) = run_geometry_cli_page(
            &source,
            &dir,
            fixture,
            page_number,
            serde_json::json!({"no_raster":no_raster,"relations":true}),
        );
        std::fs::write(
            dir.join(format!("{fixture}.lit-cli.full.json")),
            serde_json::to_vec(&actual).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{fixture}.lit-cli.preview.json")),
            serde_json::to_vec(&preview).unwrap(),
        )
        .unwrap();
        let mut expected: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.full.json"))).expect("full baseline"),
        )
        .unwrap();
        let graph: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.relations.json"))).expect("graph baseline"),
        )
        .unwrap();
        for (field, value) in graph.as_object().unwrap() {
            expected[0][field] = value.clone();
        }
        assert_eq!(actual.as_array().unwrap().len(), 1);
        let actual_page = actual[0].as_object().unwrap();
        let expected_page = expected[0].as_object().unwrap();
        assert_eq!(
            actual_page.keys().collect::<Vec<_>>(),
            expected_page.keys().collect::<Vec<_>>()
        );
        let mut differences = Vec::new();
        for (field, value) in expected_page {
            let tolerance = match field.as_str() {
                "paths" | "segments" | "polylines" => PATH_TOLERANCE,
                "textSpans" => TEXT_TOLERANCE,
                _ => GRAPH_TOLERANCE,
            };
            if let Some(difference) =
                geometry_difference_with_tolerance(field, &actual_page[field], value, tolerance)
            {
                differences.push(difference);
            }
        }
        let expected_preview: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.preview.json"))).expect("preview baseline"),
        )
        .unwrap();
        if let Some(difference) = geometry_difference_with_tolerance(
            "preview",
            &preview,
            &expected_preview,
            TEXT_TOLERANCE,
        ) {
            differences.push(difference);
        }
        assert!(
            differences.is_empty(),
            "{fixture}: {}",
            differences.join("\n")
        );
    }
    assert!(
        matched_cases > 0,
        "selected real geometry case does not exist"
    );
}

#[test]
#[ignore = "requires both saved resource-bounded Farrelly p5 runs"]
fn farrelly_large_page_saved_full_and_preview_match_reference() {
    use serde_json::value::RawValue;
    use std::collections::BTreeMap;

    const PATH_TOLERANCE: f64 = 0.01;
    const TEXT_TOLERANCE: f64 = 0.02;
    const METADATA_TOLERANCE: f64 = 0.00001;
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("real corpus"));
    let candidate_prefix = std::env::var("GEOMETRY_LARGE_CANDIDATE_PREFIX")
        .unwrap_or_else(|_| "farrelly-p5-lit-shared-style-bounded".into());
    let expected_bytes = std::fs::read(dir.join("farrelly-p5.full.json")).expect("full baseline");
    let actual_bytes =
        std::fs::read(dir.join(format!("{candidate_prefix}.full.json"))).expect("full candidate");
    // Borrow raw field slices rather than expanding two gigabytes of JSON into
    // millions of simultaneous Value maps. Compare every array element in order.
    let expected: Vec<BTreeMap<&str, &RawValue>> = serde_json::from_slice(&expected_bytes).unwrap();
    let actual: Vec<BTreeMap<&str, &RawValue>> = serde_json::from_slice(&actual_bytes).unwrap();
    assert_eq!(actual.len(), 1);
    assert_eq!(actual.len(), expected.len());
    assert_eq!(
        actual[0].keys().collect::<Vec<_>>(),
        expected[0].keys().collect::<Vec<_>>()
    );
    for (field, old) in &expected[0] {
        let new = actual[0][field];
        let tolerance = match *field {
            "paths" | "segments" | "polylines" => PATH_TOLERANCE,
            "textSpans" => TEXT_TOLERANCE,
            _ => METADATA_TOLERANCE,
        };
        assert_raw_geometry_field_matches(field, new, old, tolerance);
    }
    let expected_preview: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("farrelly-p5-reference-bounded.stdout.json")).unwrap(),
    )
    .unwrap();
    let actual_preview: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join(format!("{candidate_prefix}.stdout.json"))).unwrap(),
    )
    .unwrap();
    if let Some(difference) = geometry_difference_with_tolerance(
        "preview",
        &actual_preview,
        &expected_preview,
        TEXT_TOLERANCE,
    ) {
        panic!("{difference}");
    }
}

#[test]
#[ignore = "requires complete saved Farrelly p5 Go and Rust relation artifacts"]
fn farrelly_large_page_saved_relations_match_reference() {
    use serde_json::value::RawValue;
    use std::collections::BTreeMap;

    const GRAPH_TOLERANCE: f64 = 0.00001;
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("real corpus"));
    let candidate_prefix = std::env::var("GEOMETRY_LARGE_RELATION_CANDIDATE_PREFIX")
        .unwrap_or_else(|_| "farrelly-p5-source-glyph-relations-shared-buffer-artifact".into());
    let expected_bytes =
        std::fs::read(dir.join("farrelly-p5.relations.json")).expect("complete Go graph reference");
    let actual_bytes = std::fs::read(dir.join(format!("{candidate_prefix}.full.json")))
        .expect("complete relation-enabled artifact");
    let expected: BTreeMap<&str, &RawValue> = serde_json::from_slice(&expected_bytes).unwrap();
    let actual: Vec<BTreeMap<&str, &RawValue>> = serde_json::from_slice(&actual_bytes).unwrap();
    assert_eq!(actual.len(), 1);
    for field in [
        "relations",
        "vertices",
        "vertexRefs",
        "relationsPartial",
        "relationsAnalyzed",
    ] {
        match (actual[0].get(field), expected.get(field)) {
            (Some(new), Some(old)) => {
                assert_raw_geometry_field_matches(field, new, old, GRAPH_TOLERANCE);
            }
            (None, None) => {}
            _ => panic!("{field}: wire presence differs"),
        }
    }
}

#[test]
#[ignore = "requires complete reference/candidate report artifacts and their manifest"]
fn saved_reports_match_every_full_geometry_field() {
    use serde_json::value::RawValue;
    use std::collections::BTreeMap;

    const PATH_TOLERANCE: f64 = 0.01;
    const TEXT_TOLERANCE: f64 = 0.02;
    const METADATA_TOLERANCE: f64 = 0.00001;
    const REQUIRED_REPORTS: usize = 3;
    let manifest = PathBuf::from(
        std::env::var_os("GEOMETRY_REPORT_MANIFEST").expect("complete report manifest"),
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    let cases = manifest["cases"].as_array().expect("report cases");
    assert_eq!(cases.len(), REQUIRED_REPORTS);
    let mut differences = Vec::new();
    for case in cases {
        let name = case["name"].as_str().expect("report name");
        let old = std::fs::read(case["reference"]["geometry"].as_str().unwrap()).unwrap();
        let new = std::fs::read(case["candidate"]["geometry"].as_str().unwrap()).unwrap();
        let expected: Vec<BTreeMap<&str, &RawValue>> = serde_json::from_slice(&old).unwrap();
        let actual: Vec<BTreeMap<&str, &RawValue>> = serde_json::from_slice(&new).unwrap();
        assert_eq!(
            expected.len(),
            case["totalPages"].as_u64().unwrap() as usize
        );
        if actual.len() != expected.len() {
            differences.push(format!("{name}: complete page counts differ"));
        }
        for (index, (new, old)) in actual.iter().zip(&expected).enumerate() {
            if new.keys().collect::<Vec<_>>() != old.keys().collect::<Vec<_>>() {
                differences.push(format!("{name} page {}: schema fields differ", index + 1));
            }
            for (field, old) in old {
                let Some(new) = new.get(field) else {
                    continue;
                };
                let tolerance = match *field {
                    "paths" | "segments" | "polylines" => PATH_TOLERANCE,
                    "textSpans" => TEXT_TOLERANCE,
                    _ => METADATA_TOLERANCE,
                };
                let label = format!("{name} page {} {field}", index + 1);
                if let Some(difference) = raw_geometry_field_difference(&label, new, old, tolerance)
                {
                    differences.push(difference);
                }
            }
        }
    }
    assert!(differences.is_empty(), "{}", differences.join("\n"));
}

fn assert_raw_geometry_field_matches(
    field: &str,
    new: &serde_json::value::RawValue,
    old: &serde_json::value::RawValue,
    tolerance: f64,
) {
    if let Some(difference) = raw_geometry_field_difference(field, new, old, tolerance) {
        panic!("{difference}");
    }
}

fn raw_geometry_field_difference(
    field: &str,
    new: &serde_json::value::RawValue,
    old: &serde_json::value::RawValue,
    tolerance: f64,
) -> Option<String> {
    if old.get().starts_with('[') && new.get().starts_with('[') {
        let mut old_values = raw_array_values(old.get());
        let mut new_values = raw_array_values(new.get());
        let mut index = 0;
        loop {
            match (new_values.next(), old_values.next()) {
                (None, None) => break,
                (Some(new), Some(old)) => {
                    if let Some(difference) = geometry_difference_with_tolerance(
                        &format!("{field}[{index}]"),
                        &new,
                        &old,
                        tolerance,
                    ) {
                        return Some(difference);
                    }
                }
                _ => return Some(format!("{field}: length differs at {index}")),
            }
            index += 1;
        }
        eprintln!("{field}: {index} complete ordered elements match");
    } else {
        let new: serde_json::Value = serde_json::from_str(new.get()).unwrap();
        let old: serde_json::Value = serde_json::from_str(old.get()).unwrap();
        return geometry_difference_with_tolerance(field, &new, &old, tolerance);
    }
    None
}

fn raw_array_values(input: &str) -> impl Iterator<Item = serde_json::Value> + '_ {
    assert!(input.starts_with('['));
    let mut remaining = &input.as_bytes()[1..];
    std::iter::from_fn(move || {
        remaining = remaining.trim_ascii_start();
        if remaining.starts_with(b",") {
            remaining = remaining[1..].trim_ascii_start();
        }
        if remaining.starts_with(b"]") {
            return None;
        }
        let mut stream =
            serde_json::Deserializer::from_slice(remaining).into_iter::<serde_json::Value>();
        let value = stream
            .next()
            .expect("array element")
            .expect("valid JSON element");
        remaining = &remaining[stream.byte_offset()..];
        Some(value)
    })
}

#[test]
fn raw_array_iteration_preserves_nested_values_and_escaped_delimiters() {
    let input = serde_json::json!([
        {"text":"a, ] \" b", "points":[[0,1],[2,3]]},
        {"x":42},
        [null, true, "[ , ]"]
    ])
    .to_string();
    let expected: Vec<serde_json::Value> = serde_json::from_str(&input).unwrap();
    assert_eq!(raw_array_values(&input).collect::<Vec<_>>(), expected);
    assert_eq!(raw_array_values("[ ]").count(), 0);
}

#[test]
#[ignore = "requires saved Go preview and full geometry artifacts"]
fn preview_matches_saved_go_output() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_PARITY_DIR").expect("geometry corpus"));
    for fixture in [
        "styled",
        "layered-curve",
        "resource-identity",
        "combined-gap",
        "form-oc-only",
    ] {
        let full: Vec<liteparse_geometry::model::PageGeometry> = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.full.json"))).expect("full baseline"),
        )
        .unwrap();
        let expected: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.preview.json"))).expect("preview baseline"),
        )
        .unwrap();
        let previews: Vec<_> = full
            .iter()
            .map(|page| liteparse_geometry::preview::filter_page(page, &Default::default()))
            .collect();
        let actual = serde_json::to_value(previews).unwrap();
        if let Some(difference) = first_geometry_difference(fixture, &actual, &expected) {
            panic!("{difference}");
        }
    }
}

#[test]
#[ignore = "requires the exported Go relation analyzer reference cases"]
fn relation_graph_matches_go_reference_cases() {
    #[derive(serde::Deserialize)]
    struct ReferenceCase {
        name: String,
        input: liteparse_geometry::model::PageGeometry,
        expected: serde_json::Value,
    }
    let dir =
        PathBuf::from(std::env::var_os("GEOMETRY_RELATION_PARITY_DIR").expect("relation corpus"));
    let cases: Vec<ReferenceCase> =
        serde_json::from_slice(&std::fs::read(dir.join("cases.json")).expect("reference cases"))
            .expect("decode reference cases");
    assert!(!cases.is_empty());
    for case in cases {
        let mut page = case.input;
        analyze_page(&mut page);
        let actual = serde_json::to_value(page).unwrap();
        for field in [
            "relations",
            "vertices",
            "vertexRefs",
            "relationsPartial",
            "relationsAnalyzed",
        ] {
            if let Some(difference) =
                first_geometry_difference(field, &actual[field], &case.expected[field])
            {
                panic!("{}: {difference}", case.name);
            }
        }
    }
}

#[test]
#[ignore = "requires saved synthetic relation geometry artifacts"]
fn relation_graph_matches_synthetic_artifacts() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_PARITY_DIR").expect("geometry corpus"));
    for fixture in [
        "styled",
        "layered-curve",
        "resource-identity",
        "combined-gap",
        "form-oc-only",
    ] {
        let expected: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.full.json"))).expect("baseline"),
        )
        .expect("decode baseline");
        let mut page: liteparse_geometry::model::PageGeometry =
            serde_json::from_value(expected[0].clone()).expect("page geometry");
        analyze_page(&mut page);
        let actual = serde_json::to_value(page).unwrap();
        for field in [
            "relations",
            "vertices",
            "vertexRefs",
            "relationsPartial",
            "relationsAnalyzed",
        ] {
            if let Some(difference) =
                first_geometry_difference(field, &actual[field], &expected[0][field])
            {
                panic!("{fixture}: {difference}");
            }
        }
    }
}

#[test]
#[ignore = "requires real raster geometry parity fixture"]
fn raster_centerlines_match_saved_geometry() {
    let source = PathBuf::from(std::env::var_os("RASTER_PDF").expect("RASTER_PDF"));
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("parity dir"));
    let baseline: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("raster-p1.full.json")).expect("raster baseline"),
    )
    .expect("decode baseline");
    let old = &baseline[0];
    let library = Library::try_init().expect("PDFium");
    let document = library
        .load_document(source.to_str().unwrap(), None)
        .expect("PDF");
    let page = document.page(0).expect("page");
    let bitmap = page.render_gray(72.0).expect("gray render");
    assert_eq!(bitmap.format(), BitmapFormat::Gray);
    let RasterTrace {
        segments,
        confidence,
        ..
    } = trace_raster_segments(
        bitmap.buffer(),
        bitmap.width() as usize,
        bitmap.height() as usize,
        bitmap.stride() as usize,
        72.0,
    );
    assert_eq!(segments.len(), old["segments"].as_array().unwrap().len());
    assert!((confidence - old["confidence"].as_f64().unwrap()).abs() < 0.00001);
    if let Some(difference) = first_geometry_difference(
        "segments",
        &serde_json::to_value(&segments).unwrap(),
        &old["segments"],
    ) {
        panic!("raster: {difference}");
    }
}

#[test]
#[ignore = "requires saved BSG MuPDF grayscale pixels"]
fn bsg_raster_algorithm_matches_mupdf_pixels() {
    assert_bsg_raster_pixels("bsg-mutool.gray");
}

#[test]
#[ignore = "requires saved candidate BSG lossless grayscale pixels"]
fn bsg_candidate_raster_pixels_match_baseline() {
    let name = std::env::var("GEOMETRY_RASTER_PIXELS").expect("candidate pixels filename");
    assert_bsg_raster_pixels(&name);
}

fn assert_bsg_raster_pixels(name: &str) {
    const BASELINE_WIDTH: usize = 4961;
    const BASELINE_HEIGHT: usize = 3508;
    const BASELINE_DPI: f64 = 300.0;
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("parity dir"));
    let baseline: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("bsg-p1.full.json")).expect("BSG baseline"))
            .expect("decode baseline");
    let gray = std::fs::read(dir.join(name)).expect("saved grayscale pixels");
    let RasterTrace {
        segments,
        confidence,
        ..
    } = trace_raster_segments(
        &gray,
        BASELINE_WIDTH,
        BASELINE_HEIGHT,
        BASELINE_WIDTH,
        BASELINE_DPI,
    );
    let old = &baseline[0];
    eprintln!(
        "{name}: segments={} expected={} confidence={confidence} expected-confidence={}",
        segments.len(),
        old["segments"].as_array().unwrap().len(),
        old["confidence"]
    );
    assert_eq!(segments.len(), old["segments"].as_array().unwrap().len());
    assert!((confidence - old["confidence"].as_f64().unwrap()).abs() < 0.00001);
    if let Some(difference) = first_geometry_difference(
        "segments",
        &serde_json::to_value(&segments).unwrap(),
        &old["segments"],
    ) {
        panic!("BSG raster algorithm: {difference}");
    }
}

#[test]
#[ignore = "requires the real BSG raster parity corpus"]
fn bsg_raster_centerlines_match_real_baseline() {
    let source = PathBuf::from(std::env::var_os("BSG_PDF").expect("BSG_PDF"));
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("parity dir"));
    let baseline: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("bsg-p1.full.json")).expect("BSG baseline"))
            .expect("decode baseline");
    let old = &baseline[0];
    let library = Library::try_init().expect("PDFium");
    let document = library
        .load_document(source.to_str().unwrap(), None)
        .expect("PDF");
    let page = document.page(0).expect("page");
    let bitmap = page.render_gray(300.0).expect("gray render");
    let pixels: Vec<u8> = bitmap
        .buffer()
        .chunks(bitmap.stride() as usize)
        .take(bitmap.height() as usize)
        .flat_map(|row| row[..bitmap.width() as usize].iter().copied())
        .collect();
    std::fs::write(dir.join("bsg-pdfium-matrix.gray"), &pixels).unwrap();
    let RasterTrace {
        segments,
        confidence,
        ..
    } = trace_raster_segments(
        bitmap.buffer(),
        bitmap.width() as usize,
        bitmap.height() as usize,
        bitmap.stride() as usize,
        300.0,
    );
    eprintln!(
        "BSG raster candidate={} old={} confidence={confidence} old-confidence={}",
        segments.len(),
        old["segments"].as_array().unwrap().len(),
        old["confidence"]
    );
    assert_eq!(segments.len(), old["segments"].as_array().unwrap().len());
    assert!((confidence - old["confidence"].as_f64().unwrap()).abs() < 0.00001);
    if let Some(difference) = first_geometry_difference(
        "segments",
        &serde_json::to_value(&segments).unwrap(),
        &old["segments"],
    ) {
        panic!("BSG rendered raster: {difference}");
    }
}

#[test]
#[ignore = "requires real PDF examples"]
fn pdfium_text_rect_groups_are_measured_against_old_spans() {
    for (key, page_number) in [("BSG_PDF", 1), ("FARRELLY_PDF", 1)] {
        let path = PathBuf::from(std::env::var_os(key).expect("real PDF path"));
        let library = Library::try_init().expect("load PDFium");
        let document = library
            .load_document(path.to_str().unwrap(), None)
            .expect("load PDF");
        let page = document.page(page_number - 1).expect("load page");
        let text = page.text().expect("load text");
        let rectangles = text.count_rects(0, -1);
        eprintln!(
            "{key}: text rectangles={rectangles} characters={}",
            text.char_count()
        );
        assert!(rectangles > 0);
    }
}

fn first_geometry_difference(
    path: &str,
    actual: &serde_json::Value,
    expected: &serde_json::Value,
) -> Option<String> {
    geometry_difference_with_tolerance(path, actual, expected, 0.00001)
}

fn geometry_difference_with_tolerance(
    path: &str,
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    tolerance: f64,
) -> Option<String> {
    use serde_json::Value;
    match (actual, expected) {
        (Value::Number(a), Value::Number(b)) => {
            let a = a.as_f64()?;
            let b = b.as_f64()?;
            ((a - b).abs() > tolerance).then(|| format!("{path}: {a} != {b}"))
        }
        (Value::Array(a), Value::Array(b)) => {
            if a.len() != b.len() {
                return Some(format!("{path}: length {} != {}", a.len(), b.len()));
            }
            a.iter().zip(b).enumerate().find_map(|(index, (a, b))| {
                geometry_difference_with_tolerance(&format!("{path}[{index}]"), a, b, tolerance)
            })
        }
        (Value::Object(a), Value::Object(b)) => {
            for key in a.keys().chain(b.keys()) {
                match (a.get(key), b.get(key)) {
                    (Some(a), Some(b)) => {
                        if let Some(diff) = geometry_difference_with_tolerance(
                            &format!("{path}.{key}"),
                            a,
                            b,
                            tolerance,
                        ) {
                            return Some(diff);
                        }
                    }
                    _ => return Some(format!("{path}.{key}: field presence differs")),
                }
            }
            None
        }
        _ => (actual != expected).then(|| format!("{path}: {actual} != {expected}")),
    }
}

#[test]
#[ignore = "requires the exported geometry parity corpus"]
fn source_path_arrays_match_all_synthetic_baselines() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_PARITY_DIR").expect("geometry corpus"));
    for fixture in [
        "styled",
        "layered-curve",
        "resource-identity",
        "combined-gap",
        "form-oc-only",
        "transparency",
        "tile-positive",
    ] {
        let actual =
            capture_page_paths(&dir.join(format!("{fixture}.pdf")), 1).expect("capture PDF paths");
        let baseline: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.full.json"))).expect("read baseline"),
        )
        .expect("decode baseline");
        let old = &baseline[0];
        assert_eq!(
            actual.path_ops,
            old["pathOps"].as_u64().unwrap() as usize,
            "{fixture} pathOps"
        );
        for (field, candidate) in [
            ("paths", serde_json::to_value(&actual.paths).unwrap()),
            ("segments", serde_json::to_value(&actual.segments).unwrap()),
            (
                "polylines",
                serde_json::to_value(&actual.polylines).unwrap(),
            ),
        ] {
            if let Some(diff) = first_geometry_difference(field, &candidate, &old[field]) {
                panic!("{fixture}: {diff}");
            }
        }
    }
}

#[test]
#[ignore = "requires the real geometry parity corpus"]
fn farrelly_source_path_counts_match_real_baseline() {
    let source = PathBuf::from(std::env::var_os("FARRELLY_PDF").expect("FARRELLY_PDF"));
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("real parity dir"));
    let baseline: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("farrelly-p1.full.json")).expect("read Farrelly baseline"),
    )
    .expect("decode Farrelly baseline");
    let actual = capture_page_paths(&source, 1).expect("capture Farrelly paths");
    let old = &baseline[0];
    assert_eq!(actual.path_ops, old["pathOps"].as_u64().unwrap() as usize);
    assert_eq!(actual.paths.len(), old["paths"].as_array().unwrap().len());
    assert_eq!(
        actual.segments.len(),
        old["segments"].as_array().unwrap().len()
    );
    for (index, (path, previous)) in actual
        .paths
        .iter()
        .zip(old["paths"].as_array().unwrap())
        .enumerate()
    {
        if let Some(diff) = geometry_difference_with_tolerance(
            "path",
            &serde_json::to_value(path).unwrap(),
            previous,
            0.01,
        ) {
            panic!("Farrelly paths[{index}]: {diff}");
        }
    }
    for (index, (segment, previous)) in actual
        .segments
        .iter()
        .zip(old["segments"].as_array().unwrap())
        .enumerate()
    {
        if let Some(diff) = geometry_difference_with_tolerance(
            "segment",
            &serde_json::to_value(segment).unwrap(),
            previous,
            0.01,
        ) {
            panic!("Farrelly segments[{index}]: {diff}");
        }
    }
}

#[test]
#[ignore = "requires the real geometry parity corpus"]
fn mangawhai_source_path_counts_match_real_baseline() {
    const BASELINE_PATH_OPS: usize = 84_492;
    let source = PathBuf::from(std::env::var_os("MANGAWHAI_PDF").expect("MANGAWHAI_PDF"));
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("real parity dir"));
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("real-manifest.json")).expect("read manifest"),
    )
    .expect("decode manifest");
    let baseline = manifest
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "mangawhai-p12")
        .expect("Mangawhai case");
    let content = capture_page_content(&source, 12).expect("capture Mangawhai page");
    let preview: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("mangawhai-p12.preview.json")).expect("read preview"),
    )
    .expect("decode preview");
    assert_eq!(
        content.image_ops,
        preview[0]["imageOps"].as_u64().unwrap() as usize
    );
    assert!(
        (content.image_coverage - preview[0]["imageCoverage"].as_f64().unwrap()).abs() < 0.00001
    );
    let actual = content.paths;
    assert_eq!(actual.path_ops, BASELINE_PATH_OPS);
    assert_eq!(
        actual.paths.len(),
        baseline["path_count"].as_u64().unwrap() as usize
    );
    assert_eq!(
        actual.segments.len(),
        baseline["segment_count"].as_u64().unwrap() as usize
    );
    let old_full: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("mangawhai-p12.full.json")).expect("read full baseline"),
    )
    .expect("decode full baseline");
    for (index, (path, previous)) in actual
        .paths
        .iter()
        .zip(old_full[0]["paths"].as_array().unwrap())
        .enumerate()
    {
        if let Some(diff) = geometry_difference_with_tolerance(
            "path",
            &serde_json::to_value(path).unwrap(),
            previous,
            0.01,
        ) {
            panic!("Mangawhai paths[{index}]: {diff}; candidate={path:?}; old={previous}");
        }
    }
    for (index, (segment, previous)) in actual
        .segments
        .iter()
        .zip(old_full[0]["segments"].as_array().unwrap())
        .enumerate()
    {
        if let Some(diff) = geometry_difference_with_tolerance(
            "segment",
            &serde_json::to_value(segment).unwrap(),
            previous,
            0.01,
        ) {
            panic!("Mangawhai segments[{index}]: {diff}");
        }
    }
}

#[test]
#[ignore = "requires the real geometry parity corpus"]
fn bsg_content_images_match_coverage_baseline() {
    let source = PathBuf::from(std::env::var_os("BSG_PDF").expect("BSG_PDF"));
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("real parity dir"));
    let baseline: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("bsg-p1.full.json")).expect("read BSG baseline"),
    )
    .expect("decode BSG baseline");
    let actual = capture_page_content(&source, 1).expect("capture BSG page");
    let old = &baseline[0];
    assert_eq!(actual.image_ops, old["imageOps"].as_u64().unwrap() as usize);
    assert!((actual.image_coverage - old["imageCoverage"].as_f64().unwrap()).abs() < 0.00001);
    assert_eq!(
        actual.paths.path_ops,
        old["pathOps"].as_u64().unwrap() as usize
    );
}

#[test]
#[ignore = "requires the exported geometry parity corpus"]
fn content_stream_recovers_styled_geometry_without_manual_state() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_PARITY_DIR").expect("geometry corpus"));
    let actual = capture_page_paths(&dir.join("styled.pdf"), 1).expect("capture styled page");
    let baseline: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("styled.full.json")).expect("read styled baseline"),
    )
    .expect("decode styled baseline");
    let old = &baseline[0];
    assert_eq!(actual.path_ops, old["pathOps"].as_u64().unwrap() as usize);
    assert_eq!(actual.paths.len(), old["paths"].as_array().unwrap().len());
    assert_eq!(
        actual.segments.len(),
        old["segments"].as_array().unwrap().len()
    );
    assert_eq!(actual.paths[0].style.paint, "stroke");
    assert_eq!(actual.paths[0].style.color_space, "DeviceRGB");
    assert_eq!(actual.paths[0].style.stroke_color, [1.0, 0.0, 0.0]);
    assert_eq!(actual.paths[0].style.dash_array, [3.0, 2.0]);
    assert_eq!(actual.paths[0].style.dash_phase, 1.0);
    assert_eq!(actual.paths[1].style.fill_color, [0.0, 1.0, 0.0]);
    assert_eq!(
        actual.paths[0].ops.len(),
        old["paths"][0]["ops"].as_array().unwrap().len()
    );
    assert_eq!(
        actual.paths[1].ops.len(),
        old["paths"][1]["ops"].as_array().unwrap().len()
    );
}

#[test]
#[ignore = "requires the exported geometry parity corpus"]
fn content_stream_recovers_nested_form_style_and_layer() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_PARITY_DIR").expect("geometry corpus"));
    let actual =
        capture_page_paths(&dir.join("combined-gap.pdf"), 1).expect("capture combined page");
    let baseline: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("combined-gap.full.json")).expect("read baseline"),
    )
    .expect("decode baseline");
    let old = &baseline[0];
    assert_eq!(actual.path_ops, old["pathOps"].as_u64().unwrap() as usize);
    assert_eq!(actual.paths.len(), old["paths"].as_array().unwrap().len());
    assert_eq!(
        actual.segments.len(),
        old["segments"].as_array().unwrap().len()
    );
    let form = &actual.paths[0].style;
    assert_eq!(form.form_x_object_id, old["paths"][0]["formXObjectId"]);
    assert_eq!(form.form_x_object_ref, old["paths"][0]["formXObjectRef"]);
    assert_eq!(
        form.form_ctm,
        serde_json::from_value::<Vec<f64>>(old["paths"][0]["formCtm"].clone()).unwrap()
    );
    assert_eq!(form.clip_depth, old["paths"][0]["clipDepth"]);
    assert_eq!(
        form.clip_bbox,
        serde_json::from_value::<Vec<f64>>(old["paths"][0]["clipBBox"].clone()).unwrap()
    );
    assert_eq!(form.layer, old["paths"][0]["layer"]);
    assert_eq!(
        form.stroke_color,
        serde_json::from_value::<Vec<f64>>(old["paths"][0]["strokeColor"].clone()).unwrap()
    );
}

#[test]
#[ignore = "requires the exported geometry parity corpus"]
fn content_stream_recovers_tile_and_transparency() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_PARITY_DIR").expect("geometry corpus"));
    for fixture in ["tile-positive", "transparency", "resource-identity"] {
        let actual = capture_page_paths(&dir.join(format!("{fixture}.pdf")), 1)
            .expect("capture source paths");
        let baseline: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.full.json"))).expect("read baseline"),
        )
        .expect("decode baseline");
        let old = &baseline[0];
        assert_eq!(
            actual.path_ops,
            old["pathOps"].as_u64().unwrap() as usize,
            "{fixture} pathOps"
        );
        assert_eq!(
            actual.paths.len(),
            old["paths"].as_array().unwrap().len(),
            "{fixture} paths"
        );
        assert_eq!(
            actual.segments.len(),
            old["segments"].as_array().unwrap().len(),
            "{fixture} segments"
        );
        if fixture == "tile-positive" {
            let tile = &actual.paths[2].style;
            assert_eq!(tile.tile_id, 6);
            assert_eq!(tile.tile_ctm, [1.0, 0.0, 0.0, -1.0, 0.0, 200.0]);
            assert_eq!(tile.fill_pattern_id, "P1");
        }
        if fixture == "transparency" {
            assert_eq!(actual.paths[0].style.alpha, 0.5);
        }
    }
}

#[test]
#[ignore = "requires the exported geometry parity corpus"]
fn path_capture_matches_styled_source_geometry() {
    let dir = PathBuf::from(
        std::env::var_os("GEOMETRY_PARITY_DIR")
            .expect("GEOMETRY_PARITY_DIR must name the exported corpus"),
    );
    let path = dir.join("styled.pdf");
    let baseline: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("styled.full.json")).expect("read styled baseline"),
    )
    .expect("decode styled baseline");
    let probe = inspect_page(&path, 1).expect("inspect styled source");
    assert!(
        probe.streams[0]
            .operations
            .iter()
            .any(|op| op.operator == "W")
    );
    let lib = Library::try_init().expect("load PDFium");
    let doc = lib
        .load_document(path.to_str().unwrap(), None)
        .expect("load PDF");
    let page = doc.page(0).expect("load page");
    let view = page.view_box().expect("page view box");
    let mut capture = PathCapture::default();
    let objects = page.path_objects(&view);
    assert_eq!(objects.len(), 3);
    for (index, object) in objects.iter().enumerate() {
        let mut style = PaintStyle::default();
        if index == 2 {
            style.clip_depth = 1;
            style.clip_bbox = vec![0.0, 0.0, 80.0, 200.0];
        }
        capture.paint(object, &style).expect("capture path");
    }
    let old = &baseline[0];
    assert_eq!(capture.path_ops, old["pathOps"].as_u64().unwrap() as usize);
    assert_eq!(capture.paths.len(), old["paths"].as_array().unwrap().len());
    assert_eq!(
        capture.segments.len(),
        old["segments"].as_array().unwrap().len()
    );
    for (actual, expected) in capture.paths.iter().zip(old["paths"].as_array().unwrap()) {
        assert_eq!(actual.closed, expected["closed"].as_bool().unwrap());
        for (op, previous) in actual.ops.iter().zip(expected["ops"].as_array().unwrap()) {
            assert_eq!(op.kind, previous["kind"].as_str().unwrap());
            assert!((op.x - previous["x"].as_f64().unwrap_or(0.0)).abs() < 0.01);
            assert!((op.y - previous["y"].as_f64().unwrap_or(0.0)).abs() < 0.01);
        }
    }
}

#[test]
#[ignore = "requires the exported geometry parity corpus"]
fn combined_gap_exposes_form_curve_layer_and_pattern_sources() {
    let dir = std::env::var_os("GEOMETRY_PARITY_DIR")
        .expect("GEOMETRY_PARITY_DIR must name the exported corpus");
    let path = PathBuf::from(dir).join("combined-gap.pdf");
    assert!(path.is_file(), "missing parity fixture: {}", path.display());
    let probe = inspect_page(&path, 1).expect("inspect combined gap PDF");
    assert_eq!((probe.width_pts, probe.height_pts), (200.0, 200.0));
    assert!(probe.objects.iter().any(|object| object.kind == "Path"
        && object.form_depth == 1
        && object.path_points.len() == 4));
    assert_eq!(
        probe
            .streams
            .first()
            .expect("page stream")
            .operations
            .iter()
            .filter(|operation| operation.operator == "W")
            .count(),
        2
    );
    let mut current_rect = None;
    let mut clip_rects = Vec::new();
    for operation in &probe.streams[0].operations {
        if operation.operator == "re" {
            let coordinates: Vec<f32> = operation
                .operands
                .iter()
                .map(|operand| operand.parse().expect("rectangle number"))
                .collect();
            current_rect = Some([
                coordinates[0],
                coordinates[1],
                coordinates[0] + coordinates[2],
                coordinates[1] + coordinates[3],
            ]);
        } else if operation.operator == "W" {
            clip_rects.push(current_rect.expect("rectangle before clipping"));
        }
    }
    let intersection = clip_rects
        .iter()
        .copied()
        .reduce(|a, b| {
            [
                a[0].max(b[0]),
                a[1].max(b[1]),
                a[2].min(b[2]),
                a[3].min(b[3]),
            ]
        })
        .expect("clip rectangles");
    assert_eq!(
        [
            intersection[0],
            probe.height_pts - intersection[3],
            intersection[2],
            probe.height_pts - intersection[1],
        ],
        [20.0, 80.0, 120.0, 170.0]
    );
    assert!(
        probe
            .resources
            .iter()
            .any(|resource| resource.category == "XObject"
                && resource.name == "F1"
                && resource.object_number == Some(5))
    );
    assert!(
        probe
            .resources
            .iter()
            .any(|resource| resource.category == "Pattern"
                && resource.name == "P1"
                && resource.object_number == Some(6))
    );
    assert!(
        probe
            .resources
            .iter()
            .any(|resource| resource.category == "Properties"
                && resource.name == "MC0"
                && resource.object_number == Some(7)
                && resource.layer_name.as_deref() == Some("Layer One"))
    );
    let page = &probe.streams[0];
    assert!(
        page.operations
            .iter()
            .any(|operation| operation.operator == "re"
                && operation.operands == ["20", "30", "100", "90"])
    );
    assert!(
        page.operations
            .iter()
            .any(|operation| operation.operator == "re"
                && operation.operands == ["20", "20", "120", "120"])
    );
    assert!(
        page.operations
            .iter()
            .any(|operation| operation.operator == "BDC" && operation.operands == ["/OC", "/MC0"])
    );
    assert!(
        page.operations
            .iter()
            .any(|operation| operation.operator == "Do" && operation.operands == ["/F1"])
    );
    let form = probe
        .streams
        .iter()
        .find(|stream| stream.source == "XObject/F1")
        .expect("Form stream");
    assert!(
        form.operations
            .iter()
            .any(|operation| operation.operator == "RG"
                && operation.operands == ["0.1", "0.2", "0.3"])
    );
    assert!(
        form.operations
            .iter()
            .any(|operation| operation.operator == "c" && operation.operands.len() == 6)
    );
}

#[test]
#[ignore = "requires the exported geometry parity corpus"]
fn repeated_form_invocations_have_distinct_page_transforms() {
    let dir = std::env::var_os("GEOMETRY_PARITY_DIR")
        .expect("GEOMETRY_PARITY_DIR must name the exported corpus");
    let path = PathBuf::from(dir).join("resource-identity.pdf");
    assert!(path.is_file(), "missing parity fixture: {}", path.display());
    let probe = inspect_page(&path, 1).expect("inspect repeated Form PDF");
    let invocations: Vec<_> = probe
        .objects
        .windows(2)
        .filter(|objects| objects[0].kind == "Form" && objects[1].kind == "Path")
        .map(|objects| {
            let form = objects[0].matrix.expect("Form matrix");
            let path = &objects[1];
            let child = path.path_points[0].point.expect("move point");
            [
                form[0] * child[0] + form[2] * child[1] + form[4],
                probe.height_pts - (form[1] * child[0] + form[3] * child[1] + form[5]),
            ]
        })
        .collect();
    assert_eq!(invocations, [[20.0, 180.0], [80.0, 180.0]]);
}

#[test]
#[ignore = "requires the exported geometry parity corpus"]
fn styled_page_retains_raw_dash_and_color_operands() {
    let dir = std::env::var_os("GEOMETRY_PARITY_DIR")
        .expect("GEOMETRY_PARITY_DIR must name the exported corpus");
    let path = PathBuf::from(dir).join("styled.pdf");
    assert!(path.is_file(), "missing parity fixture: {}", path.display());
    let probe = inspect_page(&path, 1).expect("inspect styled PDF");
    let page = &probe.streams[0];
    assert!(
        page.operations
            .iter()
            .any(|operation| operation.operator == "d" && operation.operands == ["[3 2]", "1"])
    );
    assert!(
        page.operations
            .iter()
            .any(|operation| operation.operator == "RG" && operation.operands == ["1", "0", "0"])
    );
    assert!(probe.objects.iter().any(|object| object.kind == "Path"
        && object.path_draw_mode == Some([false, true])
        && object.stroke_width == Some(2.0)
        && object.stroke_rgba == Some([255, 0, 0, 255])));
}

#[test]
#[ignore = "requires the exported geometry parity corpus"]
fn transparency_uses_graphics_state_resource() {
    let dir = std::env::var_os("GEOMETRY_PARITY_DIR")
        .expect("GEOMETRY_PARITY_DIR must name the exported corpus");
    let path = PathBuf::from(dir).join("transparency.pdf");
    assert!(path.is_file(), "missing parity fixture: {}", path.display());
    let probe = inspect_page(&path, 1).expect("inspect transparency PDF");
    assert!(
        probe.streams[0]
            .operations
            .iter()
            .any(|operation| operation.operator == "gs" && operation.operands == ["/GS1"])
    );
    assert!(probe.resources.iter().any(|resource| {
        resource.category == "ExtGState"
            && resource.name == "GS1"
            && resource.object_number == Some(9)
            && resource
                .properties
                .iter()
                .any(|(key, value)| key == "CA" && value == "0.5")
            && resource
                .properties
                .iter()
                .any(|(key, value)| key == "ca" && value == "0.25")
    }));
}

#[test]
#[ignore = "requires the exported geometry parity corpus"]
fn pattern_object_number_and_page_transform_explain_tile_provenance() {
    let dir = PathBuf::from(
        std::env::var_os("GEOMETRY_PARITY_DIR")
            .expect("GEOMETRY_PARITY_DIR must name the exported corpus"),
    );
    let path = dir.join("tile-positive.pdf");
    let baseline_path = dir.join("tile-positive.full.json");
    assert!(
        path.is_file() && baseline_path.is_file(),
        "missing tile parity fixture"
    );
    let probe = inspect_page(&path, 1).expect("inspect tile fixture");
    let pattern = probe
        .resources
        .iter()
        .find(|resource| resource.category == "Pattern" && resource.name == "P1")
        .expect("P1 pattern resource");
    assert_eq!(pattern.object_number, Some(6));
    assert!(
        pattern
            .properties
            .iter()
            .any(|(key, value)| key == "XStep" && value == "8")
    );
    assert!(probe.streams.iter().any(|stream| {
        stream.source == "Pattern/P1"
            && stream.operations.iter().any(|operation| {
                operation.operator == "re" && operation.operands == ["0", "0", "8", "8"]
            })
    }));
    let baseline: serde_json::Value =
        serde_json::from_slice(&std::fs::read(baseline_path).expect("read MuPDF tile baseline"))
            .expect("decode MuPDF tile baseline");
    let tile = baseline[0]["paths"]
        .as_array()
        .expect("baseline paths")
        .iter()
        .find(|path| path["tileId"].as_u64().is_some())
        .expect("tile path");
    assert_eq!(
        tile["tileId"].as_u64(),
        pattern.object_number.map(u64::from)
    );
    let expected_ctm = [1.0, 0.0, 0.0, -1.0, 0.0, f64::from(probe.height_pts)];
    let actual_ctm: Vec<f64> = serde_json::from_value(tile["tileCtm"].clone()).expect("tile CTM");
    assert_eq!(actual_ctm, expected_ctm);
}

#[test]
#[ignore = "requires the raster fixture from the local parity corpus"]
fn raster_page_renders_lossless_grayscale() {
    let path = PathBuf::from(
        std::env::var_os("RASTER_PDF").expect("RASTER_PDF must name the raster geometry fixture"),
    );
    assert!(path.is_file(), "missing parity fixture: {}", path.display());
    let library = Library::try_init().expect("PDFium library");
    let path_str = path.to_str().expect("UTF-8 fixture path");
    let document = library.load_document(path_str, None).expect("raster PDF");
    let page = document.page(0).expect("first page");
    const RASTER_DPI: f32 = 72.0;
    const PAGE_PIXELS: i32 = 128;
    const DARK_PIXEL_LIMIT: u8 = 128;
    let bitmap = page.render_gray(RASTER_DPI).expect("grayscale render");
    assert_eq!(bitmap.format(), BitmapFormat::Gray);
    assert_eq!(
        (bitmap.width(), bitmap.height()),
        (PAGE_PIXELS, PAGE_PIXELS)
    );
    assert!(
        bitmap
            .buffer()
            .iter()
            .any(|pixel| *pixel < DARK_PIXEL_LIMIT)
    );
}

#[test]
#[ignore = "requires the Farrelly drawing from the local parity corpus"]
fn farrelly_font_resources_preserve_subset_prefixes() {
    let path = PathBuf::from(
        std::env::var_os("FARRELLY_PDF").expect("FARRELLY_PDF must name the drawing PDF"),
    );
    assert!(path.is_file(), "missing parity fixture: {}", path.display());
    let probe = inspect_page(&path, 1).expect("inspect rotated Farrelly page");
    assert_eq!(probe.rotation, 3);
    assert_eq!((probe.width_pts, probe.height_pts), (1684.0, 1191.0));
    assert!(
        probe
            .text_samples
            .iter()
            .any(|sample| sample.font_name.as_deref() == Some("ArialMT"))
    );
    assert!(probe.resources.iter().any(|resource| {
        resource.category == "Font"
            && resource.name == "TT4"
            && resource
                .properties
                .iter()
                .any(|(key, value)| key == "BaseFont" && value == "/HBIGAE+ArialMT")
    }));
    const BASELINE_PATH_OPS: usize = 23_083;
    let painted_paths = probe
        .streams
        .iter()
        .flat_map(|stream| &stream.operations)
        .filter(|operation| {
            matches!(
                operation.operator.as_str(),
                "S" | "s" | "f" | "F" | "f*" | "B" | "B*" | "b" | "b*"
            )
        })
        .count();
    let dual_paint_paths = probe
        .streams
        .iter()
        .flat_map(|stream| &stream.operations)
        .filter(|operation| matches!(operation.operator.as_str(), "B" | "B*" | "b" | "b*"))
        .count();
    assert_eq!(painted_paths + dual_paint_paths, BASELINE_PATH_OPS);
}

#[test]
#[ignore = "requires the BSG drawing from the local parity corpus"]
fn bsg_image_bounds_recover_baseline_coverage() {
    let path =
        PathBuf::from(std::env::var_os("BSG_PDF").expect("BSG_PDF must name the drawing PDF"));
    assert!(path.is_file(), "missing parity fixture: {}", path.display());
    let probe = inspect_page(&path, 1).expect("inspect BSG page");
    assert!(
        probe
            .resources
            .iter()
            .any(|resource| resource.owner == "XObject/Fm0"
                && resource.category == "Font"
                && resource.name == "TT0"
                && resource.object_number == Some(368))
    );
    const BASELINE_PATH_OPS: usize = 2;
    assert_eq!(
        probe
            .objects
            .iter()
            .filter(|object| object.kind == "Path")
            .count(),
        BASELINE_PATH_OPS
    );
    let painted_area: f64 = probe
        .objects
        .iter()
        .filter(|object| object.kind == "Image")
        .map(|object| {
            let [left, bottom, right, top] = object.bounds.expect("image bounds");
            f64::from((right - left) * (top - bottom))
        })
        .sum();
    const BASELINE_COVERAGE: f64 = 0.10645066459951227;
    const COVERAGE_TOLERANCE: f64 = 0.00001;
    let coverage = painted_area / f64::from(probe.width_pts * probe.height_pts);
    assert!(
        (coverage - BASELINE_COVERAGE).abs() <= COVERAGE_TOLERANCE,
        "PDFium image coverage {coverage} differs from MuPDF {BASELINE_COVERAGE}"
    );
}

#[test]
#[ignore = "requires saved real-page geometry and Go relation references"]
fn real_page_relations_match_go_baselines() {
    let dir = PathBuf::from(std::env::var_os("GEOMETRY_REAL_PARITY_DIR").expect("real corpus"));
    for fixture in ["bsg-p1", "farrelly-p1", "mangawhai-p12"] {
        if std::env::var("GEOMETRY_RELATION_CASE").is_ok_and(|name| name != fixture) {
            continue;
        }
        let pages: Vec<liteparse_geometry::model::PageGeometry> = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.full.json"))).unwrap(),
        )
        .unwrap();
        let mut page = pages.into_iter().next().unwrap();
        analyze_page(&mut page);
        let expected: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(format!("{fixture}.relations.json"))).unwrap(),
        )
        .unwrap();
        let actual = serde_json::to_value(page).unwrap();
        let graph: serde_json::Map<String, serde_json::Value> = [
            "relations",
            "vertices",
            "vertexRefs",
            "relationsAnalyzed",
            "relationsPartial",
        ]
        .into_iter()
        .map(|field| (field.to_owned(), actual[field].clone()))
        .collect();
        std::fs::write(
            dir.join(format!("{fixture}.lit-relations.json")),
            serde_json::to_vec(&graph).unwrap(),
        )
        .unwrap();

        for field in [
            "relations",
            "vertices",
            "vertexRefs",
            "relationsAnalyzed",
            "relationsPartial",
        ] {
            if let Some(difference) =
                first_geometry_difference(field, &actual[field], &expected[field])
            {
                panic!("{fixture}: {difference}");
            }
        }
    }
}

#[test]
fn json_geometry_coordinates_keep_binary_precision() {
    const COORDINATES: &[f64] = &[
        352.3789367675781,
        354.2989501953125,
        354.47894287109375,
        354.7789306640625,
        355.6369323730469,
        355.7579345703125,
        356.3309326171875,
        356.5789489746094,
        356.5859375,
        356.9989318847656,
        357.4189453125,
        357.47894287109375,
        357.59893798828125,
        357.82196044921875,
        358.02294921875,
        368.09893798828125,
        369.2989501953125,
        374.2189636230469,
        742.6199951171875,
        743.760009765625,
        743.7830200195312,
        743.8200073242188,
        743.9979858398438,
        744.1199951171875,
        745.02001953125,
        745.0859985351562,
        745.4400024414062,
        746.2510375976562,
        746.3760375976562,
        746.7000122070312,
        746.760009765625,
        746.8909912109375,
        746.9400024414062,
        747.8399658203125,
        752.52001953125,
    ];
    for coordinate in COORDINATES {
        let bytes = serde_json::to_vec(coordinate).unwrap();
        let recovered: f64 = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            recovered.to_bits(),
            coordinate.to_bits(),
            "coordinate {coordinate}"
        );
    }
}
