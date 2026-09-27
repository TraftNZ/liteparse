use std::io::Write;
use std::process::{Command, Stdio};

use base64::Engine;
use image::GenericImageView;
use serde_json::{Value, json};

fn fixture() -> String {
    format!(
        "{}/tests/fixtures/soft_mask.pdf",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn invoke(request: Value) -> Value {
    let output = invoke_output(request);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn invoke_output(request: Value) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lit"))
        .arg("pdf")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn measurement_fixture() -> (std::path::PathBuf, std::path::PathBuf) {
    use lopdf::{Document, Object, Stream, dictionary};
    let directory = std::env::temp_dir().join(format!(
        "lit-measurement-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("drawing.pdf");
    let mut document = Document::with_version("1.7");
    let pages = document.new_object_id();
    let contents = document.add_object(Stream::new(
        dictionary! {},
        b"0 0 0 RG 1 w 72 72 m 216 72 l S 72 144 144 144 re S".to_vec(),
    ));
    let page = document.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages, "MediaBox" => vec![0.into(), 0.into(), 720.into(), 360.into()],
        "Resources" => dictionary! {}, "Contents" => contents,
    });
    document.objects.insert(
        pages,
        Object::Dictionary(dictionary! {"Type"=>"Pages", "Kids"=>vec![page.into()], "Count"=>1}),
    );
    let catalog = document.add_object(dictionary! {"Type"=>"Catalog", "Pages"=>pages});
    document.trailer.set("Root", catalog);
    document.save(&path).unwrap();
    (directory, path)
}

fn assert_close(actual: &Value, expected: f64) {
    const TOLERANCE: f64 = 1e-6;
    let actual = actual.as_f64().unwrap();
    assert!(
        (actual - expected).abs() <= TOLERANCE,
        "{actual} != {expected}"
    );
}

#[test]
fn measures_real_pdf_points_and_calibration_without_assumed_scale() {
    let (directory, path) = measurement_fixture();
    let request = json!({"version":1,"operation":"measure","pdf_path":path,"page_number":1,
        "measurement":{"points":[[0.1,0.8],[0.3,0.8]]}});
    let measured = invoke(request.clone());
    assert_close(&measured["measurement"]["paper_length_mm"], 50.8);
    assert_close(&measured["measurement"]["edge_lengths_paper_mm"][0], 50.8);
    assert!(measured["measurement"]["real_length_mm"].is_null());
    assert_eq!(measured["measurement"]["backend"], "pdfium");
    assert_eq!(measured["measurement"]["extractor_generation"], 9);
    let mut calibrated = request;
    calibrated["measurement"]["known_distance_mm"] = json!(5080);
    let measured = invoke(calibrated);
    assert_close(&measured["measurement"]["scale_denominator"], 100.0);
    assert_close(&measured["measurement"]["real_length_mm"], 5080.0);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn measures_area_perimeter_holes_and_complete_source_polyline() {
    let (directory, path) = measurement_fixture();
    let mut request = json!({"version":1,"operation":"measure","pdf_path":path,"page_number":1,
        "measurement":{"mode":"area","points":[[0.1,0.2],[0.3,0.2],[0.3,0.6],[0.1,0.6]],
            "holes":[[[0.15,0.3],[0.25,0.3],[0.25,0.5],[0.15,0.5]]],"scale_denominator":100}});
    let measured = invoke(request.clone());
    assert_close(&measured["measurement"]["paper_area_mm2"], 1935.48);
    assert_close(&measured["measurement"]["real_area_mm2"], 19_354_800.0);
    assert_close(&measured["measurement"]["paper_length_mm"], 304.8);
    let edges = measured["measurement"]["edge_lengths_paper_mm"]
        .as_array()
        .unwrap();
    assert_eq!(edges.len(), 4);
    for edge in edges {
        assert_close(edge, 50.8);
    }
    request["measurement"]["mode"] = json!("perimeter");
    let measured = invoke(request);
    assert_close(&measured["measurement"]["paper_length_mm"], 304.8);
    assert_close(&measured["measurement"]["paper_area_mm2"], 1935.48);
    let geometry =
        invoke(json!({"version":1,"operation":"geometry","pdf_path":path,"page_number":1}));
    let index = geometry[0]["polylines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|polyline| polyline["closed"] == true)
        .unwrap()["index"]
        .as_u64()
        .unwrap();
    assert!(index > 0);
    let preview = json!({"max_segments":1,"bbox_frac":[0.6,0.6,0.9,0.9]});
    let filtered = invoke(
        json!({"version":1,"operation":"geometry","pdf_path":path,"page_number":1,
        "geometry":{"preview":preview}}),
    );
    assert!(filtered[0]["polylines"].as_array().unwrap().is_empty());
    let measured = invoke(
        json!({"version":1,"operation":"measure","pdf_path":path,"page_number":1,
        "geometry":{"preview":preview},"measurement":{"mode":"area","polyline_index":index}}),
    );
    assert_close(&measured["measurement"]["paper_area_mm2"], 2580.64);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn measurement_snaps_to_actual_extracted_line_and_rejects_invalid_boundaries() {
    let (directory, path) = measurement_fixture();
    let measured = invoke(
        json!({"version":1,"operation":"measure","pdf_path":path,"page_number":1,
        "measurement":{"points":[[0.1,0.801],[0.3,0.801]],"snap_distance_mm":1.0}}),
    );
    assert_close(&measured["measurement"]["points_pts"][0][1], 288.0);
    assert_eq!(
        measured["measurement"]["snaps"].as_array().unwrap().len(),
        2
    );
    let geometry =
        invoke(json!({"version":1,"operation":"geometry","pdf_path":path,"page_number":1}));
    let line_index = &geometry[0]["segments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|segment| segment["y1"] == 288.0 && segment["y2"] == 288.0)
        .unwrap()["index"];
    for snap in measured["measurement"]["snaps"].as_array().unwrap() {
        assert_eq!(&snap["segment_index"], line_index);
        assert_close(&snap["shift_paper_mm"], 0.127);
    }
    for measurement in [
        json!({"mode":"area","points":[[0.1,0.1],[0.8,0.8],[0.1,0.8],[0.8,0.1]]}),
        json!({"mode":"area","points":[[0.1,0.1],[0.8,0.8],[0.1,0.7],[0.6,0.1]]}),
        json!({"mode":"area","points":[[0.1,0.1],[0.8,0.1],[0.8,0.8],[0.1,0.8]],"holes":[[[0.1,0.2],[0.3,0.2],[0.3,0.4],[0.1,0.4]]]}),
        json!({"mode":"area","points":[[0.1,0.1],[0.8,0.1],[0.8,0.8],[0.1,0.8]],"holes":[[[0.2,0.2],[0.5,0.2],[0.5,0.5],[0.2,0.5]],[[0.4,0.4],[0.6,0.4],[0.6,0.6],[0.4,0.6]]]}),
        json!({"mode":"area","points":[[0.1,0.1],[0.8,0.1],[0.8,0.8],[0.1,0.8]],"holes":[[[0.2,0.2],[0.7,0.2],[0.7,0.7],[0.2,0.7]],[[0.3,0.3],[0.4,0.3],[0.4,0.4],[0.3,0.4]]]}),
        json!({"mode":"area","points":[[0.1,0.1],[0.8,0.1],[0.8,0.8],[0.1,0.8]],"holes":[[[0.7,0.7],[0.9,0.7],[0.9,0.9],[0.7,0.9]]]}),
        json!({"points":[[0.1,0.1],[0.1,0.1]],"known_distance_mm":100}),
        json!({"points":[[0.1,0.1],[0.2,0.2]],"scale_denominator":100,"known_distance_mm":100}),
        json!({"points":[[0.1,0.1],[1.2,0.2]]}),
    ] {
        let output = invoke_output(
            json!({"version":1,"operation":"measure","pdf_path":path,"page_number":1,"measurement":measurement}),
        );
        assert!(!output.status.success());
        assert!(
            output.stdout.is_empty(),
            "failed measurement emitted a success response"
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn fractional_page_preserves_exact_dpi_at_the_painted_edge() {
    const EDGE_COLUMN: u32 = 80;
    const SAMPLE_ROWS: std::ops::Range<u32> = 10..90;
    const MAX_EDGE_CHANNEL_DELTA: u8 = 10;
    const PAGE_WIDTH_PTS: f64 = 100.25;
    const CLIP_ORIGIN_X_PTS: u32 = 50;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let reference = image::open(root.join("fractional_render_page.reference.jpg"))
        .unwrap()
        .to_rgb8();
    let response = invoke(json!({"version":1,"operation":"render_pages",
        "pdf_path":root.join("fractional_render_page.pdf"),"page_numbers":[1],"dpi":72}));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(response["images"][0]["data"].as_str().unwrap())
        .unwrap();
    let actual = image::load_from_memory(&bytes).unwrap().to_rgb8();
    assert_eq!(actual.dimensions(), reference.dimensions());
    for row in SAMPLE_ROWS {
        for (actual, expected) in actual
            .get_pixel(EDGE_COLUMN, row)
            .0
            .into_iter()
            .zip(reference.get_pixel(EDGE_COLUMN, row).0)
        {
            assert!(actual.abs_diff(expected) <= MAX_EDGE_CHANNEL_DELTA);
        }
    }
    let response = invoke(json!({"version":1,"operation":"native_clip",
        "pdf_path":root.join("fractional_render_page.pdf"),"page_number":1,"dpi":72,
        "rect":[f64::from(CLIP_ORIGIN_X_PTS)/PAGE_WIDTH_PTS,0.0,
            (PAGE_WIDTH_PTS-f64::from(CLIP_ORIGIN_X_PTS))/PAGE_WIDTH_PTS,1.0]}));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(response["image"]["data"].as_str().unwrap())
        .unwrap();
    let clip = image::load_from_memory(&bytes).unwrap().to_rgb8();
    assert_eq!(
        clip.dimensions(),
        (reference.width() - CLIP_ORIGIN_X_PTS, reference.height())
    );
    for row in SAMPLE_ROWS {
        for (actual, expected) in clip
            .get_pixel(EDGE_COLUMN - CLIP_ORIGIN_X_PTS, row)
            .0
            .into_iter()
            .zip(reference.get_pixel(EDGE_COLUMN, row).0)
        {
            assert!(actual.abs_diff(expected) <= MAX_EDGE_CHANNEL_DELTA);
        }
    }
}

fn assert_geometry_equal(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Number(actual), Value::Number(expected)) => {
            // JSON integer and decimal spellings carry the same geometry value.
            assert_eq!(actual.as_f64(), expected.as_f64());
        }
        (Value::Array(actual), Value::Array(expected)) => {
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert_geometry_equal(actual, expected);
            }
        }
        (Value::Object(actual), Value::Object(expected)) => {
            assert_eq!(actual.len(), expected.len());
            for (key, expected) in expected {
                assert_geometry_equal(actual.get(key).unwrap(), expected);
            }
        }
        _ => assert_eq!(actual, expected),
    }
}

fn assert_source_geometry_fixture(name: &str) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let directory = tempfile::tempdir().unwrap();
    let artifact = directory.path().join("geometry.json");
    let preview = invoke(json!({"version":1,"operation":"geometry",
        "pdf_path":root.join(format!("{name}.pdf")),
        "geometry":{"artifact_out":artifact,"no_raster":true}}));
    let actual: Value = serde_json::from_slice(&std::fs::read(artifact).unwrap()).unwrap();
    assert_eq!(
        preview[0]["version"],
        liteparse_geometry::model::EXTRACTOR_VERSION
    );
    assert_eq!(actual[0]["version"], preview[0]["version"]);
    let reference: Value =
        serde_json::from_slice(&std::fs::read(root.join(format!("{name}.geometry.json"))).unwrap())
            .unwrap();
    for field in ["paths", "segments", "polylines"] {
        assert_geometry_equal(&actual[0][field], &reference[0][field]);
    }
}

#[test]
fn source_path_omits_repeated_endpoints_after_curves_and_close() {
    assert_source_geometry_fixture("repeated_path_endpoints");
}

#[test]
fn source_rectangles_preserve_degenerate_edges() {
    assert_source_geometry_fixture("degenerate_rectangles");
}

#[test]
fn source_curves_preserve_reference_degenerate_commands() {
    assert_source_geometry_fixture("degenerate_curves");
}

#[test]
fn geometry_preserves_spaces_at_font_boundaries() {
    assert_text_geometry_fixture("font_boundary_spaces");
}

#[test]
fn geometry_counts_overlapping_native_spaces_once() {
    assert_text_geometry_fixture("overlapping_native_spaces");
}

fn assert_text_geometry_fixture(name: &str) {
    const TEXT_POSITION_TOLERANCE_PTS: f64 = 0.02;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let directory = tempfile::tempdir().unwrap();
    let artifact = directory.path().join("geometry.json");
    invoke(json!({"version":1,"operation":"geometry",
        "pdf_path":root.join(format!("{name}.pdf")),
        "geometry":{"artifact_out":artifact,"no_raster":true}}));
    let actual: Value = serde_json::from_slice(&std::fs::read(artifact).unwrap()).unwrap();
    let expected: Value =
        serde_json::from_slice(&std::fs::read(root.join(format!("{name}.geometry.json"))).unwrap())
            .unwrap();
    let actual = actual[0]["textSpans"].as_array().unwrap();
    let expected = expected[0]["textSpans"].as_array().unwrap();
    assert_eq!(actual.len(), expected.len());
    for (actual, expected) in actual.iter().zip(expected) {
        assert_eq!(
            actual.as_object().unwrap().keys().collect::<Vec<_>>(),
            expected.as_object().unwrap().keys().collect::<Vec<_>>()
        );
        for field in ["text", "fontName", "fontSize"] {
            assert_geometry_equal(&actual[field], &expected[field]);
        }
        for field in ["x0", "y0", "x1", "y1"] {
            assert!(
                (actual[field].as_f64().unwrap() - expected[field].as_f64().unwrap()).abs()
                    <= TEXT_POSITION_TOLERANCE_PTS,
                "{field}: {} != {}",
                actual[field],
                expected[field]
            );
        }
    }
}

#[test]
fn pdf_jpeg_matches_reference_chroma_sampling_and_quality_tables() {
    const JPEG_SOI: [u8; 2] = [0xff, 0xd8];
    const JPEG_SOS: u8 = 0xda;
    const JPEG_SOF: u8 = 0xc0;
    const JPEG_DQT: u8 = 0xdb;
    const MARKER_BYTES: usize = 2;
    const LENGTH_BYTES: usize = 2;
    const SOF_HEADER_BYTES: usize = 6;
    const COMPONENT_BYTES: usize = 3;
    const QUANTIZATION_BLOCK_BYTES: usize = 65;
    fn metadata(bytes: &[u8]) -> (Vec<u8>, std::collections::BTreeMap<u8, Vec<u8>>) {
        assert!(bytes.starts_with(&JPEG_SOI));
        let mut position = MARKER_BYTES;
        let mut frame = Vec::new();
        let mut tables = std::collections::BTreeMap::new();
        loop {
            assert_eq!(bytes[position], JPEG_SOI[0]);
            let marker = bytes[position + 1];
            position += MARKER_BYTES;
            if marker == JPEG_SOS {
                break;
            }
            let length = usize::from(u16::from_be_bytes([bytes[position], bytes[position + 1]]));
            let payload = &bytes[position + LENGTH_BYTES..position + length];
            match marker {
                JPEG_SOF => {
                    frame.extend_from_slice(&payload[..SOF_HEADER_BYTES]);
                    for component in payload[SOF_HEADER_BYTES..].as_chunks::<COMPONENT_BYTES>().0 {
                        // Component ids are encoder-local; factors/table ids are the contract.
                        frame.extend_from_slice(&component[1..]);
                    }
                }
                JPEG_DQT => {
                    assert_eq!(payload.len() % QUANTIZATION_BLOCK_BYTES, 0);
                    for table in payload.as_chunks::<QUANTIZATION_BLOCK_BYTES>().0 {
                        tables.insert(table[0], table[1..].to_vec());
                    }
                }
                _ => {}
            }
            position += length;
        }
        assert!(!frame.is_empty() && !tables.is_empty());
        (frame, tables)
    }
    let response = invoke(json!({"version":1,"operation":"render_pages",
        "pdf_path":fixture(),"page_numbers":[1],"dpi":72}));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(response["images"][0]["data"].as_str().unwrap())
        .unwrap();
    let reference = include_bytes!("fixtures/soft_mask_72dpi.reference.jpg");
    assert_eq!(metadata(&bytes), metadata(reference));
    assert_eq!(
        image::load_from_memory(&bytes).unwrap().dimensions(),
        image::load_from_memory(reference).unwrap().dimensions()
    );
}

#[test]
fn geometry_aligns_consecutive_empty_quoted_and_form_text_shows() {
    use lopdf::{Document, Object, Stream, dictionary};

    const PAGE_SIZE_PTS: i64 = 200;
    const TEXT_POSITION_TOLERANCE_PTS: f64 = 0.00001;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("text-positioning.pdf");
    let mut source = Document::with_version("1.7");
    let pages_id = source.new_object_id();
    let font_id = source.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let form_id = source.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 50.into(), 20.into()],
            "Matrix" => vec![1.into(), 0.into(), 0.into(), 1.into(), 120.into(), 120.into()],
            "Resources" => dictionary! {"Font" => dictionary! {"F1" => font_id}},
        },
        b"BT /F1 8 Tf 1 0 0 1 0 0 Tm (FORM) Tj ET".to_vec(),
    ));
    let contents_id = source.add_object(Stream::new(
        dictionary! {},
        b"BT /F1 10 Tf 1 0 0 1 20 180 Tm (A) Tj (B) Tj [() 20 ()] TJ \
        [(C) -40 (D)] TJ 12 TL (E) ' 0 0 (F) \" 3 Ts 0 -12 TD (G) Tj ET /Form Do"
            .to_vec(),
    ));
    let page_id = source.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), PAGE_SIZE_PTS.into(), PAGE_SIZE_PTS.into()],
        "Contents" => contents_id,
        "Resources" => dictionary! {
            "Font" => dictionary! {"F1" => font_id},
            "XObject" => dictionary! {"Form" => form_id},
        },
    });
    source.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Count" => 1, "Kids" => vec![page_id.into()],
        }),
    );
    let catalog_id = source.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages_id});
    source.trailer.set("Root", catalog_id);
    source.save(&path).unwrap();
    let request = json!({"version":1,"operation":"geometry","pdf_path":path,
        "geometry":{"no_raster":true}});
    let first = invoke(request.clone());
    assert_eq!(invoke(request), first, "text positioning is deterministic");
    let spans = first[0]["textSpans"].as_array().unwrap();
    let text: String = spans
        .iter()
        .map(|span| span["text"].as_str().unwrap())
        .collect();
    assert_eq!(text, "ABCDEFGFORM");
    let form = spans.iter().find(|span| span["text"] == "FORM").unwrap();
    assert_eq!(form["x0"], 120.0);
    assert_eq!(form["fontSize"], 8.0);
    assert!(
        (form["y0"].as_f64().unwrap() - 68.4).abs() < TEXT_POSITION_TOLERANCE_PTS,
        "Form text inherits the parent's 3-point text rise"
    );
    let raised = spans.iter().find(|span| span["text"] == "G").unwrap();
    assert!((raised["y0"].as_f64().unwrap() - 42.25).abs() < f64::from(f32::EPSILON));
}

