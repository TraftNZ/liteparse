//! Synthetic PDFs keep clipping regressions independent of customer documents.
use liteparse::{RawTextItem, extract_raw_text_items, stages};
use pdfium::Library;

fn pdf(content: &str, forms: &[&str], rotation: i32) -> Vec<u8> {
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 600 800] /Rotate {rotation} /Resources << /Font << /F1 4 0 R >> /XObject << /Outer 6 0 R >> /ExtGState << /Clear << /ca 0 >> /Half << /ca 0.5 >> /Faint << /ca 0.02 >> >> >> /Contents 5 0 R >>"
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Courier >>".to_string(),
        stream("", content),
    ];
    objects.extend(forms.iter().map(|s| s.to_string()));
    let mut data = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(data.len());
        data.extend_from_slice(format!("{} 0 obj\n{object}\nendobj\n", i + 1).as_bytes());
    }
    let xref = data.len();
    data.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        data.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    data.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    data
}

fn stream(dict: &str, content: &str) -> String {
    format!(
        "<< {dict} /Length {} >>\nstream\n{content}\nendstream",
        content.len()
    )
}

fn extract(bytes: &[u8]) -> (Vec<RawTextItem>, String) {
    let lib = Library::init();
    let doc = lib.load_document_from_bytes(bytes, None).unwrap();
    let page = doc.page(0).unwrap();
    let text = page.text().unwrap();
    let raw = extract_raw_text_items(&page, &text, &page.view_box().unwrap(), None);
    let standard = stages::extract(&doc, &stages::ExtractRequest::default()).unwrap();
    let standard = standard.pages[0]
        .text_items
        .iter()
        .map(|i| i.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    (raw, standard)
}

fn assert_text(bytes: &[u8], expected: &str) {
    let (raw, standard) = extract(bytes);
    let raw: String = raw.iter().map(|i| i.text.as_str()).collect();
    // PDFium inserts separators between source objects. Compare real characters.
    let compact = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
    assert_eq!(compact(&raw), compact(expected), "raw: {raw:?}");
    assert_eq!(
        compact(&standard),
        compact(expected),
        "standard: {standard:?}"
    );
}

#[test]
fn clipped_overflow_does_not_leak_into_next_row() {
    let content = "q 100 200 70 8 re W* n BT /F1 6 Tf 105 201 Td (VISIBLE) Tj 0 -7 Td (42) Tj ET Q BT /F1 6 Tf 105 194 Td (NEXT) Tj ET";
    assert_text(&pdf(content, &[], 0), "VISIBLE NEXT");
}

#[test]
fn horizontal_partial_glyphs_survive_in_full_at_every_page_rotation() {
    // Courier advances 6 pt, so C's advance starts at x = 22 and its ink a little after.
    // A clip edge at 22 touches the advance but shows none of C's ink.
    for rotation in [0, 90, 180, 270] {
        for (edge, expected) in [
            (26, "ABC"),
            (24, "ABC"),
            (23, "ABC"),
            (22, "AB"),
            (21, "AB"),
        ] {
            let content = format!(
                "q 10 10 {} 30 re W n BT /F1 10 Tf 10 20 Td (ABCDE) Tj ET Q",
                edge - 10
            );
            assert_text(&pdf(&content, &[], rotation), expected);
        }
    }
}

#[test]
fn nested_forms_compose_transforms_and_inherit_outer_clips() {
    let inner = stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << /Font << /F1 4 0 R >> >>",
        "0 0 25 80 re W n BT /F1 10 Tf 5 20 Td (ABCDE) Tj ET",
    );
    let outer = stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 200 200] /Resources << /XObject << /Inner 7 0 R >> >>",
        "2 0 0 2 10 15 cm /Inner Do",
    );
    let content = "q 100 100 35 300 re W n 1 0 0 1 100 100 cm /Outer Do Q";
    assert_text(&pdf(content, &[&outer, &inner], 0), "AB");
}

#[test]
fn form_bbox_clips_text_without_explicit_clip_operator() {
    let form = stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 17 100] /Resources << /Font << /F1 4 0 R >> >>",
        "BT /F1 10 Tf 0 20 Td (ABCDE) Tj ET",
    );
    assert_text(&pdf("1 0 0 1 100 100 cm /Outer Do", &[&form], 0), "ABC");
}

#[test]
fn stacked_clips_intersect_and_restore_does_not_leak() {
    let content = "q 0 0 100 100 re W n 10 10 14 30 re W* n BT /F1 10 Tf 10 20 Td (ABCDE) Tj ET Q BT /F1 10 Tf 100 100 Td (RESTORED) Tj ET";
    assert_text(&pdf(content, &[], 0), "ABC RESTORED");
}

