//! Whole-document heading normalization for liteparse page Markdown.

use std::collections::{HashMap, HashSet};

pub const STRUCTURE_VERSION: &str = "1";
const MAX_HEADING_LEVEL: usize = 6;
const MIN_TRUSTED_BOOKMARKS: usize = 3;
const MIN_SPEC_SECTION_CODES: usize = 3;
const MIN_CONSECUTIVE_RUNNING_HEADING_PAGES: usize = 3;
const MIN_BODY_SENTENCE_CHARS: usize = 100;
const MIN_COMMA_HEAVY_HEADING_CHARS: usize = 110;
const MIN_BODY_DESCRIPTION_COMMAS: usize = 2;
const MIN_DRAWING_LANDSCAPE_RATIO: f32 = 1.2;
const COUNTER_TEMPLATE_SHARE_NUMERATOR: usize = 4;
const COUNTER_TEMPLATE_SHARE_DENOMINATOR: usize = 5;
const DRAWING_SHEET_MARKER_AT_A1: &str = "@ a1";
const DRAWING_SHEET_MARKER_SCALE_1_100: &str = "1 : 100";
const DRAWING_SHEET_MARKER_RESOURCE_CONSENT_PLAN: &str = "resource consent plan";
/// Standards-body abbreviations that commonly precede a citation number
/// (e.g. "Timber to NZS 3602"). A heading ending in one of these is a
/// standards citation, not a running page-number footer, even though its
/// last word is numeric.
const STANDARDS_ABBREVIATIONS: &[&str] = &["NZS", "AS", "BS", "ISO", "EN", "AS/NZS", "BS/EN"];

