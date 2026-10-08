use std::process::Command;

use lopdf::{Document, Object, Stream, dictionary};
use serde_json::Value;

fn bookmarked_fixture(path: &std::path::Path) {
    let mut document = Document::with_version("1.7");
    let pages = document.new_object_id();
    let outlines = document.new_object_id();
    let font = document.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let titles = ["Chapter One", "Chapter Two", "Chapter Three"];
    let mut page_ids = Vec::new();
    for title in titles {
        let stream = document.add_object(Stream::new(
            dictionary! {},
            format!("BT /F1 18 Tf 72 700 Td ({title}) Tj ET").into_bytes(),
        ));
        let page = document.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            "Contents" => stream,
        });
        page_ids.push(page);
    }
    document.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages", "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
            "Count" => page_ids.len() as i64,
        }),
    );
    let item_ids: Vec<_> = titles.iter().map(|_| document.new_object_id()).collect();
    for (index, title) in titles.iter().enumerate() {
        let mut item = dictionary! {
            "Title" => Object::string_literal(*title), "Parent" => outlines,
            "Dest" => vec![Object::Reference(page_ids[index]), Object::Name(b"Fit".to_vec())],
        };
        if index > 0 {
            item.set("Prev", item_ids[index - 1]);
        }
        if index + 1 < item_ids.len() {
            item.set("Next", item_ids[index + 1]);
        }
        document
            .objects
            .insert(item_ids[index], Object::Dictionary(item));
    }
    document.objects.insert(
        outlines,
        Object::Dictionary(dictionary! {
            "Type" => "Outlines", "First" => item_ids[0],
            "Last" => item_ids[2], "Count" => item_ids.len() as i64,
        }),
    );
    let catalog = document.add_object(dictionary! {
        "Type" => "Catalog", "Pages" => pages, "Outlines" => outlines,
    });
    document.trailer.set("Root", catalog);
    document.save(path).expect("save bookmarked PDF fixture");
}

fn parse(path: &std::path::Path, emit_markdown: bool) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lit"));
    command.args(["parse", "--format", "json", "--no-ocr", "-q"]);
    if emit_markdown {
        command.arg("--emit-markdown");
    }
    let output = command.arg(path).output().expect("run lit parse");
    assert!(
        output.status.success(),
        "lit parse: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON parse result")
}

#[test]
fn emit_markdown_preserves_page_text_and_exports_bookmarks() {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let path = directory.path().join("bookmarked.pdf");
    bookmarked_fixture(&path);
    let plain = parse(&path, false);
    let markdown = parse(&path, true);
    let plain_pages = plain["pages"].as_array().expect("plain pages");
    let markdown_pages = markdown["pages"].as_array().expect("Markdown pages");
    assert_eq!(plain_pages.len(), 3);
    assert_eq!(markdown_pages.len(), 3);
    for (index, (plain_page, markdown_page)) in plain_pages.iter().zip(markdown_pages).enumerate() {
        assert_eq!(plain_page["text"], markdown_page["text"]);
        assert!(plain_page.get("markdown").is_none());
        let page_markdown = markdown_page["markdown"].as_str().expect("page Markdown");
        let expected = format!("# Chapter {}", ["One", "Two", "Three"][index]);
        assert!(page_markdown.lines().any(|line| line == expected));
    }
    assert_eq!(markdown["structure_version"], "2");
    let outline = markdown["outline"].as_array().expect("document outline");
    assert_eq!(outline.len(), 3);
    assert_eq!(outline[0]["title"], "Chapter One");
    assert_eq!(outline[1]["page_index"], 1);
    assert_eq!(outline[2]["title"], "Chapter Three");
}

/// Document pages that carry a running header and footer on every page, plus
/// a note printed on the first `NOTE_PAGES` pages only.
const RUNNING_PAGES: usize = 12;
const NOTE_PAGES: usize = 2;
const RUNNING_HEADER: &str = "Acme Holdings Limited";
const RUNNING_FOOTER: &str = "Confidential Issue Sheet";
const SHORT_NOTE: &str = "Temporary Site Note";
/// Pages per `lit parse` batch; deliberately leaves a 2-page final batch.
const BATCH_PAGES: usize = 5;