#[test]
fn geometry_recovers_custom_cmap_zero_codes_and_declared_glyph_widths() {
    use lopdf::{Document, Object, Stream, dictionary};

    const PAGE_SIZE_PTS: i64 = 100;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("custom-cmap.pdf");
    let mut source = Document::with_version("1.7");
    let pages_id = source.new_object_id();
    let descriptor = source.add_object(dictionary! {
        "Type" => "FontDescriptor", "FontName" => "Helvetica", "Flags" => 32,
        "FontBBox" => vec![0.into(), (-250).into(), 1000.into(), 750.into()],
        "Ascent" => 750, "Descent" => -250, "CapHeight" => 750,
        "ItalicAngle" => 0, "StemV" => 80,
    });
    let descendant = source.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "CIDFontType2", "BaseFont" => "Helvetica",
        "CIDSystemInfo" => dictionary! {"Registry" => Object::string_literal("Adobe"),
            "Ordering" => Object::string_literal("Identity"), "Supplement" => 0},
        "FontDescriptor" => descriptor, "CIDToGIDMap" => "Identity",
        "DW" => 500, "W" => vec![1.into(), Object::Array(vec![500.into(), 500.into()])],
    });
    let encoding = source.add_object(Stream::new(dictionary! {}, b"/CIDInit /ProcSet findresource begin
        12 dict begin begincmap /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> def
        /CMapName /GeometryCodes def /CMapType 1 def /WMode 0 def
        1 begincodespacerange <0000> <ffff> endcodespacerange
        2 begincidchar <0000> 1 <0041> 2 endcidchar
        endcmap CMapName currentdict /CMap defineresource pop end end".to_vec()));
    let unicode = source.add_object(Stream::new(dictionary! {}, b"/CIDInit /ProcSet findresource begin
        12 dict begin begincmap /CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def
        /CMapName /GeometryUnicode def /CMapType 2 def
        1 begincodespacerange <0000> <ffff> endcodespacerange
        2 beginbfchar <0000> <005a> <0041> <0041> endbfchar
        endcmap CMapName currentdict /CMap defineresource pop end end".to_vec()));
    let font = source.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type0", "BaseFont" => "Helvetica",
        "DescendantFonts" => vec![descendant.into()], "Encoding" => encoding, "ToUnicode" => unicode,
    });
    let contents = source.add_object(Stream::new(
        dictionary! {},
        b"BT /F1 20 Tf 1 0 0 1 10 80 Tm <00000041> Tj ET".to_vec(),
    ));
    let page = source.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), PAGE_SIZE_PTS.into(), PAGE_SIZE_PTS.into()],
        "Resources" => dictionary! {"Font" => dictionary! {"F1" => font}}, "Contents" => contents,
    });
    source.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Count" => 1, "Kids" => vec![page.into()],
        }),
    );
    let catalog = source.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages_id});
    source.trailer.set("Root", catalog);
    source.save(&path).unwrap();
    let actual = invoke(json!({"version":1,"operation":"geometry","pdf_path":path,
        "geometry":{"no_raster":true}}));
    let spans = actual[0]["textSpans"].as_array().unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["text"], "ZA");
    assert_eq!(spans[0]["x0"], 10.0);
    assert_eq!(spans[0]["x1"], 30.0);
    assert_eq!(spans[0]["y0"], 5.0);
    assert_eq!(spans[0]["y1"], 25.0);

    let base_encoding = source.add_object(source.get_object(encoding).unwrap().clone());
    source.objects.insert(
        encoding,
        Object::Stream(Stream::new(
            dictionary! {"UseCMap" => base_encoding},
            b"/CIDInit /ProcSet findresource begin 12 dict begin begincmap
        /CMapName /GeometryInheritedCodes def /CMapType 1 def /WMode 0 def
        2 begincidchar <0000> 1 <0041> 2 endcidchar
        endcmap CMapName currentdict /CMap defineresource pop end end"
                .to_vec(),
        )),
    );
    source.save(&path).unwrap();
    let inherited = invoke(json!({"version":1,"operation":"geometry","pdf_path":path,
        "geometry":{"no_raster":true}}));
    assert_eq!(
        inherited, actual,
        "inherited CMap boundaries preserve zero codes and glyph positions"
    );
}