#[derive(Debug, Clone)]
pub struct Bookmark {
    pub level: u8,
    pub title: String,
    pub page_index: i32,
    pub y_pdf: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct PositionedLine {
    pub text: String,
    pub y_top: f32,
}

#[derive(Debug, Clone)]
pub struct PagePosition {
    pub width: f32,
    pub height: f32,
    pub lines: Vec<PositionedLine>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub changed_levels: usize,
    pub demoted_chrome: usize,
    pub grafted_bookmarks: usize,
}

fn looks_like_drawing_sheet(page: &str, position: &PagePosition) -> bool {
    if position.width <= position.height * MIN_DRAWING_LANDSCAPE_RATIO {
        return false;
    }
    let text = page.to_ascii_lowercase();
    text.contains("drawing")
        && (text.contains(DRAWING_SHEET_MARKER_AT_A1)
            || text.contains(DRAWING_SHEET_MARKER_SCALE_1_100)
            || text.contains(DRAWING_SHEET_MARKER_RESOURCE_CONSENT_PLAN))
}

/// Normalize heading lines across all pages. Non-heading lines are preserved
/// exactly, including table cells and OCR text.
///
/// `page_numbers` gives the real, whole-document page index (matching
/// `Bookmark::page_index`) for each entry in `pages`/`positions`, since
/// `pages` may be a subset of the document (e.g. `--target-pages` or an OCR
/// batch). `total_pages` is the real page count of the whole document, used
/// to bound plausibility checks (e.g. trailing page numbers in headings).
pub fn normalize_pages(
    pages: &mut [String],
    bookmarks: &[Bookmark],
    repeated_headers: &HashSet<String>,
    positions: &[PagePosition],
    page_numbers: &[i32],
    total_pages: usize,
) -> Report {
    promote_numbered_lines(pages, positions);
    let mut repeated_headers = repeated_headers.clone();
    repeated_headers.extend(locally_repeated_headings(pages));
    let bookmarks: Vec<Bookmark> = bookmarks
        .iter()
        .filter(|bookmark| {
            let title = bookmark.title.trim();
            !is_caption_heading(title)
                && !is_amendment_instruction_heading(title)
                && !is_body_sentence_heading(title)
                && !is_numbered_list_sentence(title)
                && !is_counter_only_heading(title)
                && !is_drawing_page_note(title)
                && !is_priced_schedule_heading(title)
        })
        .cloned()
        .collect();
    let trusted = trustworthy_bookmarks(&bookmarks, total_pages);
    let mut report = Report::default();
    let mut seen_bookmarks = HashSet::new();
    let mut bookmark_levels: HashMap<(i32, String), usize> = HashMap::new();
    if trusted {
        for bookmark in &bookmarks {
            bookmark_levels.insert(
                (bookmark.page_index, title_key(&bookmark.title)),
                usize::from(bookmark.level).clamp(1, MAX_HEADING_LEVEL),
            );
        }
    }
    let doc_page = |local_index: usize| -> i32 {
        page_numbers
            .get(local_index)
            .copied()
            .unwrap_or(local_index as i32)
    };

    let mut style_votes: HashMap<usize, HashMap<usize, usize>> = HashMap::new();
    let uses_spec_codes = pages
        .iter()
        .flat_map(|page| page.lines())
        .filter_map(parse_heading)
        .filter(|(_, title)| is_spec_section_code(title.trim()))
        .count()
        >= MIN_SPEC_SECTION_CODES;
    let mut in_spec_section = false;
    for (local_index, page) in pages.iter().enumerate() {
        if looks_like_contents_page(page)
            || positions
                .get(local_index)
                .is_some_and(|position| looks_like_drawing_sheet(page, position))
        {
            continue;
        }
        let page_index = doc_page(local_index);
        for line in page.lines() {
            let Some((old_level, title)) = parse_heading(line) else {
                continue;
            };
            let title = title.trim();
            if uses_spec_codes && is_spec_section_code(title) {
                in_spec_section = true;
            }
            if (is_running_header(title, &repeated_headers)
                && !bookmark_levels.contains_key(&(page_index, title_key(title))))
                || is_page_number_heading(title, total_pages)
                || is_counter_only_heading(title)
                || is_drawing_page_note(title)
                || is_numbered_list_sentence(title)
                || is_caption_heading(title)
                || is_log_observation_heading(title)
                || is_contact_heading(title)
                || is_amendment_instruction_heading(title)
                || is_body_sentence_heading(title)
                || is_priced_schedule_heading(title)
            {
                continue;
            }
            let explicit = bookmark_levels
                .get(&(page_index, title_key(title)))
                .copied()
                .or_else(|| numbered_level_in_context(title, uses_spec_codes, in_spec_section));
            if let Some(level) = explicit {
                *style_votes
                    .entry(old_level)
                    .or_default()
                    .entry(level.clamp(1, MAX_HEADING_LEVEL))
                    .or_default() += 1;
            }
        }
    }
    let mut style_levels: HashMap<usize, usize> = style_votes
        .into_iter()
        .filter_map(|(style, votes)| {
            votes
                .into_iter()
                .max_by_key(|(level, count)| (*count, std::cmp::Reverse(*level)))
                .map(|(level, _)| (style, level))
        })
        .collect();
    let mut previous_level: usize = 0;
    in_spec_section = false;

    for (local_index, page) in pages.iter_mut().enumerate() {
        let page_index = doc_page(local_index);
        let contents_page = looks_like_contents_page(page);
        let drawing_sheet = positions
            .get(local_index)
            .is_some_and(|position| looks_like_drawing_sheet(page, position));
        let mut lines = Vec::new();
        let source_lines: Vec<&str> = page.split('\n').collect();
        for (line_index, line) in source_lines.iter().enumerate() {
            let Some((old_level, title)) = parse_heading(line) else {
                lines.push((*line).to_owned());
                continue;
            };
            let title = title.trim();
            let key = title_key(title);
            let bookmarked = bookmark_levels.contains_key(&(page_index, key.clone()));
            if uses_spec_codes && is_spec_section_code(title) {
                in_spec_section = true;
            }
            // Specific lexical/structural content classifiers take priority
            // over the generic cross-page repetition heuristic: a phrase
            // that legitimately repeats because it is common document
            // content (e.g. "Add new clause:" opening several amendments)
            // is demoted to a plain line like any other non-heading text,
            // not silently dropped as page chrome.
            if title.is_empty()
                || is_page_number_heading(title, total_pages)
                || is_counter_only_heading(title)
                || is_drawing_page_note(title)
                || is_numbered_list_sentence(title)
                || is_caption_heading(title)
                || is_log_observation_heading(title)
                || is_contact_heading(title)
                || is_amendment_instruction_heading(title)
                || is_body_sentence_heading(title)
                || is_priced_schedule_heading(title)
                || (drawing_sheet && !bookmarked)
                || (numbered_level_in_context(title, uses_spec_codes, in_spec_section).is_none()
                    && !bookmarked
                    && !title.eq_ignore_ascii_case("contents")
                    && !is_uppercase_section_title(title)
                    && is_table_context_heading(&source_lines, line_index))
                || (contents_page && !title.eq_ignore_ascii_case("contents"))
            {
                lines.push(title.to_owned());
                report.demoted_chrome += 1;
                continue;
            }
            if is_running_header(title, &repeated_headers) && !bookmarked {
                // Not otherwise-classified boilerplate that repeats
                // page-to-page (e.g. a document/section title running
                // header or footer): drop it entirely rather than leaking
                // it into section text as a plain line.
                report.demoted_chrome += 1;
                continue;
            }
            let bookmark_level = bookmark_levels.get(&(page_index, key.clone())).copied();
            if bookmark_level.is_some() {
                seen_bookmarks.insert((page_index, key));
            }
            let new_level = if let Some(explicit) = bookmark_level
                .or_else(|| numbered_level_in_context(title, uses_spec_codes, in_spec_section))
            {
                explicit.clamp(1, MAX_HEADING_LEVEL)
            } else if old_level == 1 {
                1
            } else {
                *style_levels
                    .entry(old_level)
                    .or_insert_with(|| old_level.min(previous_level.saturating_add(1).max(1)))
            };
            previous_level = new_level;
            if new_level != old_level {
                report.changed_levels += 1;
            }
            lines.push(format!("{} {}", "#".repeat(new_level), title));
        }
        *page = lines.join("\n");
    }

    if trusted {
        for bookmark in bookmarks.iter().rev() {
            let key = (bookmark.page_index, title_key(&bookmark.title));
            if seen_bookmarks.contains(&key) {
                continue;
            }
            let Some(local_index) = page_numbers
                .iter()
                .position(|&doc_index| doc_index == bookmark.page_index)
            else {
                // The bookmarked page is outside this batch (e.g. a
                // --target-pages/OCR subset); nothing to graft onto.
                continue;
            };
            let heading = format!(
                "{} {}\n\n",
                "#".repeat(usize::from(bookmark.level).clamp(1, MAX_HEADING_LEVEL)),
                bookmark.title.trim()
            );
            let page = &mut pages[local_index];
            if has_heading_for_bookmark(page, &bookmark.title) {
                continue;
            }
            if let Some((start, end)) = find_standalone_title(page, &bookmark.title) {
                page.replace_range(start..end, &heading);
            } else {
                let offset = child_heading_offset(page, &bookmark.title)
                    .or_else(|| {
                        positions.get(local_index).and_then(|position| {
                            bookmark_insertion_offset(page, bookmark, position)
                        })
                    })
                    .unwrap_or(0);
                let prefix = if offset > 0 && !page[..offset].ends_with('\n') {
                    "\n\n"
                } else {
                    ""
                };
                page.insert_str(offset, &format!("{prefix}{heading}"));
            }
            report.grafted_bookmarks += 1;
        }
    }
    report
}

fn promote_numbered_lines(pages: &mut [String], positions: &[PagePosition]) {
    for (page_index, page) in pages.iter_mut().enumerate() {
        if looks_like_contents_page(page) {
            continue;
        }
        let Some(position) = positions.get(page_index) else {
            continue;
        };
        let mut seen = HashSet::new();
        let mut candidates = Vec::new();
        for line in &position.lines {
            let title = line.text.trim();
            if is_physical_numbered_heading(title) && seen.insert(title) {
                candidates.push(title);
            }
        }
        if candidates.is_empty() {
            continue;
        }
        let mut promoted = HashSet::new();
        let mut output = Vec::new();
        for markdown_line in page.split('\n') {
            if parse_heading(markdown_line).is_some() || markdown_line.trim_start().starts_with('|')
            {
                output.push(markdown_line.to_owned());
                continue;
            }
            let mut remainder = markdown_line;
            loop {
                let matched = candidates
                    .iter()
                    .filter(|title| !promoted.contains(*title))
                    .filter_map(|title| {
                        let offset = remainder.find(title)?;
                        let before = &remainder[..offset];
                        let after = &remainder[offset + title.len()..];
                        (before.trim().is_empty() || after.trim().is_empty())
                            .then_some((*title, offset))
                    })
                    .min_by_key(|(title, offset)| (*offset, std::cmp::Reverse(title.len())));
                let Some((title, offset)) = matched else {
                    output.push(remainder.to_owned());
                    break;
                };
                let before = &remainder[..offset];
                let after = &remainder[offset + title.len()..];
                if !before.trim().is_empty() {
                    output.push(before.trim_end().to_owned());
                    output.push(String::new());
                }
                output.push(format!("# {title}"));
                promoted.insert(title);
                if after.trim().is_empty() {
                    break;
                }
                output.push(String::new());
                remainder = after.trim_start();
            }
        }
        if !promoted.is_empty() {
            *page = output.join("\n");
        }
    }
}

fn is_physical_numbered_heading(title: &str) -> bool {
    const MAX_PHYSICAL_HEADING_CHARS: usize = 100;
    const MAX_PHYSICAL_HEADING_WORDS: usize = 12;
    let mut words = title.split_whitespace();
    let Some(number) = words.next() else {
        return false;
    };
    let Some(first_title_word) = words.next() else {
        return false;
    };
    number.as_bytes().first().is_some_and(u8::is_ascii_digit)
        && numbered_level(title).is_some()
        && (number.contains('.') || is_uppercase_section_title(title))
        && title.len() <= MAX_PHYSICAL_HEADING_CHARS
        && title.split_whitespace().count() <= MAX_PHYSICAL_HEADING_WORDS
        && !title.ends_with('.')
        && first_title_word
            .chars()
            .next()
            .is_some_and(char::is_uppercase)
}

fn has_heading_for_bookmark(markdown: &str, title: &str) -> bool {
    let target = title_key(title);
    target.len() >= 6
        && markdown
            .lines()
            .filter_map(parse_heading)
            .any(|(_, heading)| title_key(heading.trim()).starts_with(&target))
}

fn child_heading_offset(markdown: &str, title: &str) -> Option<usize> {
    numbered_level(title)?;
    let number = title
        .split_whitespace()
        .next()?
        .trim_end_matches(['.', ')', ':']);
    let child_prefix = format!("{number}.");
    markdown_line_spans(markdown).find_map(|(start, _, line)| {
        parse_heading(line)
            .is_some_and(|(_, heading)| heading.trim().starts_with(&child_prefix))
            .then_some(start)
    })
}

fn find_standalone_title(markdown: &str, title: &str) -> Option<(usize, usize)> {
    let target = title_key(title);
    if target.is_empty() {
        return None;
    }
    markdown_line_spans(markdown).find_map(|(start, end, line)| {
        let plain = line.trim().trim_matches(['*', '_', '`']);
        (title_key(plain) == target).then_some((start, end))
    })
}

fn bookmark_insertion_offset(
    markdown: &str,
    bookmark: &Bookmark,
    position: &PagePosition,
) -> Option<usize> {
    let target_top = position.height - bookmark.y_pdf?;
    let mut lines: Vec<_> = position
        .lines
        .iter()
        .filter(|line| line.y_top >= target_top)
        .collect();
    lines.sort_by(|a, b| a.y_top.total_cmp(&b.y_top));
    for line in lines {
        let key = title_key(&line.text);
        if key.len() < 6 {
            continue;
        }
        if let Some((start, _, _)) = markdown_line_spans(markdown)
            .find(|(_, _, candidate)| title_key(candidate).contains(&key))
        {
            return Some(start);
        }
    }
    None
}

fn markdown_line_spans(markdown: &str) -> impl Iterator<Item = (usize, usize, &str)> {
    let mut offset = 0;
    markdown.split_inclusive('\n').map(move |line| {
        let start = offset;
        offset += line.len();
        (start, offset, line.trim_end_matches('\n'))
    })
}

fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let marks = line.bytes().take_while(|byte| *byte == b'#').count();
    if marks == 0 || marks > MAX_HEADING_LEVEL || !line.as_bytes().get(marks)?.is_ascii_whitespace()
    {
        return None;
    }
    Some((marks, &line[marks..]))
}