#[test]
fn unsupported_clips_preserve_source_text() {
    for path in [
        "0 0 m 1 0 1 1 0 1 c h",             // curve
        "0 0 m 1 0 l 1 1 l 5 5 1 1 1 1 c h", // curve among lines
    ] {
        let content = format!("q {path} W* n BT /F1 10 Tf 10 20 Td (KEEP) Tj ET Q");
        assert_text(&pdf(&content, &[], 0), "KEEP");
    }
}

#[test]
fn text_outside_a_polygon_clip_is_hidden_and_text_inside_is_kept() {
    // A triangle over the lower left; one string sits inside it, one far outside.
    let content = "q 0 0 m 300 0 l 0 300 l h W* n BT /F1 10 Tf 20 20 Td (INSIDE) Tj ET BT /F1 10 Tf 400 400 Td (OUTSIDE) Tj ET Q";
    assert_text(&pdf(content, &[], 0), "INSIDE");
}

/// A chain of forms, each clipping through its own triangle-like polygon of
/// hundreds of vertices before drawing the next, the last drawing the text.
fn nested_polygon_forms(depth: usize) -> Vec<String> {
    const POINTS: usize = 450;
    const STEP: f64 = 0.5;
    const BASE: f64 = 500.0;
    const FIRST_FORM_OBJECT: usize = 6;
    (0..depth)
        .map(|level| {
            let side = BASE - level as f64 * 0.25;
            let mut content = format!("0 0 m {side} 0 l ");
            for i in 0..POINTS {
                let t = i as f64 * STEP;
                content.push_str(&format!("{} {} l ", side - t, t));
            }
            content.push_str("h W* n ");
            if level + 1 == depth {
                content.push_str(
                    "BT /F1 10 Tf 20 20 Td (INSIDE) Tj ET BT /F1 10 Tf 400 400 Td (OUTSIDE) Tj ET",
                );
            } else {
                content.push_str("/Next Do");
            }
            stream(
                &format!(
                    "/Type /XObject /Subtype /Form /BBox [0 0 600 800] /Resources << /Font << /F1 4 0 R >> /XObject << /Next {} 0 R >> >>",
                    FIRST_FORM_OBJECT + level + 1
                ),
                &content,
            )
        })
        .collect()
}

fn assert_nested_polygon_forms(depth: usize, expected: &str) {
    let forms = nested_polygon_forms(depth);
    let forms: Vec<&str> = forms.iter().map(String::as_str).collect();
    assert_text(&pdf("/Outer Do", &forms, 0), expected);
}

#[test]
fn nested_polygon_clips_within_budget_still_hide_outside_text() {
    assert_nested_polygon_forms(5, "INSIDE");
}

#[test]
fn nested_polygon_clips_past_the_vertex_budget_fail_open_and_keep_text() {
    assert_nested_polygon_forms(40, "INSIDEOUTSIDE");
}

#[test]
fn text_in_the_notch_of_a_concave_polygon_clip_is_hidden() {
    // An L: the square 100..200 square is the notch, outside the outline.
    let content = "q 0 0 m 200 0 l 200 100 l 100 100 l 100 200 l 0 200 l h W n BT /F1 10 Tf 20 20 Td (ARM) Tj ET BT /F1 10 Tf 130 130 Td (NOTCH) Tj ET Q";
    assert_text(&pdf(content, &[], 0), "ARM");
}

#[test]
fn text_in_the_hole_of_a_compound_clip_is_hidden_when_both_fill_rules_agree() {
    // The hole winds against the outline, so even-odd and non-zero both leave it empty.
    let content = "q 0 0 m 300 0 l 300 300 l 0 300 l h 100 100 m 100 200 l 200 200 l 200 100 l h W* n BT /F1 10 Tf 20 20 Td (RING) Tj ET BT /F1 10 Tf 130 140 Td (HOLE) Tj ET Q";
    assert_text(&pdf(content, &[], 0), "RING");
}

#[test]
fn text_in_a_hole_the_fill_rules_disagree_on_is_kept() {
    // Same winding: even-odd leaves the hole empty, non-zero fills it, and the
    // file's rule is not reported, so the text stays.
    let content = "q 0 0 m 300 0 l 300 300 l 0 300 l h 100 100 m 200 100 l 200 200 l 100 200 l h W* n BT /F1 10 Tf 130 140 Td (HOLE) Tj ET Q";
    assert_text(&pdf(content, &[], 0), "HOLE");
}