#[test]
fn geometry_streams_artifact_and_preserves_transform_and_raster_controls() {
    let path = format!(
        "{}/tests/fixtures/rotated_user_unit.pdf",
        env!("CARGO_MANIFEST_DIR")
    );
    let directory = tempfile::tempdir().unwrap();
    let artifact = directory.path().join("geometry.json");
    let request = json!({"version":1,"operation":"geometry","pdf_path":path,
        "page_numbers":[1],"geometry":{"no_raster":true,"relations":true,
        "artifact_out":artifact,"preview":{"max_segments":1,"no_text":true}}});
    let first = invoke(request.clone());
    let full: Value = serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    assert_eq!(first[0]["widthPts"], 200.0);
    assert_eq!(first[0]["heightPts"], 400.0);
    assert_eq!(first[0]["source"], "vector");
    assert_eq!(full[0]["relationsAnalyzed"], true);
    assert!(first[0]["segments"].as_array().unwrap().len() <= 1);
    assert!(first[0].get("textSpans").is_none());
    assert_eq!(
        invoke(request),
        first,
        "repeated extraction must be deterministic"
    );
    let traced = invoke(json!({"version":1,"operation":"geometry","pdf_path":path,
        "page_number":1,"geometry":{"force_raster":true,"raster_dpi":72,
        "artifact_out":artifact}}));
    assert_eq!(traced[0]["source"], "raster-traced");
    let traced_full: Value = serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    assert!(traced_full[0]["paths"].is_null());
    assert!(traced_full[0]["polylines"].is_null());
    assert!(traced_full[0].get("relationsAnalyzed").is_none());
    let skipped = invoke(json!({"version":1,"operation":"geometry","pdf_path":path,
        "geometry":{"force_raster":true,"no_raster":true}}));
    assert_eq!(skipped[0]["source"], "vector");
}

