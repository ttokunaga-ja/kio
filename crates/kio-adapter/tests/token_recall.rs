//! Does the text a page declares actually come back?
//!
//! `experiments/ocr-verification/fixtures/generated-images/ground-truth.json`
//! states, per input image, a token printed on it. Nothing checked whether that
//! token survives OCR until 2026-08-09, and by then a capture had been shipped
//! that loses its own token — `code-editor-no-crops.json`, whose
//! `G1-01-TOKEN-4827` is in neither `markdown.text` nor any `block_content` of
//! the 613 KB the service returned. A whole class of failure was going
//! unmeasured: the response parses, the shape is right, every count agrees, and
//! the words are gone.
//!
//! # Why this file has no per-capture code
//!
//! `real_layout_parsing_captures.rs` wires each capture in by hand three times —
//! an `include_str!` const, the `captures` array, and `FIGURES`. That is exactly
//! how five captures came to sit in this directory read by nothing while the
//! suite stayed green. Repeating that structure here would rebuild the problem.
//!
//! So this reads its inputs at run time: `capture-manifest.json` says which
//! captures exist and which input each came from, and
//! [`every_capture_in_this_directory_is_read_by_this_test`] fails if the
//! directory holds a capture the manifest does not list. **Adding a capture is
//! adding a row**, and forgetting the row is a test failure rather than silence.
//!
//! # What is asserted, and what is only printed
//!
//! The assertion is binary: does the declared token appear in `markdown.text`.
//! `visible_text` recall is printed and never asserted — the infographic loses
//! 18 fragments that are all chart-internal text, which is correct behaviour and
//! the reason `related_images[]` and `kio open` exist. A pinned rate would fail
//! on healthy variation and teach everyone to edit the number.
//!
//! `blocks` and `widest` are printed on the same terms, and for a sharper
//! reason: they describe the *cause* the token column only sees the effect of.
//! The one capture that loses its token is also the one whose page came back as
//! a single block covering nearly all of it — but that is one page, and one page
//! is exactly how S3-J's refuted rule got written. See [`block_spread`].

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use kio_adapter::local_ocr_markdownize::parse_layout_parsing;
use serde_json::Value;
use unicode_normalization::UnicodeNormalization;

/// The manifest lives beside the captures so that whoever adds one sees it. It
/// is the single name this directory scan must skip.
const MANIFEST: &str = "capture-manifest.json";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/layout-parsing")
}

/// Read from the experiment that produced the images rather than copied here:
/// a copy would be one more thing to keep in step, and the point of this test is
/// that the declaration and the measurement are not allowed to drift.
fn ground_truth_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../experiments/ocr-verification/fixtures/generated-images/ground-truth.json")
}

fn read_json(path: &Path) -> Value {
    let raw = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    serde_json::from_str(&raw)
        .unwrap_or_else(|error| panic!("{} is not valid JSON: {error}", path.display()))
}

struct Row {
    capture: String,
    source_image: String,
    /// What was **measured**, not what is wanted. See the manifest.
    token_in_markdown: bool,
    measured: String,
}

fn string_field(entry: &Value, key: &str) -> String {
    entry[key]
        .as_str()
        .unwrap_or_else(|| panic!("a capture-manifest.json row has no string `{key}`: {entry}"))
        .to_owned()
}

fn rows() -> Vec<Row> {
    let manifest = read_json(&fixtures().join(MANIFEST));
    let rows: Vec<Row> = manifest["captures"]
        .as_array()
        .expect("capture-manifest.json carries a `captures` array")
        .iter()
        .map(|entry| Row {
            capture: string_field(entry, "capture"),
            source_image: string_field(entry, "source_image"),
            token_in_markdown: entry["token_in_markdown"].as_bool().unwrap_or_else(|| {
                panic!(
                    "{} has no boolean `token_in_markdown`",
                    string_field(entry, "capture")
                )
            }),
            measured: string_field(entry, "measured"),
        })
        .collect();
    // A floor, not a count: adding captures must never fail, losing them must.
    assert!(
        rows.len() >= 9,
        "capture-manifest.json lists {} captures; it listed 9 on 2026-08-09, so \
         rows have been removed rather than added",
        rows.len()
    );
    rows
}

