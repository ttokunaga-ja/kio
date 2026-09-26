//! Rendering Markdown escapes out of the derived search projection.
//!
//! 07 §5.2.1 requires provider raw text embedded in Markdown body to be
//! escaped whatever its origin, and `canonical_source_escape` implements that
//! maximally: it entity-encodes `&`, `<` and `>` and puts a backslash in front
//! of *every* ASCII punctuation character, so that recovered OCR text can never
//! smuggle a heading, a table or raw HTML past the acceptance check. That is
//! the right rule for the stored bytes and the wrong one for the search index —
//! a deadline the service read as `期限 7/10` is stored as `期限 7\/10`, and the
//! query `7/10` does not find it. The miss happens before ranking: the chunk is
//! never a candidate at all.
//!
//! So the escapes are resolved here, in the derived projection, exactly like
//! the NUL stripping and the NFC normalization in `project_search_text`. Identity is untouched — `chunk_id`, `text_hash`, `byte_start/end` and
//! the persisted `chunks.jsonl` / normalized Markdown all still carry the
//! escaped bytes, and evidence offsets still resolve against those.
//!
//! **Code is left alone**, and that is the whole design of this module rather
//! than an afterthought. Counting `\` followed by ASCII punctuation across the
//! 1,165 tracked `.md` files in this repository:
//!
//! | where | occurrences |
//! |---|---|
//! | inside a fenced block | 439 |
//! | inside an inline code span | 16 |
//! | plain body text | 23 |
//!
//! 416 of the fenced ones were measured in the retained, non-authorizing paid
//! OCR archive at `eval/fixtures/normalized-corpus/`. That archive supplied the
//! frozen regression examples but is not a current evaluator manifest input.
//! A blanket unescape would rewrite 455 sequences of genuine code — `find … -exec … \;`,
//! `\(\s*\)`, `AppData\Local\Temp`, JSON `\"` — to repair 23. Inside code a
//! backslash is not an escape and a reader sees it, so the projection keeps it.
//!
//! FTS and embedding inputs share this pure projection. Embedding callers derive
//! it from authenticated canonical CAS text, never from the mutable SQLite text
//! column. Canonical text hashes and evidence offsets remain over original bytes.

use unicode_normalization::UnicodeNormalization;

use crate::chunking::is_fence_delimiter;

/// Derive searchable text without changing canonical text identity or offsets.
/// Apply NFC first, strip U+0000 second, then resolve Markdown escapes outside
/// code spans and fences. The transformation order is part of the projection.
#[must_use]
pub fn project_search_text(text: &str) -> String {
    resolve_markdown_escapes(&text.nfc().collect::<String>().replace('\u{0}', ""))
}

/// Resolve Markdown escapes so the index holds the text a reader sees.
///
/// Backslash escapes and the three entities `canonical_source_escape` emits are
/// undone outside code; fenced blocks and inline code spans are copied through
/// byte for byte.
pub(crate) fn resolve_markdown_escapes(markdown: &str) -> String {
    let mut resolved = String::with_capacity(markdown.len());
    let mut in_fence = false;
    for line in markdown.split_inclusive('\n') {
        if is_fence_delimiter(line) {
            in_fence = !in_fence;
            resolved.push_str(line);
            continue;
        }
        if in_fence {
            resolved.push_str(line);
            continue;
        }
        resolve_line(line, &mut resolved);
    }
    resolved
}

/// Resolve one non-fenced line.
///
/// Inline code spans are tracked per line rather than across the document.
/// CommonMark lets a span cross a newline, but an unbalanced backtick is far
/// more common than a wrapped span, and the two failure directions are not
/// symmetric: treating body text as code leaves an escape in place, which is
/// the status quo this module improves on, while treating code as body text
/// corrupts it. Bounding the span to its line fails in the recoverable
/// direction.
fn resolve_line(line: &str, resolved: &mut String) {
    let mut characters = line.chars().peekable();
    let mut in_span = false;
    while let Some(character) = characters.next() {
        match character {
            '`' => {
                in_span = !in_span;
                resolved.push(character);
            }
            _ if in_span => resolved.push(character),
            // A backslash-escaped backtick is consumed here and never reaches
            // the arm above, so it cannot open a span — which is what
            // CommonMark says, and what makes escaped provider text (where
            // every backtick is escaped) safe to walk.
            '\\' => match characters.peek() {
                Some(next) if next.is_ascii_punctuation() => {
                    resolved.push(*next);
                    characters.next();
                }
                _ => resolved.push('\\'),
            },
            // Decoding the entity consumes it whole, so the `;` that
            // `canonical_source_escape` would have backslashed is already past
            // by the time the backslash arm could see it. That ordering is what
            // makes a literal `&amp;` in the source survive the round trip:
            // escaped it is `&amp;amp\;`, and this reads it back as `&amp;`.
            '&' => {
                if let Some(decoded) = decode_entity(&mut characters) {
                    resolved.push(decoded);
                } else {
                    resolved.push('&');
                }
            }
            _ => resolved.push(character),
        }
    }
}