#[test]
fn geometry_errors_leave_stdout_empty_and_preserve_previous_artifact() {
    let directory = tempfile::tempdir().unwrap();
    let artifact = directory.path().join("geometry.json");
    let previous = b"previous artifact";
    std::fs::write(&artifact, previous).unwrap();
    for changes in [
        json!({"page_numbers":[0]}),
        json!({"page_numbers":[999]}),
        json!({"page_numbers":[1],"page_number":1}),
        json!({"geometry":{"preview":{"bbox_frac":[0,0,2,1]}}}),
        json!({"geometry":{"raster_dpi":0}}),
        json!({"geometry":{"force_raster":true,"raster_dpi":1000000000}}),
        json!({"geometry":{"artifact_out":directory.path().join("missing/geometry.json")}}),
        json!({"geometry":{"artifact_out":fixture()}}),
    ] {
        let mut request = json!({"version":1,"operation":"geometry","pdf_path":fixture(),
            "geometry":{"artifact_out":artifact}});
        for (key, value) in changes.as_object().unwrap() {
            request[key] = value.clone();
        }
        let output = invoke_output(request);
        assert!(!output.status.success(), "{changes}");
        assert!(output.stdout.is_empty(), "{changes}");
        assert_eq!(std::fs::read(&artifact).unwrap(), previous);
    }
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

fn image_size(image: &Value) -> (u32, u32) {
    let encoded = image["data"].as_str().unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    image::load_from_memory(&bytes).unwrap().dimensions()
}

#[test]
fn runs_every_pdf_operation_against_a_real_masked_pdf() {
    let path = fixture();
    let base = json!({"version":1,"pdf_path":path});
    let mut request = base.clone();
    request["operation"] = json!("page_count");
    assert_eq!(invoke(request)["total_pages"], 1);

    let mut request = base.clone();
    request["operation"] = json!("page_info");
    request["page_number"] = json!(1);
    let info = invoke(request);
    assert_eq!(info["page_info"]["width_pt"], 200.0);
    assert_eq!(info["page_info"]["height_pt"], 100.0);

    let mut request = base.clone();
    request["operation"] = json!("render_pages");
    request["page_numbers"] = json!([99, 1]);
    request["dpi"] = json!(72);
    let pages = invoke(request);
    assert_eq!(pages["images"].as_array().unwrap().len(), 1);
    assert_eq!(image_size(&pages["images"][0]), (200, 100));

    for operation in ["render_crop", "native_clip", "render_scale"] {
        let mut request = base.clone();
        request["operation"] = json!(operation);
        request["page_number"] = json!(1);
        request["dpi"] = json!(72);
        if operation != "render_scale" {
            request["rect"] = json!([0.25, 0.2, 0.5, 0.6]);
        }
        let output = invoke(request);
        let expected = if operation == "render_scale" {
            (200, 100)
        } else {
            (100, 60)
        };
        assert_eq!(image_size(&output["image"]), expected, "{operation}");
        assert_eq!(output["image"]["effective_dpi"], 72);
    }

    let mut request = base;
    request["operation"] = json!("extract_images");
    request["page_number"] = json!(1);
    let output = invoke(request);
    assert_eq!(output["images"].as_array().unwrap().len(), 1);
    assert_eq!(image_size(&output["images"][0]), (20, 12));
    assert_eq!(output["images"][0]["has_alpha"], true);
    assert_eq!(output["images"][0]["mime_type"], "image/png");
}

#[test]
fn extracts_unfiltered_soft_masks_with_identical_pixels_and_bounds() {
    let source = fixture();
    let reference = invoke(json!({
        "version": 1, "operation": "extract_images", "pdf_path": source, "page_number": 1,
    }));
    let mut document = lopdf::Document::load(&source).unwrap();
    let masks: Vec<_> = document
        .objects
        .values()
        .filter_map(|object| {
            object
                .as_stream()
                .ok()?
                .dict
                .get(b"SMask")
                .ok()?
                .as_reference()
                .ok()
        })
        .collect();
    assert!(!masks.is_empty());
    for reference in masks {
        let mask = document
            .get_object_mut(reference)
            .unwrap()
            .as_stream_mut()
            .unwrap();
        assert!(mask.dict.has(b"Filter"));
        let alpha = mask.decompressed_content().unwrap();
        mask.set_plain_content(alpha);
        assert!(!mask.dict.has(b"Filter"));
    }
    let directory = tempfile::tempdir().unwrap();
    let unfiltered = directory.path().join("unfiltered-soft-mask.pdf");
    document.save(&unfiltered).unwrap();
    let actual = invoke(json!({
        "version": 1, "operation": "extract_images", "pdf_path": unfiltered, "page_number": 1,
    }));
    assert_eq!(actual, reference);
}

#[test]
fn rejects_invalid_single_page_and_fraction_requests() {
    for request in [
        json!({"version":1,"operation":"page_info","pdf_path":fixture(),"page_number":0}),
        json!({"version":1,"operation":"native_clip","pdf_path":fixture(),"page_number":1,"rect":[0.9,0.0,0.2,1.0]}),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lit"))
            .arg("pdf")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
        assert!(!child.wait().unwrap().success());
    }
}

#[test]
fn extracts_source_image_inside_a_transformed_form() {
    let path = format!(
        "{}/tests/fixtures/form_image.pdf",
        env!("CARGO_MANIFEST_DIR")
    );
    let output =
        invoke(json!({"version":1,"operation":"extract_images","pdf_path":path,"page_number":1}));
    let image = &output["images"][0];
    assert_eq!(output["images"].as_array().unwrap().len(), 1);
    assert_eq!(image_size(image), (10, 10));
    assert_eq!(image["x"], 0.2);
    assert_eq!(image["y"], 0.5);
    assert_eq!(image["width"], 0.2);
    assert_eq!(image["height"], 0.2);
}

#[test]
fn renders_rotated_user_unit_page_and_allocates_only_the_clip() {
    let path = format!(
        "{}/tests/fixtures/rotated_user_unit.pdf",
        env!("CARGO_MANIFEST_DIR")
    );
    let info = invoke(json!({"version":1,"operation":"page_info","pdf_path":path,"page_number":1}));
    assert_eq!(info["page_info"]["width_pt"], 200.0);
    assert_eq!(info["page_info"]["height_pt"], 400.0);

    let full = invoke(
        json!({"version":1,"operation":"render_scale","pdf_path":path,"page_number":1,"dpi":150}),
    );
    assert_eq!(image_size(&full["image"]), (417, 834));
    let clip = invoke(
        json!({"version":1,"operation":"native_clip","pdf_path":path,"page_number":1,"dpi":150,"rect":[0.1,0.1,0.7,0.7]}),
    );
    assert_eq!(image_size(&clip["image"]), (293, 584));
}

#[test]
fn render_excludes_annotation_appearances_but_keeps_page_contents() {
    const ANNOTATION_EDGE_X: u32 = 84;
    const ANNOTATION_EDGE_Y: u32 = 64;
    let path = format!(
        "{}/tests/fixtures/annotation.pdf",
        env!("CARGO_MANIFEST_DIR")
    );
    let output = invoke(
        json!({"version":1,"operation":"render_scale","pdf_path":path,"page_number":1,"dpi":150}),
    );
    let encoded = output["image"]["data"].as_str().unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    let image = image::load_from_memory(&bytes).unwrap().to_rgb8();
    let edge = image.get_pixel(ANNOTATION_EDGE_X, ANNOTATION_EDGE_Y);
    assert!(edge.0.iter().all(|channel| *channel > 230), "{edge:?}");
}

#[test]
fn geometry_preserves_vertical_glyphs_and_following_horizontal_cursor() {
    const TEXT_POSITION_TOLERANCE_PTS: f64 = 0.02;
    let path = format!(
        "{}/tests/fixtures/vertical_cursor.pdf",
        env!("CARGO_MANIFEST_DIR")
    );
    let directory = tempfile::tempdir().unwrap();
    let artifact = directory.path().join("vertical.json");
    let request = json!({"version":1,"operation":"geometry","pdf_path":path,
        "geometry":{"no_raster":true,"artifact_out":artifact}});
    let preview = invoke(request.clone());
    assert_eq!(
        invoke(request),
        preview,
        "vertical text output is deterministic"
    );
    let actual: Value = serde_json::from_slice(&std::fs::read(artifact).unwrap()).unwrap();
    let expected: Value =
        serde_json::from_str(include_str!("fixtures/vertical_cursor.geometry.json")).unwrap();
    let actual_spans = actual[0]["textSpans"].as_array().unwrap();
    let expected_spans = expected[0]["textSpans"].as_array().unwrap();
    assert_eq!(actual_spans.len(), expected_spans.len());
    assert_eq!(actual[0]["textSpans"], preview[0]["textSpans"]);
    for (index, (new, old)) in actual_spans.iter().zip(expected_spans).enumerate() {
        for field in ["text", "fontName"] {
            assert_eq!(new[field], old[field], "span {index} {field}");
        }
        for field in ["fontSize", "rotation"] {
            assert_eq!(
                new[field].as_f64(),
                old[field].as_f64(),
                "span {index} {field}"
            );
        }
        for field in ["x0", "y0", "x1", "y1"] {
            assert!(
                (new[field].as_f64().unwrap() - old[field].as_f64().unwrap()).abs()
                    <= TEXT_POSITION_TOLERANCE_PTS,
                "span {index} {field}: {} versus {}",
                new[field],
                old[field]
            );
        }
    }
}

#[test]
fn geometry_counts_filtered_inline_images_and_preserves_following_paths() {
    use flate2::{Compression, write::ZlibEncoder};
    use lopdf::{Document, Object, Stream, dictionary};

    const PAGE_SIZE_PTS: i64 = 200;
    const EXPECTED_IMAGE_COVERAGE: f64 = 0.005;
    const COVERAGE_TOLERANCE: f64 = 0.000001;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("inline-image.pdf");
    let artifact = directory.path().join("geometry.json");
    let mut source = Document::with_version("1.7");
    let pages_id = source.new_object_id();
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::none());
    encoder.write_all(b" EI BI ID Q RGB").unwrap();
    let mut content = b"q 10 0 0 20 30 40 cm BI /CS/R17 /W 5 /H 1 /BPC 8 /F/Fl ID ".to_vec();
    content.extend_from_slice(&encoder.finish().unwrap());
    content.extend_from_slice(b"\nEI Q 0 0 m 100 100 l S");
    let contents_id = source.add_object(Stream::new(dictionary! {}, content));
    let page_id = source.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), PAGE_SIZE_PTS.into(), PAGE_SIZE_PTS.into()],
        "Contents" => contents_id,
        "Resources" => dictionary! {"ColorSpace" => dictionary! {"R17" => "DeviceRGB"}},
    });
    source.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    let catalog_id = source.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages_id});
    source.trailer.set("Root", catalog_id);
    source.save(&path).unwrap();
    invoke(json!({"version":1,"operation":"geometry","pdf_path":path,
        "geometry":{"artifact_out":artifact,"no_raster":true}}));
    let pages: Value = serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    assert_eq!(pages[0]["imageOps"], 1);
    assert!(
        (pages[0]["imageCoverage"].as_f64().unwrap() - EXPECTED_IMAGE_COVERAGE).abs()
            < COVERAGE_TOLERANCE
    );
    assert_eq!(pages[0]["pathOps"], 1);
    assert_eq!(pages[0]["segments"].as_array().unwrap().len(), 1);
    let rendered = invoke(
        json!({"version":1,"operation":"render_pages","pdf_path":path,
        "page_numbers":[1],"dpi":72}),
    );
    assert_eq!(rendered["images"].as_array().unwrap().len(), 1);
}