/// The token `ground-truth.json` declares is printed on an input image.
fn declared_token(ground_truth: &Value, source_image: &str) -> String {
    let entry = ground_truth
        .as_array()
        .expect("ground-truth.json is an array")
        .iter()
        .find(|entry| entry["file"].as_str() == Some(source_image))
        .unwrap_or_else(|| panic!("ground-truth.json declares nothing for {source_image}"));
    let tokens = entry["tokens"].as_array().expect("`tokens` is an array");
    assert_eq!(
        tokens.len(),
        1,
        "{source_image} declares {} tokens; this test reads exactly one",
        tokens.len()
    );
    tokens[0].as_str().expect("a token is a string").to_owned()
}

/// The `visible_text` fragments `ground-truth.json` claims are on the page.
fn declared_fragments(ground_truth: &Value, source_image: &str) -> Vec<String> {
    ground_truth
        .as_array()
        .expect("ground-truth.json is an array")
        .iter()
        .find(|entry| entry["file"].as_str() == Some(source_image))
        .and_then(|entry| entry["visible_text"].as_str())
        .unwrap_or_default()
        .split(';')
        .map(|fragment| fragment.trim().to_owned())
        .filter(|fragment| !fragment.is_empty())
        .collect()
}

/// Markdown escapes undone, then NFKC, whitespace dropped, case folded.
///
/// NFKC because the service returns full-width forms where the ground truth
/// writes ASCII (`再実行？` against `再実行?`), and counting that as a miss would
/// describe the encoding rather than the OCR. Whitespace because a line break
/// inside a token is a rendering detail, not a lost token.
///
/// The escapes go for exactly that reason too. 07 §5.2.1 requires provider text
/// embedded in the Markdown body to carry the CommonMark source escape, so
/// `期限 7/10` is archived as `期限 7\/10` and `&` as `&amp;`. Both render back to
/// what the page said. A comparison that scored them as misses would be reporting
/// the escape, not the OCR — and would have declared the whole furniture recovery
/// ineffective on the day it started working.
fn squeeze(text: &str) -> String {
    unescape_markdown(text)
        .nfkc()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The inverse of `canonical_source_escape`, for comparison only.
///
/// Backslashes first, then the three entity references: the escape writes `&` as
/// `&amp;` rather than `\&`, so its output holds no backslash that belongs to an
/// entity, and undoing them in this order cannot turn `\&amp;` into an ampersand
/// that was never there.
fn unescape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\'
            && let Some(&next) = characters.peek()
            && next.is_ascii_punctuation()
        {
            out.push(next);
            characters.next();
            continue;
        }
        out.push(character);
    }
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn pages(capture: &Value) -> &[Value] {
    capture["result"]["layoutParsingResults"]
        .as_array()
        .map(Vec::as_slice)
        .expect("a /layout-parsing response carries result.layoutParsingResults")
}