fn numbered_level(title: &str) -> Option<usize> {
    let lower = title.to_ascii_lowercase();
    for prefix in ["section ", "part ", "appendix "] {
        if lower.starts_with(prefix) {
            return Some(1);
        }
    }
    let mut words = title.split_whitespace();
    let first = words.next()?;
    let number = if first.contains("/AS") || first.contains("/as") {
        words.next()?
    } else {
        first
    };
    let trimmed = number.trim_end_matches(['.', ')', ':']);
    if trimmed.is_empty() || !trimmed.chars().next()?.is_ascii_digit() {
        return None;
    }
    if !trimmed
        .split('.')
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
    {
        return None;
    }
    Some(trimmed.matches('.').count() + 1)
}

fn numbered_level_in_context(
    title: &str,
    uses_spec_codes: bool,
    in_spec_section: bool,
) -> Option<usize> {
    if uses_spec_codes && is_spec_section_code(title) {
        return Some(1);
    }
    let level = numbered_level(title)?;
    if in_spec_section && !is_spec_section_code(title) && has_short_numeric_prefix(title) {
        Some((level + 1).min(MAX_HEADING_LEVEL))
    } else {
        Some(level)
    }
}

fn has_short_numeric_prefix(title: &str) -> bool {
    let Some(first) = title.split_whitespace().next() else {
        return false;
    };
    let token = first.trim_end_matches(['.', ')', ':']);
    let stem = token.split('.').next().unwrap_or_default();
    !stem.is_empty() && stem.len() <= 2 && stem.chars().all(|c| c.is_ascii_digit())
}

fn is_spec_section_code(title: &str) -> bool {
    let mut words = title.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if words.next().is_none() || !(4..=8).contains(&first.len()) {
        return false;
    }
    first
        .as_bytes()
        .get(..4)
        .is_some_and(|prefix| prefix.iter().all(u8::is_ascii_digit))
        && first[4..].chars().all(|c| c.is_ascii_alphanumeric())
}