#[test]
fn a_rotated_square_clip_hides_text_beyond_its_corners() {
    let content = "q 150 100 m 200 150 l 150 200 l 100 150 l h W n BT /F1 10 Tf 140 148 Td (MID) Tj ET BT /F1 10 Tf 95 95 Td (CORNER) Tj ET Q";
    assert_text(&pdf(content, &[], 0), "MID");
}

#[test]
fn unclipped_raw_items_are_identical_with_redundant_clip() {
    let text = "BT /F1 10 Tf 10 20 Td (UNCHANGED WORDS) Tj ET";
    let plain = extract(&pdf(text, &[], 0));
    let clipped = extract(&pdf(&format!("q 0 0 600 800 re W n {text} Q"), &[], 0));
    assert_eq!(plain, clipped);
}

#[test]
fn invisible_ocr_text_without_path_clips_is_preserved() {
    assert_text(
        &pdf("BT /F1 10 Tf 3 Tr 10 20 Td (OCR) Tj ET", &[], 0),
        "OCR",
    );
}

#[test]
fn disjoint_clips_hide_all_glyphs() {
    assert_text(
        &pdf(
            "q 0 0 5 5 re W n 10 10 5 5 re W n BT /F1 10 Tf 10 20 Td (HIDDEN) Tj ET Q",
            &[],
            0,
        ),
        "",
    );
}

#[test]
fn content_matrix_is_not_applied_to_clip_twice() {
    assert_text(
        &pdf(
            "q 2 0 0 2 100 100 cm 0 0 14 40 re W n BT /F1 10 Tf 0 20 Td (ABCDE) Tj ET Q",
            &[],
            0,
        ),
        "ABC",
    );
}

#[test]
fn unsupported_parent_clip_preserves_form_text() {
    let form = stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 100 100] /Resources << /Font << /F1 4 0 R >> >>",
        "BT /F1 10 Tf 10 20 Td (KEEP) Tj ET",
    );
    assert_text(
        &pdf("0 0 m 1 0 1 1 0 1 c h W n /Outer Do", &[&form], 0),
        "KEEP",
    );
}

/// Left, right, bottom and top of a box in y-up page space.
type Edges = (f32, f32, f32, f32);

/// The glyph's ink box (its strict box, `grounding_bounds`) in y-up page space, with
/// its loose box (advance width by font ascent and descent), as (left, right, bottom, top).
fn glyph_boxes(item: &RawTextItem) -> (Edges, Edges) {
    // Raw item coordinates have a top-left origin; content streams use y-up.
    let ink = item.grounding_bounds.unwrap();
    let ink = (ink.left, ink.right, 800.0 - ink.bottom, 800.0 - ink.top);
    let top = 800.0 - item.y;
    let loose = (item.x, item.x + item.width, top - item.height, top);
    (ink, loose)
}

#[test]
fn partial_glyphs_keep_the_full_text_and_geometry_at_all_clip_edges() {
    let text = "BT /F1 10 Tf 10 20 Td (A) Tj ET";
    let original = extract(&pdf(text, &[], 0));
    let glyph = original.0.iter().find(|item| item.text == "A").unwrap();
    // A clip that shows 0.1 pt of the glyph's ink at any edge keeps the whole glyph.
    let ((left, right, bottom, top), _) = glyph_boxes(glyph);
    let (width, height) = (right - left, top - bottom);
    let clips = [
        (left - 1.0, bottom - 1.0, 1.1, height + 2.0),
        (right - 0.1, bottom - 1.0, 1.1, height + 2.0),
        (left - 1.0, bottom - 1.0, width + 2.0, 1.1),
        (left - 1.0, top - 0.1, width + 2.0, 1.1),
    ];
    for (x, y, width, height) in clips {
        let content = format!("q {x} {y} {width} {height} re W n {text} Q");
        assert_eq!(extract(&pdf(&content, &[], 0)), original);
    }
}

#[test]
fn glyphs_whose_ink_lies_outside_the_clip_are_hidden_even_when_their_loose_box_grazes_it() {
    // The loose box carries side bearings, ascent and descent, so a clip edge can touch
    // it while no ink is visible. Each clip reaches 0.05 pt into the loose box only.
    let graze = 0.05;
    for (text, vertical) in [("A", true), (".", false)] {
        let content = format!("BT /F1 10 Tf 10 20 Td ({text}) Tj ET");
        let original = extract(&pdf(&content, &[], 0));
        let glyph = original.0.iter().find(|item| item.text == text).unwrap();
        let ((ink_left, ink_right, ink_bottom, ink_top), (left, right, bottom, top)) =
            glyph_boxes(glyph);
        let (width, height) = (right - left, top - bottom);
        let clips = if vertical {
            // "A" stands on the baseline and stops below the ascent.
            assert!(ink_bottom > bottom + 1.0 && ink_top < top - 1.0);
            [
                (left - 1.0, bottom - 1.0, width + 2.0, 1.0 + graze),
                (left - 1.0, top - graze, width + 2.0, 1.0 + graze),
            ]
        } else {
            // "." is narrow ink in the middle of a 6 pt advance.
            assert!(ink_left > left + 1.0 && ink_right < right - 1.0);
            [
                (left - 1.0, bottom - 1.0, 1.0 + graze, height + 2.0),
                (right - graze, bottom - 1.0, 1.0 + graze, height + 2.0),
            ]
        };
        for (x, y, width, height) in clips {
            let content = format!("q {x} {y} {width} {height} re W n {content} Q");
            assert_text(&pdf(&content, &[], 0), "");
        }
    }
}