#[test]
fn geometry_patterns_use_parent_stream_coordinates_on_pages_and_forms() {
    use lopdf::{Document, Object, Stream, dictionary};

    const PAGE_SIZE_PTS: i64 = 200;
    const EXPECTED_IMAGE_COVERAGE: f64 = 0.05;
    const COVERAGE_TOLERANCE: f64 = 0.000001;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pattern-coordinates.pdf");
    let artifact = directory.path().join("geometry.json");
    let mut source = Document::with_version("1.7");
    let pages_id = source.new_object_id();
    let image_id = source.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Image", "Width" => 1, "Height" => 1,
            "ColorSpace" => "DeviceRGB", "BitsPerComponent" => 8,
        },
        vec![255, 0, 0],
    ));
    let pattern_id = source.add_object(Stream::new(
        dictionary! {
            "Type" => "Pattern", "PatternType" => 1, "PaintType" => 1, "TilingType" => 1,
            "BBox" => vec![0.into(), 0.into(), 20.into(), 20.into()], "XStep" => 20, "YStep" => 20,
            "Matrix" => vec![1.into(), 0.into(), 0.into(), 1.into(), 20.into(), 20.into()],
            "Resources" => dictionary! {"XObject" => dictionary! {"Im" => image_id}},
        },
        b"q 20 0 0 20 0 0 cm /Im Do Q".to_vec(),
    ));
    let paint = b"q 0.5 0 0 0.5 0 0 cm /Pattern cs /P scn 40 40 40 40 re f Q";
    let form_id = source.add_object(Stream::new(
        dictionary! {
            "Type" => "XObject", "Subtype" => "Form",
            "BBox" => vec![0.into(), 0.into(), 100.into(), 100.into()],
            "Matrix" => vec![2.into(), 0.into(), 0.into(), 2.into(), 100.into(), 50.into()],
            "Resources" => dictionary! {"Pattern" => dictionary! {"P" => pattern_id}},
        },
        paint.to_vec(),
    ));
    let mut content = paint.to_vec();
    content.extend_from_slice(b" /Form Do");
    let contents_id = source.add_object(Stream::new(dictionary! {}, content));
    let page_id = source.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), PAGE_SIZE_PTS.into(), PAGE_SIZE_PTS.into()],
        "Contents" => contents_id,
        "Resources" => dictionary! {"Pattern" => dictionary! {"P" => pattern_id},
            "XObject" => dictionary! {"Form" => form_id}},
    });
    source.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    let catalog_id = source.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages_id});
    source.trailer.set("Root", catalog_id);
    source.save(&path).unwrap();
    invoke(json!({"version":1,"operation":"geometry","pdf_path":path,
        "geometry":{"artifact_out":artifact,"no_raster":true}}));
    let pages: Value = serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    assert_eq!(pages[0]["imageOps"], 2);
    assert!(
        (pages[0]["imageCoverage"].as_f64().unwrap() - EXPECTED_IMAGE_COVERAGE).abs()
            < COVERAGE_TOLERANCE,
        "coverage {}",
        pages[0]["imageCoverage"]
    );
}