fn title_key(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_running_header(title: &str, repeated_headers: &HashSet<String>) -> bool {
    repeated_headers.contains(&normalize_repeated_heading(title))
}

fn normalize_repeated_heading(title: &str) -> String {
    normalize_digits(title)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn locally_repeated_headings(pages: &[String]) -> HashSet<String> {
    let mut occurrences: HashMap<String, Vec<usize>> = HashMap::new();
    for (page_index, page) in pages.iter().enumerate() {
        for (_, title) in page.lines().filter_map(parse_heading) {
            let title = title.trim();
            if numbered_level(title).is_some()
                && !title.to_ascii_lowercase().starts_with("section ")
            {
                continue;
            }
            occurrences
                .entry(normalize_repeated_heading(title))
                .or_default()
                .push(page_index);
        }
    }
    occurrences
        .into_iter()
        .filter_map(|(title, mut page_indexes)| {
            page_indexes.sort_unstable();
            page_indexes.dedup();
            let mut consecutive = 1;
            for pair in page_indexes.windows(2) {
                if pair[1] == pair[0] + 1 {
                    consecutive += 1;
                    if consecutive >= MIN_CONSECUTIVE_RUNNING_HEADING_PAGES {
                        return Some(title);
                    }
                } else {
                    consecutive = 1;
                }
            }
            None
        })
        .collect()
}

fn normalize_digits(title: &str) -> String {
    let mut normalized = String::new();
    let mut in_digits = false;
    for c in title.to_lowercase().chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                normalized.push('#');
                in_digits = true;
            }
        } else {
            normalized.push(c);
            in_digits = false;
        }
    }
    normalized
}

/// A running-header/footer line that repeats a chapter title and page
/// number, e.g. "1.5. Decision Theory 45". Must not fire on a standards
/// citation whose trailing token is a designation number, e.g.
/// "3.1 Timber to NZS 3602" or "3.2 Fixings to AS 1397" — those are
/// distinguished by requiring the trailing number to be a plausible page
/// (within the document's page count) and by excluding headings whose
/// trailing number is immediately preceded by a standards-body
/// abbreviation.
fn is_page_number_heading(title: &str, total_pages: usize) -> bool {
    let words: Vec<_> = title.split_whitespace().collect();
    if words.len() < 3 {
        return false;
    }
    let last = words[words.len() - 1];
    if !last.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let Ok(page_number) = last.parse::<usize>() else {
        return false;
    };
    if page_number == 0 || (total_pages > 0 && page_number > total_pages) {
        return false;
    }
    if !words[..words.len() - 1]
        .iter()
        .any(|word| word.chars().any(char::is_alphabetic))
    {
        return false;
    }
    !is_standards_abbreviation(words[words.len() - 2])
}

fn is_standards_abbreviation(word: &str) -> bool {
    let trimmed = word.trim_matches(|c: char| !c.is_alphanumeric());
    STANDARDS_ABBREVIATIONS
        .iter()
        .any(|abbreviation| trimmed.eq_ignore_ascii_case(abbreviation))
}

fn is_counter_only_heading(title: &str) -> bool {
    title.chars().any(|c| c.is_ascii_digit())
        && !title.chars().any(char::is_alphabetic)
        && title
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_whitespace() || c == '.')
}

fn is_drawing_page_note(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    let mut words = lower.split_whitespace();
    let Some(paper_size) = words.next() else {
        return false;
    };
    paper_size.len() > 1
        && paper_size.starts_with('a')
        && paper_size[1..].chars().all(|c| c.is_ascii_digit())
        && words.next() == Some("drawing")
        && words.next() == Some("page")
}

fn is_caption_heading(title: &str) -> bool {
    let lower = title.trim().to_ascii_lowercase();
    lower == "figure"
        || lower == "table"
        || lower.starts_with("figure ")
        || lower.starts_with("table ")
        || lower.starts_with("fig. ")
}

fn is_table_context_heading(lines: &[&str], heading_index: usize) -> bool {
    const TABLE_CONTEXT_LINES: usize = 6;
    let start = heading_index.saturating_sub(TABLE_CONTEXT_LINES);
    let end = (heading_index + TABLE_CONTEXT_LINES + 1).min(lines.len());
    lines[start..end].iter().enumerate().any(|(offset, line)| {
        if start + offset == heading_index {
            return false;
        }
        let trimmed = line.trim();
        (start + offset > heading_index && trimmed.starts_with('|') && trimmed.ends_with('|'))
            || (start + offset < heading_index
                && trimmed.to_ascii_lowercase().starts_with("**table "))
    })
}

fn is_uppercase_section_title(title: &str) -> bool {
    title.split_whitespace().count() >= 2
        && title.chars().any(char::is_alphabetic)
        && title
            .chars()
            .filter(|c| c.is_alphabetic())
            .all(char::is_uppercase)
}

fn is_amendment_instruction_heading(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    lower.starts_with("add ") || lower.starts_with("delete ") || lower.starts_with("replace ")
}

fn is_body_sentence_heading(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    (title.len() >= MIN_BODY_SENTENCE_CHARS && title.trim_end().ends_with('.'))
        || (lower.starts_with("the ") && lower.contains(" shall ") && title.ends_with(':'))
        || (title.len() >= MIN_COMMA_HEAVY_HEADING_CHARS
            && title.matches(',').count() >= MIN_BODY_DESCRIPTION_COMMAS
            && numbered_level(title).is_none())
}

fn is_priced_schedule_heading(title: &str) -> bool {
    numbered_level(title).is_some() && title.contains('$')
}

fn is_log_observation_heading(title: &str) -> bool {
    let trimmed = title.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ' ');
    trimmed.starts_with('@')
}

fn is_contact_heading(title: &str) -> bool {
    let lower = title.to_ascii_lowercase();
    lower.contains("www.") || lower.contains("http://") || lower.contains("https://")
}