/// Everything Kio would index — taken from the adapter, not from the field.
///
/// This read `markdown.text` straight out of the capture and called it what Kio
/// indexes. That held until the adapter began recovering the `header`, `footer`
/// and `number` blocks the service leaves out of that field
/// (`tasks/furniture-text-recovery-design.md (git 56a6831)`). The raw field and the archived
/// text are now different strings, and reading the field would report text as lost
/// while it sits in the index — the exact inverse of the failure this file exists
/// to catch.
fn markdown_text(capture: &Value) -> String {
    parse_layout_parsing(capture)
        .expect("a real capture parses")
        .iter()
        .map(|page| page.markdown.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Everything the service recognised, whether or not it reached the Markdown.
/// The difference between this and [`markdown_text`] is the diagnosis: text here
/// but not there was read and then dropped in assembly — `header`, `footer` and
/// `number` blocks are, which is how `期限 7/10` leaves the index.
fn recognised_text(capture: &Value) -> String {
    pages(capture)
        .iter()
        .filter_map(|page| page["prunedResult"]["parsing_res_list"].as_array())
        .flatten()
        .filter_map(|block| block["block_content"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The page's pixel extent, from `result.dataInfo`. An image carries `width` and
/// `height` directly; a PDF moves them into `pages[]`, because a PDF can hold
/// more than one. Both shapes are in this directory — see the README's note on
/// `infographic-two-charts-as-pdf.json`.
fn page_extents(capture: &Value) -> Vec<(f64, f64)> {
    let info = &capture["result"]["dataInfo"];
    if let Some(pages) = info["pages"].as_array() {
        return pages
            .iter()
            .filter_map(|page| Some((page["width"].as_f64()?, page["height"].as_f64()?)))
            .collect();
    }
    match (info["width"].as_f64(), info["height"].as_f64()) {
        (Some(width), Some(height)) => vec![(width, height)],
        _ => Vec::new(),
    }
}

/// How many blocks the page was cut into, and how much of the page the largest
/// single block covers.
///
/// **Printed, never asserted, and deliberately not a rule.** The code editor is
/// the only capture whose window came back as one `content` block spanning
/// almost the entire page, and it is also the only one that lost its token. That
/// is one page. S3-J generalised from one page — keep `block_label == "chart"`,
/// drop the rest — and the very next capture refuted it. Writing the same shape
/// of rule here from the same amount of evidence would be the same mistake.
///
/// So these are numbers, not a threshold. When a second collapsed page is ever
/// captured, the comparison is already on screen instead of nine captures
/// needing re-measurement to find out whether the pattern was real.
///
/// One thing they settle immediately: **the block count alone does not separate
/// these pages.** `whiteboard-no-crops.json` is cut into four blocks too, the
/// same as the code editor, and keeps its token — its widest block covers 4% of
/// the page against the editor's 91%. Anyone reaching for "few blocks means the
/// window collapsed" is refuted by the row directly above it.
/// Blocks the adapter lifts back into the Markdown — `header`, `footer` and
/// `number` carrying text the service recognised and its own Markdown omitted.
///
/// Counted from the response rather than asked of the adapter, so the column says
/// what was there to recover rather than what the recovery reports about itself.
fn recovered_blocks(capture: &Value) -> usize {
    pages(capture)
        .iter()
        .filter_map(|page| page["prunedResult"]["parsing_res_list"].as_array())
        .flatten()
        .filter(|block| {
            matches!(
                block["block_label"].as_str(),
                Some("header" | "footer" | "number")
            )
        })
        .filter(|block| {
            block["block_content"]
                .as_str()
                .is_some_and(|content| !content.trim().is_empty())
        })
        .count()
}

fn block_spread(capture: &Value) -> (usize, Option<f64>) {
    let extents = page_extents(capture);
    let mut blocks = 0;
    let mut widest: Option<f64> = None;

    for (index, page) in pages(capture).iter().enumerate() {
        let Some(list) = page["prunedResult"]["parsing_res_list"].as_array() else {
            continue;
        };
        blocks += list.len();

        let Some(&(width, height)) = extents.get(index) else {
            continue;
        };
        let page_area = width * height;
        if page_area <= 0.0 {
            continue;
        }

        for block in list {
            let Some(bbox) = block["block_bbox"].as_array() else {
                continue;
            };
            let corners: Vec<f64> = bbox.iter().filter_map(Value::as_f64).collect();
            let [x0, y0, x1, y1] = corners[..] else {
                continue;
            };
            let share = ((x1 - x0) * (y1 - y0)).abs() / page_area;
            widest = Some(widest.map_or(share, |current: f64| current.max(share)));
        }
    }

    (blocks, widest)
}

/// A capture placed here and named nowhere is a capture nothing reads. That is
/// not hypothetical: it is what happened to five of these for a day, and what
/// this directory's README describes happening three times before that.
#[test]
fn every_capture_in_this_directory_is_read_by_this_test() {
    let on_disk: BTreeSet<String> = fs::read_dir(fixtures())
        .expect("the fixtures directory exists")
        .map(|entry| entry.expect("a readable directory entry").file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".json") && name != MANIFEST)
        .collect();
    let listed: BTreeSet<String> = rows().into_iter().map(|row| row.capture).collect();

    let unlisted: Vec<&String> = on_disk.difference(&listed).collect();
    assert!(
        unlisted.is_empty(),
        "these captures sit in the fixtures directory and no test reads them: \
         {unlisted:?}\n\n\
         Add a row to {MANIFEST} naming the input image it came from and whether \
         that image's declared token comes back. Placing the file is not enough — \
         being read is the whole point of keeping real captures."
    );

    let missing: Vec<&String> = listed.difference(&on_disk).collect();
    assert!(
        missing.is_empty(),
        "{MANIFEST} lists captures that are not in the directory: {missing:?}"
    );

    println!(
        "{} captures on disk, all listed in {MANIFEST}",
        on_disk.len()
    );
}

#[test]
fn every_declared_token_comes_back_exactly_as_the_manifest_measured() {
    let ground_truth = read_json(&ground_truth_file());
    let mut table = String::from(
        "\n  token   recall  blocks  widest  recov  capture                             declared token\n\
           \x20 ------  ------  ------  ------  -----  ----------------------------------  --------------------\n",
    );

    for row in rows() {
        let capture = read_json(&fixtures().join(&row.capture));
        let token = declared_token(&ground_truth, &row.source_image);
        assert!(
            token.is_ascii(),
            "{} declares a non-ASCII token {token:?}; the comparison below assumes \
             ASCII and would need rethinking rather than loosening",
            row.source_image
        );

        let needle = squeeze(&token);
        let in_markdown = squeeze(&markdown_text(&capture)).contains(&needle);
        let recognised = squeeze(&recognised_text(&capture)).contains(&needle);

        let fragments = declared_fragments(&ground_truth, &row.source_image);
        let indexed = squeeze(&markdown_text(&capture));
        let recovered = fragments
            .iter()
            .filter(|fragment| indexed.contains(&squeeze(fragment)))
            .count();

        let (blocks, widest) = block_spread(&capture);
        table.push_str(&format!(
            "  {:<6}  {:>2}/{:<3}  {:>6}  {:>6}  {:>5}  {:<34}  {}\n",
            if in_markdown { "yes" } else { "NO" },
            recovered,
            fragments.len(),
            blocks,
            widest.map_or_else(|| "-".to_owned(), |share| format!("{:.0}%", share * 100.0)),
            recovered_blocks(&capture),
            row.capture,
            token
        ));

        if in_markdown == row.token_in_markdown {
            continue;
        }
        if in_markdown {
            panic!(
                "{}: {token} now appears in markdown.text, and {MANIFEST} records that it \
                 does not.\n\n\
                 This is the service getting better, and the test is saying so rather than \
                 accepting either answer. The `false` recorded on {} is a measurement of a \
                 defect, never a target — nothing is supposed to keep returning nothing.\n\n\
                 Set \"token_in_markdown\": true for this row, and say in the commit what \
                 changed: a new image digest, new weights, a re-taken capture. Do not relax \
                 this assertion to tolerate both answers, because then neither direction is \
                 detected.",
                row.capture, row.measured
            );
        }
        panic!(
            "{}: ground-truth.json declares {token} for {}, and it is no longer in \
             markdown.text.\n\n\
             Present in some block_content: {recognised}.\n\
             {}\n\n\
             Measured present on {}.",
            row.capture,
            row.source_image,
            if recognised {
                "  -> the service still read the text; the loss is in how markdown.text is \
                 assembled. `header`, `footer` and `number` blocks are dropped from it — see \
                 this directory's README."
            } else {
                "  -> the text was not recognised at all, the way the code editor page's \
                 whole window collapsed into one `content` block."
            },
            row.measured
        );
    }

    println!("{table}");
}