#[test]
fn geometry_restores_font_names_from_each_paint_resource() {
    use lopdf::{Document, Object, Stream, dictionary};

    const PAGE_SIZE_PTS: i64 = 200;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("font-resources.pdf");
    let artifact = directory.path().join("geometry.json");
    let mut source = Document::with_version("1.7");
    let pages_id = source.new_object_id();
    let first_font = source.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "AAAAAA+Helvetica",
    });
    let second_font = source.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "BBBBBB+Helvetica",
    });
    let contents_id = source.add_object(Stream::new(
        dictionary! {},
        b"BT /F1 12 Tf 1 0 0 1 20 150 Tm (FIRST) Tj /F2 12 Tf 1 0 0 1 20 100 Tm (SECOND) Tj ET"
            .to_vec(),
    ));
    let page_id = source.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), PAGE_SIZE_PTS.into(), PAGE_SIZE_PTS.into()],
        "Contents" => contents_id,
        "Resources" => dictionary! {"Font" => dictionary! {"F1" => first_font, "F2" => second_font}},
    });
    source.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    let catalog_id = source.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages_id});
    source.trailer.set("Root", catalog_id);
    source.save(&path).unwrap();
    invoke(json!({"version":1,"operation":"geometry","pdf_path":path,
        "geometry":{"artifact_out":artifact,"no_raster":true}}));
    let pages: Value = serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    let spans = pages[0]["textSpans"].as_array().unwrap();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0]["text"], "FIRST");
    assert_eq!(spans[0]["fontName"], "AAAAAA+Helvetica");
    assert_eq!(spans[1]["text"], "SECOND");
    assert_eq!(spans[1]["fontName"], "BBBBBB+Helvetica");
}