fn is_numbered_list_sentence(title: &str) -> bool {
    let mut words = title.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    let number = first.trim_end_matches(['.', ')']);
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let rest: Vec<_> = words.collect();
    let lower = title.to_ascii_lowercase();
    rest.is_empty()
        || (rest.len() >= 6
            && (title.ends_with(|c: char| matches!(c, '.' | ';' | ':'))
                || lower.contains(" will ")
                || lower.contains(" shall ")))
}

fn looks_like_contents_page(markdown: &str) -> bool {
    let lower = markdown.to_ascii_lowercase();
    let leader_entries = markdown
        .lines()
        .filter(|line| {
            let trimmed = line.trim().trim_end_matches('*').trim();
            trimmed.contains("...") && trimmed.chars().last().is_some_and(|c| c.is_ascii_digit())
        })
        .count();
    leader_entries >= MIN_TRUSTED_BOOKMARKS
        || (lower.contains("contents") && (leader_entries > 0 || markdown.contains("|---")))
}

fn trustworthy_bookmarks(bookmarks: &[Bookmark], page_count: usize) -> bool {
    if bookmarks.len() < MIN_TRUSTED_BOOKMARKS {
        return false;
    }
    let mut previous = -1;
    for bookmark in bookmarks {
        if bookmark.page_index < previous
            || bookmark.page_index < 0
            || bookmark.page_index as usize >= page_count
        {
            return false;
        }
        previous = bookmark.page_index;
        let lower = bookmark.title.to_ascii_lowercase();
        if lower.starts_with("page ") || bookmark.title.trim().chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }
    let mut templates: HashMap<String, usize> = HashMap::new();
    for bookmark in bookmarks {
        *templates
            .entry(normalize_digits(&title_key(&bookmark.title)))
            .or_default() += 1;
    }
    if templates.values().any(|count| {
        *count * COUNTER_TEMPLATE_SHARE_DENOMINATOR
            >= bookmarks.len() * COUNTER_TEMPLATE_SHARE_NUMERATOR
    }) {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test helper matching the pre-item-2 call shape: pages are assumed to
    /// be the whole document in order, so real page numbers equal their
    /// position and the total page count equals `pages.len()`.
    fn normalize(
        pages: &mut [String],
        bookmarks: &[Bookmark],
        repeated_headers: &HashSet<String>,
        positions: &[PagePosition],
    ) -> Report {
        let page_numbers: Vec<i32> = (0..pages.len() as i32).collect();
        let total_pages = pages.len();
        normalize_pages(
            pages,
            bookmarks,
            repeated_headers,
            positions,
            &page_numbers,
            total_pages,
        )
    }

    #[test]
    fn numbering_overrides_inconsistent_font_levels() {
        let mut pages = vec!["## 1 Scope\n\n# 1.1 Work\n\n### 1.2 Materials".into()];
        let report = normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert_eq!(pages[0], "# 1 Scope\n\n## 1.1 Work\n\n## 1.2 Materials");
        assert_eq!(report.changed_levels, 3);
    }

    #[test]
    fn drawing_sheet_labels_are_not_sections() {
        let drawing = "## WALL KEY E\n\n## Office 1\n\n## 1 Site Plan\n\nDrawing No. A1 @ A1";
        let mut pages = vec![
            drawing.to_owned(),
            "## Office 2\n\nDrawing No. A2 @ A1".into(),
        ];
        let positions = vec![
            PagePosition {
                width: 1190.0,
                height: 842.0,
                lines: vec![],
            },
            PagePosition {
                width: 595.0,
                height: 842.0,
                lines: vec![],
            },
        ];
        normalize(&mut pages, &[], &HashSet::new(), &positions);
        assert!(!pages[0].lines().any(|line| line.starts_with('#')));
        assert!(
            pages[1]
                .lines()
                .any(|line| line.starts_with('#') && line.ends_with("Office 2"))
        );
    }

    #[test]
    fn counter_fragments_are_not_headings() {
        let mut pages = vec!["# 4. 5.\n\n# 6. 7.\n\n## 6.7 Drainage".into()];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert_eq!(pages[0], "4. 5.\n\n6. 7.\n\n## 6.7 Drainage");
    }

    #[test]
    fn unnumbered_root_title_keeps_root_level() {
        let mut pages = vec!["# GENERAL INFORMATION\n\n### 2.1 Scope\n\n# 4. 5.".into()];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(pages[0].starts_with("# GENERAL INFORMATION"));
        assert!(pages[0].ends_with("4. 5."));
    }

    #[test]
    fn paper_size_drawing_note_is_not_a_section() {
        let mut pages = vec!["## A4 drawing page - figure 8.19\n\n# 8.7.5 Holes in plates".into()];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(pages[0].starts_with("A4 drawing page - figure 8.19"));
        assert!(
            pages[0]
                .lines()
                .any(|line| line.starts_with('#') && line.ends_with("Holes in plates"))
        );
    }

    #[test]
    fn physical_numbered_titles_are_promoted_out_of_paragraphs() {
        let mut pages = vec![
            "# 2 EXCAVATION AND EARTHWORKS\n\n2.1 Introduction\n\nBody text. 2.2 Relevant Documents\n\nDocuments referred to here."
                .into(),
        ];
        let positions = vec![PagePosition {
            width: 600.0,
            height: 800.0,
            lines: vec![
                PositionedLine {
                    text: "2.1 Introduction".into(),
                    y_top: 100.0,
                },
                PositionedLine {
                    text: "2.2 Relevant Documents".into(),
                    y_top: 400.0,
                },
            ],
        }];
        normalize(&mut pages, &[], &HashSet::new(), &positions);
        assert!(pages[0].contains("## 2.1 Introduction"));
        assert!(pages[0].contains("Body text.\n\n## 2.2 Relevant Documents"));
    }

    #[test]
    fn physical_numbered_sentences_are_not_promoted() {
        let mut pages: Vec<String> =
            vec!["1. Areas and dimensions are subject to final survey.".into()];
        let positions = vec![PagePosition {
            width: 600.0,
            height: 800.0,
            lines: vec![PositionedLine {
                text: pages[0].clone(),
                y_top: 100.0,
            }],
        }];
        normalize(&mut pages, &[], &HashSet::new(), &positions);
        assert_eq!(
            pages[0],
            "1. Areas and dimensions are subject to final survey."
        );
    }

    #[test]
    fn numbered_street_address_is_not_promoted() {
        assert!(!is_physical_numbered_heading(
            "10 Andrew Baxter Drive, Mangere, Auckland, New Zealand"
        ));
        assert!(is_physical_numbered_heading("1 PRELIMINARY AND GENERAL"));
    }

    #[test]
    fn running_page_title_is_demoted_without_losing_text() {
        // This single page is a snippet from a ~50-page book (the PRML
        // running-header regression case): the trailing "45" must still
        // read as a plausible page number even though only one page is
        // passed in, so total_pages must reflect the real document size,
        // not this snippet's length.
        const BOOK_TOTAL_PAGES: usize = 300;
        let mut pages = vec!["# 1.5. Decision Theory 45\nBody".into()];
        normalize_pages(
            &mut pages,
            &[],
            &HashSet::new(),
            &[],
            &[0],
            BOOK_TOTAL_PAGES,
        );
        assert_eq!(pages[0], "1.5. Decision Theory 45\nBody");
    }

    #[test]
    fn standards_citation_with_trailing_designation_number_is_not_a_heading_running_header() {
        // "## 3.1 Timber to NZS 3602" / "## 3.2 Fixings to AS 1397": the
        // trailing token is a standard's designation number, not a page
        // number, and must not be demoted by the running-header/page-number
        // heuristic even though the heading has a numbered clause prefix.
        let mut pages = vec![
            "# 3 Materials\n## 3.1 Timber to NZS 3602\nBody\n## 3.2 Fixings to AS 1397\nMore body"
                .into(),
        ];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(pages[0].contains("## 3.1 Timber to NZS 3602"));
        assert!(pages[0].contains("## 3.2 Fixings to AS 1397"));
    }

    #[test]
    fn section_local_running_headers_are_dropped() {
        let mut pages = vec![
            "# SECTION 1 - WALLS NZS 3604:2011\n# 1 Walls".into(),
            "# SECTION 1 - WALLS NZS 3604:2011\n# 1.1 Scope".into(),
            "# SECTION 1 - WALLS NZS 3604:2011\n# 1.2 Materials".into(),
            "# 2 Floors".into(),
        ];
        let report = normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(!pages.iter().any(|page| page.contains("SECTION 1 - WALLS")));
        assert_eq!(pages[0], "# 1 Walls");
        assert_eq!(pages[1], "## 1.1 Scope");
        assert_eq!(pages[2], "## 1.2 Materials");
        assert_eq!(pages[3], "# 2 Floors");
        assert_eq!(report.demoted_chrome, 3);
    }

    #[test]
    fn repeated_short_clause_titles_remain_headings() {
        let mut pages = vec![
            "# 1 GENERAL".into(),
            "# 1 GENERAL".into(),
            "# 1 GENERAL".into(),
        ];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(pages.iter().all(|page| page == "# 1 GENERAL"));
    }

    #[test]
    fn repeated_amendment_instructions_are_text() {
        let mut pages = vec![
            "# Add new clause:".into(),
            "# Add new clause:".into(),
            "# Add new clause:".into(),
        ];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(pages.iter().all(|page| page == "Add new clause:"));
    }

    #[test]
    fn tender_amendment_sentence_is_not_a_heading() {
        let mut pages = vec!["# Delete the old wording and replace with the time specified".into()];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert_eq!(
            pages[0],
            "Delete the old wording and replace with the time specified"
        );
        assert!(is_amendment_instruction_heading("Add to Clause 5.2.3:"));
        assert!(is_amendment_instruction_heading("Delete clause"));
        assert!(is_numbered_list_sentence(
            "1. Crushing resistance can be as low as 120kN instead of 130kN;"
        ));
        assert!(is_numbered_list_sentence(
            "3. The contractor will notify the Area Archaeologist of Heritage New"
        ));
        assert!(is_body_sentence_heading(
            "The retention money shall become payable to the Contractor:"
        ));
        assert!(is_body_sentence_heading(
            "Granular Fill [AUCKLAND VOLCANIC FIELD] CLAY with some silt, orangey brown to brown mottled light grey, wet, very stiff, plastic"
        ));
        assert!(is_body_sentence_heading(
            "Granular Fill [AUCKLAND VOLCANIC FIELD] silty CLAY, orangey brown to brown mottled light grey, moist to wet, very stiff"
        ));
        assert!(is_priced_schedule_heading(
            "3.2 Cut to Waste (Solid measure) - supply all m3 $ -"
        ));
    }

    #[test]
    fn amendment_bookmark_does_not_graft_demoted_instruction() {
        let mut pages = vec![
            "# Introduction".into(),
            "# Add new clause:\n# Scope".into(),
            "# Conclusion".into(),
        ];
        let bookmarks = vec![
            Bookmark {
                level: 1,
                title: "Introduction".into(),
                page_index: 0,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "Add new clause:".into(),
                page_index: 1,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "Scope".into(),
                page_index: 1,
                y_pdf: None,
            },
            Bookmark {
                level: 1,
                title: "Conclusion".into(),
                page_index: 2,
                y_pdf: None,
            },
        ];
        normalize(&mut pages, &bookmarks, &HashSet::new(), &[]);
        assert!(pages[1].contains("Add new clause:\n"));
        assert!(!pages[1].contains("# Add new clause:"));
        assert!(pages[1].contains("## Scope"));
    }

    #[test]
    fn matching_bookmark_preserves_first_running_title() {
        let mut pages = vec![
            "# SECTION 1 - WALLS NZS 3604:2011".into(),
            "# SECTION 1 - WALLS NZS 3604:2011".into(),
            "# SECTION 1 - WALLS NZS 3604:2011".into(),
        ];
        let bookmarks = vec![
            Bookmark {
                level: 1,
                title: "SECTION 1 - WALLS NZS 3604:2011".into(),
                page_index: 0,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "Middle".into(),
                page_index: 1,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "End".into(),
                page_index: 2,
                y_pdf: None,
            },
        ];
        normalize(&mut pages, &bookmarks, &HashSet::new(), &[]);
        assert!(pages[0].starts_with("# SECTION 1 - WALLS NZS 3604:2011"));
        assert!(!pages[1].contains("# SECTION 1 - WALLS"));
        assert!(!pages[2].contains("# SECTION 1 - WALLS"));
    }

    #[test]
    fn table_column_labels_are_demoted_but_numbered_clause_remains() {
        let mut pages = vec![
            "**Table 8.4 - Studs**\n# Wind zone\n\n(mm x mm)\n\nsize\n\n| High | 90 x 45 |\n|---|---|\n# 8.5 Wall framing\nBody"
                .into(),
        ];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(pages[0].contains("\nWind zone\n"));
        assert!(pages[0].contains("# 8.5 Wall framing"));
    }

    #[test]
    fn contents_and_uppercase_reference_headings_survive_near_tables() {
        let mut pages = vec![
            "# CONTENTS\n| Clause | Page |\n|---|---|".into(),
            "# NEW ZEALAND STANDARDS\n| NZS 3604 | Timber |\n|---|---|".into(),
        ];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(pages[0].starts_with("# CONTENTS"));
        assert!(pages[1].starts_with("# NEW ZEALAND STANDARDS"));
    }

    #[test]
    fn long_numbered_note_is_not_a_heading() {
        let mut pages = vec![
            "# 1. Areas and dimensions are subject to final survey and deposit of plans.".into(),
        ];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(!pages[0].starts_with('#'));
    }

    #[test]
    fn trustworthy_bookmarks_align_and_graft() {
        let mut pages = vec!["### Scope".into(), "Text".into(), "## Work".into()];
        let bookmarks = vec![
            Bookmark {
                level: 1,
                title: "Scope".into(),
                page_index: 0,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "Missing".into(),
                page_index: 1,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "Work".into(),
                page_index: 2,
                y_pdf: None,
            },
        ];
        let report = normalize(&mut pages, &bookmarks, &HashSet::new(), &[]);
        assert_eq!(pages[0], "# Scope");
        assert!(pages[1].starts_with("## Missing\n\n"));
        assert_eq!(report.grafted_bookmarks, 1);
    }

    #[test]
    fn nonmonotonic_figure_navigation_does_not_hide_structural_bookmarks() {
        let mut pages = vec![
            "### Introduction".into(),
            "### Scope".into(),
            "### End".into(),
        ];
        let bookmarks = vec![
            Bookmark {
                level: 1,
                title: "Introduction".into(),
                page_index: 0,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "Table 1.1 - Limits".into(),
                page_index: 2,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "Scope".into(),
                page_index: 1,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "Figure 1.1 - Layout".into(),
                page_index: 0,
                y_pdf: None,
            },
            Bookmark {
                level: 1,
                title: "End".into(),
                page_index: 2,
                y_pdf: None,
            },
        ];
        normalize(&mut pages, &bookmarks, &HashSet::new(), &[]);
        assert_eq!(pages, ["# Introduction", "## Scope", "# End"]);
    }

    #[test]
    fn bookmark_parent_uses_existing_prefix_and_precedes_child() {
        let mut pages = vec![
            "## 6.3 SETTING OUT 6.3.1 General\nBody\n### 6.4.1 Height of piles\nBody".into(),
            "# 7 ROOF".into(),
        ];
        let bookmarks = vec![
            Bookmark {
                level: 2,
                title: "6.3 Setting out".into(),
                page_index: 0,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "6.4 Piles".into(),
                page_index: 0,
                y_pdf: None,
            },
            Bookmark {
                level: 1,
                title: "7 Roof".into(),
                page_index: 1,
                y_pdf: None,
            },
        ];
        normalize(&mut pages, &bookmarks, &HashSet::new(), &[]);
        assert_eq!(pages[0].matches("6.3 SETTING OUT").count(), 1);
        assert!(!pages[0].contains("6.3 Setting out"));
        assert!(pages[0].find("## 6.4 Piles").unwrap() < pages[0].find("### 6.4.1").unwrap());
    }

    #[test]
    fn contents_continuation_does_not_promote_appendix_entries() {
        let mut pages = vec![
            "# Contents\n**1 Introduction ................ 1**\n**2 Design ................ 2**\n**3 Scope ................ 3**".into(),
            "**10.4 Construction Monitoring ................ 10**\n**11 Limitations ................ 11**\n**Appendix A ................ 12**\n# Appendix A: Architectural Plans".into(),
        ];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(pages[0].starts_with("# Contents"));
        assert!(pages[1].ends_with("Appendix A: Architectural Plans"));
        assert!(!pages[1].contains("# Appendix A"));
    }

    #[test]
    fn log_depth_and_contact_lines_are_text() {
        let mut pages = vec!["# 2 @1.9m becomes CLAY\n###### GeoStudio Ltd 7 www.geostudio.co.nz\n# 10 Geotechnical Recommendations".into()];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert_eq!(
            pages[0],
            "2 @1.9m becomes CLAY\nGeoStudio Ltd 7 www.geostudio.co.nz\n# 10 Geotechnical Recommendations"
        );
    }

    #[test]
    fn unnumbered_siblings_share_style_level_under_numbered_sections() {
        let mut pages = vec![
            "# 1 Scope\n###### Design\n###### Construction".into(),
            "# 2 Methods\n###### Testing".into(),
        ];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert_eq!(pages[0], "# 1 Scope\n## Design\n## Construction");
        assert_eq!(pages[1], "# 2 Methods\n## Testing");
    }

    #[test]
    fn missing_bookmark_uses_page_position() {
        let mut pages = vec![
            "# Introduction\nOpening text\nSection content\n".into(),
            "# Final\nEnding text".into(),
        ];
        let bookmarks = vec![
            Bookmark {
                level: 1,
                title: "Introduction".into(),
                page_index: 0,
                y_pdf: Some(90.0),
            },
            Bookmark {
                level: 2,
                title: "Middle".into(),
                page_index: 0,
                y_pdf: Some(50.0),
            },
            Bookmark {
                level: 1,
                title: "Final".into(),
                page_index: 1,
                y_pdf: Some(90.0),
            },
        ];
        let positions = vec![
            PagePosition {
                width: 100.0,
                height: 100.0,
                lines: vec![
                    PositionedLine {
                        text: "Opening text".into(),
                        y_top: 25.0,
                    },
                    PositionedLine {
                        text: "Section content".into(),
                        y_top: 60.0,
                    },
                ],
            },
            PagePosition {
                width: 100.0,
                height: 100.0,
                lines: vec![],
            },
        ];
        let report = normalize(&mut pages, &bookmarks, &HashSet::new(), &positions);
        assert_eq!(report.grafted_bookmarks, 1);
        assert!(pages[0].contains("Opening text\n## Middle\n\nSection content"));
    }

    #[test]
    fn bookmark_promotes_standalone_title_without_duplicating_it() {
        let mut pages = vec![
            "# Introduction".into(),
            "Opening text\n**Middle Section**\nBody".into(),
            "# Final".into(),
        ];
        let bookmarks = vec![
            Bookmark {
                level: 1,
                title: "Introduction".into(),
                page_index: 0,
                y_pdf: None,
            },
            Bookmark {
                level: 2,
                title: "Middle Section".into(),
                page_index: 1,
                y_pdf: None,
            },
            Bookmark {
                level: 1,
                title: "Final".into(),
                page_index: 2,
                y_pdf: None,
            },
        ];
        normalize(&mut pages, &bookmarks, &HashSet::new(), &[]);
        assert_eq!(pages[1].matches("Middle Section").count(), 1);
        assert!(pages[1].contains("## Middle Section"));
    }

    #[test]
    fn counter_template_bookmarks_are_not_treated_as_structure() {
        let bookmarks: Vec<_> = (1..=5)
            .map(|page| Bookmark {
                level: 1,
                title: format!("Sheet {page}"),
                page_index: page - 1,
                y_pdf: None,
            })
            .collect();
        assert!(!trustworthy_bookmarks(&bookmarks, 5));
    }

    #[test]
    fn masterspec_section_codes_parent_internal_clause_numbers() {
        let mut pages = vec![
            "# 1220 PROJECT\n# 1 GENERAL\n## 1.1 Scope\n# 2 PRODUCTS\n# 1232 INTERPRETATION\n# 1 GENERAL\n# 1240 ESTABLISHMENT"
                .into(),
        ];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert_eq!(
            pages[0],
            "# 1220 PROJECT\n## 1 GENERAL\n### 1.1 Scope\n## 2 PRODUCTS\n# 1232 INTERPRETATION\n## 1 GENERAL\n# 1240 ESTABLISHMENT"
        );
    }

    // -- Golden document tests (item 8) --------------------------------

    #[test]
    fn golden_bookmarked_spec() {
        let mut pages = vec![
            "Introduction\nThis specification covers general requirements.".to_owned(),
            "Materials\nAll materials shall comply with the relevant standards.".to_owned(),
            "Fixings\nFixings shall be hot-dip galvanised.".to_owned(),
        ];
        let bookmarks = vec![
            Bookmark {
                level: 1,
                title: "Introduction".into(),
                page_index: 0,
                y_pdf: None,
            },
            Bookmark {
                level: 1,
                title: "Materials".into(),
                page_index: 1,
                y_pdf: None,
            },
            Bookmark {
                level: 1,
                title: "Fixings".into(),
                page_index: 2,
                y_pdf: None,
            },
        ];
        let report = normalize(&mut pages, &bookmarks, &HashSet::new(), &[]);
        assert_eq!(report.grafted_bookmarks, 3);
        assert!(pages[0].starts_with("# Introduction"));
        assert!(pages[0].contains("This specification covers general requirements."));
        assert_eq!(pages[0].matches("# Introduction").count(), 1);
        assert!(pages[1].starts_with("# Materials"));
        assert!(pages[1].contains("All materials shall comply with the relevant standards."));
        assert!(pages[2].starts_with("# Fixings"));
        assert!(pages[2].contains("Fixings shall be hot-dip galvanised."));
    }

    #[test]
    fn golden_numbered_spec_without_bookmarks() {
        let mut pages = vec![
            "# 3 Materials\n\n## 3.1 Timber to NZS 3602\n\nAll timber shall be graded.\n\n## 3.2 Fixings to AS 1397\n\nFixings shall be galvanised steel.\n\n# 4 Workmanship\n\n## 4.1 Site Preparation\n\nClear the site prior to works."
                .to_owned(),
        ];
        normalize(&mut pages, &[], &HashSet::new(), &[]);
        assert!(pages[0].contains("# 3 Materials"));
        assert!(pages[0].contains("## 3.1 Timber to NZS 3602"));
        assert!(pages[0].contains("## 3.2 Fixings to AS 1397"));
        assert!(pages[0].contains("# 4 Workmanship"));
        assert!(pages[0].contains("## 4.1 Site Preparation"));
        assert!(pages[0].contains("All timber shall be graded."));
        assert!(pages[0].contains("Clear the site prior to works."));
    }

    #[test]
    fn golden_textbook_running_page_headers_across_document() {
        // A PRML-style textbook: each page repeats the current chapter
        // title followed by the page number ("1.5. Decision Theory 45").
        // These must never surface as section headings, on any page of
        // the whole (300-page) book, while the body text is preserved.
        const BOOK_TOTAL_PAGES: usize = 300;
        let mut pages = vec![
            "# 1.5. Decision Theory 40\nBayesian methods provide a framework.".to_owned(),
            "# 1.5. Decision Theory 41\nWe now consider loss functions in detail.".to_owned(),
            "# 1.5. Decision Theory 42\nThe minimum expected loss criterion follows.".to_owned(),
            "# 1.6 Information Theory 43\nThis section introduces entropy.".to_owned(),
            "# 1.6 Information Theory 44\nEntropy measures average information content.".to_owned(),
        ];
        let page_numbers = [40, 41, 42, 43, 44];
        normalize_pages(
            &mut pages,
            &[],
            &HashSet::new(),
            &[],
            &page_numbers,
            BOOK_TOTAL_PAGES,
        );
        for page in &pages {
            assert!(
                !page.starts_with('#'),
                "running page header retained as a heading: {page}"
            );
        }
        assert!(pages[0].contains("Bayesian methods provide a framework."));
        assert!(pages[3].contains("This section introduces entropy."));
    }
}