#[test]
fn ink_just_outside_the_clip_is_kept_within_a_small_share_of_the_em() {
    // PDFium's ink box can understate the painted outline by about 1% of the font
    // size, so a clip edge 0.1 pt past the ink of a 10 pt glyph still shows it.
    let text = "BT /F1 10 Tf 10 20 Td (A) Tj ET";
    let original = extract(&pdf(text, &[], 0));
    let glyph = original.0.iter().find(|item| item.text == "A").unwrap();
    let ((left, right, _, top), _) = glyph_boxes(glyph);
    let content = format!(
        "q {} {} {} 5 re W n {text} Q",
        left - 1.0,
        top + 0.1,
        right - left + 2.0
    );
    assert_eq!(extract(&pdf(&content, &[], 0)), original);
    let content = format!(
        "q {} {} {} 5 re W n {text} Q",
        left - 1.0,
        top + 1.0,
        right - left + 2.0
    );
    assert_text(&pdf(&content, &[], 0), "");
}

const GHOST_FORM_RESOURCES: &str = "/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Resources << /Font << /F1 4 0 R >> /XObject << /Inner 7 0 R >> >>";

fn ghost_text_form() -> String {
    stream(GHOST_FORM_RESOURCES, "BT /F1 10 Tf 5 20 Td (GHOST) Tj ET")
}

#[test]
fn text_in_a_form_drawn_at_zero_alpha_is_hidden() {
    let content = "q /Clear gs /Outer Do Q BT /F1 10 Tf 100 100 Td (SHOWN) Tj ET";
    assert_text(&pdf(content, &[&ghost_text_form()], 0), "SHOWN");
}

#[test]
fn text_in_a_form_nested_below_a_zero_alpha_form_is_hidden() {
    let inner = stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Resources << /Font << /F1 4 0 R >> >>",
        "BT /F1 10 Tf 5 20 Td (DEEP) Tj ET",
    );
    let outer = stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Resources << /XObject << /Inner 7 0 R >> >>",
        "/Inner Do",
    );
    let content = "q /Clear gs /Outer Do Q BT /F1 10 Tf 100 100 Td (SHOWN) Tj ET";
    assert_text(&pdf(content, &[&outer, &inner], 0), "SHOWN");
}

#[test]
fn text_in_a_translucent_form_is_kept() {
    let content = "q /Half gs /Outer Do Q";
    assert_text(&pdf(content, &[&ghost_text_form()], 0), "GHOST");
}

#[test]
fn zero_alpha_form_under_an_unreadable_clip_still_hides_its_text() {
    let content =
        "q 0 0 m 1 0 1 1 0 1 c h W n /Clear gs /Outer Do Q BT /F1 10 Tf 100 100 Td (SHOWN) Tj ET";
    assert_text(&pdf(content, &[&ghost_text_form()], 0), "SHOWN");
}

#[test]
fn text_in_a_form_at_two_percent_alpha_is_hidden() {
    let content = "q /Faint gs /Outer Do Q BT /F1 10 Tf 100 100 Td (SHOWN) Tj ET";
    assert_text(&pdf(content, &[&ghost_text_form()], 0), "SHOWN");
}

#[test]
fn nested_translucent_forms_hide_text_when_their_alphas_multiply_below_visible() {
    let inner = stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Resources << /Font << /F1 4 0 R >> >>",
        "BT /F1 10 Tf 5 20 Td (DEEP) Tj ET",
    );
    let outer = stream(
        "/Type /XObject /Subtype /Form /BBox [0 0 300 300] /Resources << /XObject << /Inner 7 0 R >> /ExtGState << /Tenth << /ca 0.08 >> >> >>",
        "q /Tenth gs /Inner Do Q",
    );
    let content = "q /Half gs /Outer Do Q BT /F1 10 Tf 100 100 Td (SHOWN) Tj ET";
    assert_text(&pdf(content, &[&outer, &inner], 0), "SHOWN");
}