/// Consume `lt;`, `gt;` or `amp;` after a `&`, or consume nothing.
fn decode_entity(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) -> Option<char> {
    const ENTITIES: [(&str, char); 3] = [("lt;", '<'), ("gt;", '>'), ("amp;", '&')];
    // `amp;` is the longest name, so four characters of lookahead decide it.
    // Collecting the whole remaining iterator instead would make one `&` cost a
    // scan to end of line, and a line of entity-escaped text quadratic.
    let ahead = characters.clone().take(4).collect::<String>();
    for (name, decoded) in ENTITIES {
        if ahead.starts_with(name) {
            for _ in 0..name.chars().count() {
                characters.next();
            }
            return Some(decoded);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_applies_nfc_then_nul_removal_then_body_escape_resolution() {
        let source = "e\u{301} e\0\u{301} \\\0! &a\0mp; `e\u{301} \\\0! &amp;`\n```\ne\u{301} \\\0! &amp;\n```\n";
        let expected = "é e\u{301} ! & `é \\! &amp;`\n```\né \\! &amp;\n```\n";
        assert_eq!(project_search_text(source), expected);
    }

    #[test]
    fn escaped_punctuation_is_resolved_in_body_text() {
        assert_eq!(resolve_markdown_escapes("期限 7\\/10"), "期限 7/10");
        assert_eq!(
            resolve_markdown_escapes("Ln 1, Col 1 Spaces: 2 UTF\\-8 LF"),
            "Ln 1, Col 1 Spaces: 2 UTF-8 LF"
        );
    }

    #[test]
    fn entities_are_decoded_in_body_text() {
        assert_eq!(resolve_markdown_escapes("&lt;div&gt;"), "<div>");
        assert_eq!(resolve_markdown_escapes("a &amp; b"), "a & b");
    }

    #[test]
    fn a_literal_entity_in_the_source_survives_the_round_trip() {
        // `canonical_source_escape("&amp;")` is `&amp;amp\;` — the `&` becomes an
        // entity and the trailing `;` gets a backslash. Reading it back must
        // give the source text, not `&`.
        assert_eq!(resolve_markdown_escapes("&amp;amp\\;"), "&amp;");
    }

    #[test]
    fn a_lone_ampersand_that_starts_no_entity_is_kept() {
        assert_eq!(resolve_markdown_escapes("Q&A"), "Q&A");
        assert_eq!(resolve_markdown_escapes("&ampersand"), "&ampersand");
    }

    #[test]
    fn a_backslash_before_a_non_punctuation_char_is_kept() {
        assert_eq!(resolve_markdown_escapes("\\d+ digits"), "\\d+ digits");
        assert_eq!(resolve_markdown_escapes("trailing \\"), "trailing \\");
    }

    #[test]
    fn fenced_code_keeps_its_backslashes() {
        let markdown = "body 7\\/10\n```sh\nfind . -exec shasum {} \\;\n```\nbody &lt;b&gt;\n";
        assert_eq!(
            resolve_markdown_escapes(markdown),
            "body 7/10\n```sh\nfind . -exec shasum {} \\;\n```\nbody <b>\n"
        );
    }

    #[test]
    fn tilde_fences_and_indented_fences_close_the_block() {
        let markdown = "  ~~~\n  a \\| b\n  ~~~\nafter \\| here\n";
        assert_eq!(
            resolve_markdown_escapes(markdown),
            "  ~~~\n  a \\| b\n  ~~~\nafter | here\n"
        );
    }

    #[test]
    fn inline_code_spans_keep_their_backslashes() {
        assert_eq!(
            resolve_markdown_escapes("the pattern `\\(\\s*\\)` matches \\(this\\)"),
            "the pattern `\\(\\s*\\)` matches (this)"
        );
    }

    #[test]
    fn an_escaped_backtick_does_not_open_a_span() {
        // Every backtick in provider text is escaped, so this is the shape all
        // recovered OCR text takes. If the escaped backtick opened a span the
        // rest of the line would stop being unescaped.
        assert_eq!(
            resolve_markdown_escapes("run \\`cmd\\` then 7\\/10"),
            "run `cmd` then 7/10"
        );
    }

    #[test]
    fn an_unbalanced_backtick_bounds_its_damage_to_one_line() {
        let markdown = "half `open 7\\/10\nnext line 7\\/10\n";
        assert_eq!(
            resolve_markdown_escapes(markdown),
            "half `open 7\\/10\nnext line 7/10\n"
        );
    }

    #[test]
    fn text_without_escapes_is_returned_unchanged() {
        let markdown = "# 見出し\n\n本文はそのまま。No escapes here.\n";
        assert_eq!(resolve_markdown_escapes(markdown), markdown);
    }
}
