use std::path::PathBuf;

use liteparse_geometry::content_paths::capture_page_content;
use lopdf::{Document, Object};

#[test]
fn original_alice_structural_pages_keep_native_paths_and_raster_coverage() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../../../internal/evalharness/testdata/alice-street/P3302 - 136 Alice Street PSE_Certified_B.pdf",
    );
    let document = Document::load(&source).unwrap();
    let image = document.get_object((1455, 0)).unwrap();
    assert!(
        matches!(image, Object::Dictionary(_)),
        "original producer padding must reproduce lopdf dictionary-only decoding"
    );
    assert_eq!(
        image
            .as_dict()
            .unwrap()
            .get(b"Subtype")
            .unwrap()
            .as_name()
            .unwrap(),
        b"Image"
    );
    assert_eq!(document.get_pages().len(), 7);
    let expected_paths = [552, 3110, 2851, 3196, 1508, 579, 792];
    let expected_images = [11, 11, 16, 11, 11, 28, 11];
    for number in 1..=7 {
        let page = capture_page_content(&source, number)
            .unwrap_or_else(|error| panic!("page {number}: {error}"));
        assert_eq!(
            page.paths.paths.len(),
            expected_paths[number as usize - 1],
            "page {number} native paths"
        );
        assert_eq!(
            page.image_ops,
            expected_images[number as usize - 1],
            "page {number} raster operations"
        );
        assert!(
            page.image_coverage > 0.0 && page.image_coverage <= 1.0,
            "page {number} must retain raster coverage"
        );
        println!(
            "page {number}: paths={} image_ops={} image_coverage={}",
            page.paths.paths.len(),
            page.image_ops,
            page.image_coverage
        );
    }
}

#[test]
fn original_alice_architectural_pages_keep_native_paths() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(
        "../../../../internal/evalharness/testdata/alice-street/ALICE ST BP PLANS_260609b.pdf",
    );
    assert_eq!(Document::load(&source).unwrap().get_pages().len(), 7);
    for number in 1..=7 {
        let page = capture_page_content(&source, number)
            .unwrap_or_else(|error| panic!("architectural page {number}: {error}"));
        assert!(
            !page.paths.paths.is_empty(),
            "architectural page {number} lost native paths"
        );
        assert!(page.width_pts > 0.0 && page.height_pts > 0.0);
    }
}