fn running_chrome_fixture(path: &std::path::Path) {
    let mut document = Document::with_version("1.7");
    let pages = document.new_object_id();
    let font = document.add_object(dictionary! {
        "Type" => "Font", "Subtype" => "Type1", "BaseFont" => "Helvetica",
    });
    let mut page_ids = Vec::new();
    for number in 1..=RUNNING_PAGES {
        let note = if number <= NOTE_PAGES {
            format!("BT /F1 10 Tf 72 500 Td ({SHORT_NOTE}) Tj ET ")
        } else {
            String::new()
        };
        let content = format!(
            "BT /F1 10 Tf 72 760 Td ({RUNNING_HEADER}) Tj ET \
             BT /F1 10 Tf 72 400 Td (Body paragraph number {number} with unique wording {}) Tj ET \
             {note}BT /F1 10 Tf 72 30 Td ({RUNNING_FOOTER}) Tj ET",
            number * 7919
        );
        let stream = document.add_object(Stream::new(dictionary! {}, content.into_bytes()));
        page_ids.push(document.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages,
            "MediaBox" => vec![0.into(), 0.into(), 612.into(), 792.into()],
            "Resources" => dictionary! { "Font" => dictionary! { "F1" => font } },
            "Contents" => stream,
        }));
    }
    document.objects.insert(
        pages,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => page_ids.iter().copied().map(Object::Reference).collect::<Vec<_>>(),
            "Count" => page_ids.len() as i64,
        }),
    );
    let catalog = document.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages });
    document.trailer.set("Root", catalog);
    document
        .save(path)
        .expect("save running chrome PDF fixture");
}

/// Header/footer repetition is a whole-document signal: pages parsed in
/// separate batches and structured together lose the running chrome on every
/// page, including the short final batch, while text repeated on only
/// `NOTE_PAGES` pages survives.
#[test]
fn structure_removes_running_chrome_across_parse_batches() {
    let directory = tempfile::tempdir().expect("temporary fixture directory");
    let pdf = directory.path().join("running.pdf");
    running_chrome_fixture(&pdf);

    let mut command = Command::new(env!("CARGO_BIN_EXE_lit"));
    command.arg("structure").arg(&pdf);
    for (index, first) in (1..=RUNNING_PAGES).step_by(BATCH_PAGES).enumerate() {
        let last = (first + BATCH_PAGES - 1).min(RUNNING_PAGES);
        let pages_file = directory.path().join(format!("batch_{index}.json"));
        let batch = Command::new(env!("CARGO_BIN_EXE_lit"))
            .args([
                "parse",
                "--format",
                "json",
                "--no-ocr",
                "-q",
                "--target-pages",
            ])
            .arg(format!("{first}-{last}"))
            .arg("--emit-parsed-pages")
            .arg(&pages_file)
            .arg(&pdf)
            .output()
            .expect("run lit parse batch");
        assert!(
            batch.status.success(),
            "lit parse: {}",
            String::from_utf8_lossy(&batch.stderr)
        );
        command.arg("--pages-file").arg(&pages_file);
    }
    let output = command.output().expect("run lit structure");
    assert!(
        output.status.success(),
        "lit structure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let structured: Value = serde_json::from_slice(&output.stdout).expect("structure JSON");
    assert_eq!(structured["structure_version"], "2");
    let pages = structured["pages"].as_array().expect("structured pages");
    assert_eq!(pages.len(), RUNNING_PAGES);
    for (index, page) in pages.iter().enumerate() {
        assert_eq!(page["page"], index + 1);
        let markdown = page["markdown"].as_str().expect("page Markdown");
        assert!(
            !markdown.contains(RUNNING_HEADER),
            "page {}: {markdown}",
            index + 1
        );
        assert!(
            !markdown.contains(RUNNING_FOOTER),
            "page {}: {markdown}",
            index + 1
        );
        assert!(markdown.contains(&format!("Body paragraph number {}", index + 1)));
        assert_eq!(
            markdown.contains(SHORT_NOTE),
            index < NOTE_PAGES,
            "page {}: {markdown}",
            index + 1
        );
    }
}