#[test]
fn geometry_suppresses_consecutive_glyphs_and_preserves_repeated_words() {
    use lopdf::{Document, Object, Stream, dictionary};

    const PAGE_SIZE_PTS: i64 = 200;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("repeated-paints.pdf");
    let artifact = directory.path().join("geometry.json");
    let mut source = Document::with_version("1.7");
    let pages_id = source.new_object_id();
    let font_id = source.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let contents_id = source.add_object(Stream::new(dictionary! {},
        b"BT /F1 12 Tf 1 0 0 1 20 150 Tm (A) Tj 1 0 0 1 20 150 Tm (A) Tj 1 0 0 1 20 100 Tm (0.820) Tj 1 0 0 1 20 100 Tm (0.820) Tj 1 0 0 1 20 100 Tm (0.820) Tj ET".to_vec()));
    let page_id = source.add_object(dictionary! {
        "Type" => "Page", "Parent" => pages_id,
        "MediaBox" => vec![0.into(), 0.into(), PAGE_SIZE_PTS.into(), PAGE_SIZE_PTS.into()],
        "Contents" => contents_id,
        "Resources" => dictionary! {"Font" => dictionary! {"F1" => font_id}},
    });
    source.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => vec![page_id.into()], "Count" => 1,
        }),
    );
    let catalog_id = source.add_object(dictionary! {"Type" => "Catalog", "Pages" => pages_id});
    source.trailer.set("Root", catalog_id);
    source.save(&path).unwrap();
    invoke(json!({"version":1,"operation":"geometry","pdf_path":path,
        "geometry":{"artifact_out":artifact,"no_raster":true}}));
    let pages: Value = serde_json::from_slice(&std::fs::read(&artifact).unwrap()).unwrap();
    let spans = pages[0]["textSpans"].as_array().unwrap();
    assert_eq!(
        spans
            .iter()
            .map(|span| span["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["A", "0.820", "0.820", "0.820"]
    );
    assert_eq!(spans[1], spans[2]);
    assert_eq!(spans[2], spans[3]);
}

#[test]
fn editable_measurements_preserve_ui_edges_and_invalid_boundary_numbers() {
    let (directory, path) = measurement_fixture();
    let measured = invoke(json!({"version":1,"operation":"measure","pdf_path":path,
        "page_number":1,"measurement":{"mode":"area","validate_geometry":false,
        "points":[[0.1,0.2],[0.3,0.2],[0.3,0.6],[0.1,0.6],[0.1,0.2]],
        "scale_denominator":100}}));
    assert_eq!(
        measured["measurement"]["edge_lengths_paper_mm"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    assert_close(&measured["measurement"]["edge_lengths_paper_mm"][4], 0.0);
    assert_close(&measured["measurement"]["paper_length_mm"], 203.2);
    assert_close(&measured["measurement"]["paper_area_mm2"], 2580.64);
    let measured = invoke(json!({"version":1,"operation":"measure","pdf_path":path,
        "page_number":1,"measurement":{"mode":"perimeter","validate_geometry":false,
        "points":[[0.1,0.2],[0.3,0.6],[0.3,0.2],[0.1,0.6]],"scale_denominator":100}}));
    assert_close(&measured["measurement"]["paper_area_mm2"], 0.0);
    assert!(measured["measurement"]["paper_length_mm"].as_f64().unwrap() > 0.0);
    let measured = invoke(json!({"version":1,"operation":"measure","pdf_path":path,
        "page_number":1,"measurement":{"mode":"area","validate_geometry":false,
        "points":[[0.1,0.2],[0.3,0.2],[0.3,0.6],[0.1,0.6]],
        "holes":[[[0.0,0.0],[1.0,0.0],[1.0,1.0],[0.0,1.0]]],"scale_denominator":100}}));
    assert_close(&measured["measurement"]["paper_area_mm2"], 0.0);
    std::fs::remove_dir_all(directory).unwrap();
}
