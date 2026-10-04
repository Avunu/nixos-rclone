//! Markdown ⇄ docx, in process.
//!
//! This replaces two pandoc invocations and the two Haskell filters that went
//! with them. The filters existed because a round trip through docx must give
//! the Obsidian note back as it was written:
//!
//! * **markdown → docx** strips heading ids (so the docx does not accumulate
//!   stale bookmarks) and turns tight lists loose: Google Docs renders a list
//!   item properly only when it is its own paragraph, not a bare "Plain" run.
//! * **docx → markdown** turns loose lists tight again, so Obsidian does not
//!   show a blank line between every item.
//!
//! Both are walks over carta's pandoc-compatible AST.
//!
//! What this does not carry, as before: wikilinks and callouts come back
//! escaped (`\[\[Note\]\]`), and images are not embedded.

use anyhow::{Context, Result, anyhow};
use carta::ast::Block;
use carta::walk::for_each_block;
use carta::{
    Document, MediaBag, Output, ReaderOptions, WrapMode, WriterOptions, read_document,
    render_document,
};

/// Obsidian's lists have no blank line before them, which CommonMark would
/// not accept as a list.
const MARKDOWN: &str = "markdown+lists_without_preceding_blankline";

/// Convert markdown text to a docx file.
///
/// `reference` is an earlier docx of the same note: its styles, fonts, theme,
/// page setup, headers and footers (page numbers) are reused, so formatting
/// someone applied in Google Docs survives the next conversion.
pub fn md_to_docx(markdown: &str, reference: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let (mut doc, media) = read_document(MARKDOWN, markdown.as_bytes(), &ReaderOptions::default())
        .map_err(|e| anyhow!("reading markdown: {e}"))?;
    md2docx(&mut doc);

    let mut opts = WriterOptions::default();
    opts.wrap = WrapMode::Preserve;
    opts.docx.reference_doc = reference.clone();
    let bytes = match render_document("docx", doc, media, &opts)
        .map_err(|e| anyhow!("writing docx: {e}"))?
    {
        Output::Bytes(b) => b,
        Output::Text(_) => return Err(anyhow!("docx writer returned text")),
    };
    // carta takes only the styles from a reference; the page numbers, headers
    // and page setup someone added in Google Docs are carried over here.
    match reference {
        Some(r) => crate::markdown::page::graft_page_setup(bytes, &r),
        None => Ok(bytes),
    }
}

/// Convert a docx file to markdown text, ending in a newline like a file.
pub fn docx_to_md(docx: &[u8]) -> Result<String> {
    let (mut doc, _media): (Document, MediaBag) =
        read_document("docx", docx, &ReaderOptions::default())
            .map_err(|e| anyhow!("reading docx: {e}"))?;
    docx2md(&mut doc);

    let mut opts = WriterOptions::default();
    opts.wrap = WrapMode::None;
    match render_document("markdown", doc, MediaBag::default(), &opts)
        .map_err(|e| anyhow!("writing markdown: {e}"))?
    {
        Output::Text(mut t) => {
            if !t.ends_with('\n') {
                t.push('\n');
            }
            Ok(t)
        }
        Output::Bytes(_) => Err(anyhow!("markdown writer returned bytes")),
    }
}

/// Convert files, as the sync passes do.
pub fn md_file_to_docx(
    md: &std::path::Path,
    reference: Option<&std::path::Path>,
) -> Result<Vec<u8>> {
    let text = std::fs::read_to_string(md).with_context(|| format!("reading {}", md.display()))?;
    let reference = match reference {
        Some(r) => Some(std::fs::read(r).with_context(|| format!("reading {}", r.display()))?),
        None => None,
    };
    md_to_docx(&text, reference).with_context(|| format!("converting {}", md.display()))
}

pub fn docx_file_to_md(docx: &std::path::Path) -> Result<String> {
    let bytes = std::fs::read(docx).with_context(|| format!("reading {}", docx.display()))?;
    docx_to_md(&bytes).with_context(|| format!("converting {}", docx.display()))
}

/// The markdown → docx filter.
fn md2docx(doc: &mut Document) {
    for_each_block(&mut doc.blocks, &mut |b| match b {
        Block::Header(_, attr, _) => attr.id.clear(),
        Block::BulletList(items) | Block::OrderedList(_, items) => {
            for item in items {
                // Only an item that is a single tight run: anything richer is
                // already made of paragraphs.
                if let [only @ Block::Plain(_)] = item.as_mut_slice()
                    && let Block::Plain(inlines) = std::mem::replace(only, Block::HorizontalRule)
                {
                    *only = Block::Para(inlines);
                }
            }
        }
        _ => {}
    });
}

/// The docx → markdown filter.
fn docx2md(doc: &mut Document) {
    for_each_block(&mut doc.blocks, &mut |b| match b {
        // The docx reader numbers repeated headings (`a`, `a-1`), which the
        // markdown writer would then spell out as `{#a-1}`.
        Block::Header(_, attr, _) => attr.id.clear(),
        Block::BulletList(items) | Block::OrderedList(_, items) => {
            for item in items {
                if let [only @ Block::Para(_)] = item.as_mut_slice()
                    && let Block::Para(inlines) = std::mem::replace(only, Block::HorizontalRule)
                {
                    *only = Block::Plain(inlines);
                }
            }
        }
        _ => {}
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    const FIXTURE: &str = include_str!("../../fixtures/test.md");

    fn round_trip(md: &str) -> String {
        docx_to_md(&md_to_docx(md, None).unwrap()).unwrap()
    }

    #[test]
    fn the_fixture_survives_a_round_trip_unchanged() {
        // Was the `round-trip` check, which ran pandoc and both filters.
        assert_eq!(round_trip(FIXTURE), FIXTURE);
    }

    #[test]
    fn produces_a_real_docx() {
        let bytes = md_to_docx("# Title\n\nBody.\n", None).unwrap();
        assert_eq!(&bytes[..2], b"PK", "a zip container");
        let mut zip = Vec::new();
        // The body is in word/document.xml; search the raw (stored) archive
        // for the entry name rather than pulling in a zip reader.
        std::io::Cursor::new(&bytes).read_to_end(&mut zip).unwrap();
        assert!(zip.windows(17).any(|w| w == b"word/document.xml"));
    }

    #[test]
    fn lists_stay_tight_and_do_not_gain_blank_lines() {
        let md = "Intro:\n\n- one\n- two\n    - nested a\n    - nested b\n- three\n\n1. first\n2. second\n";
        let back = round_trip(md);
        assert!(back.contains("- one\n- two\n"), "{back:?}");
        assert!(!back.contains("- one\n\n"), "loose list: {back:?}");
        assert!(
            back.contains("1.  first\n2.  second\n") || back.contains("1. first\n2. second\n"),
            "{back:?}"
        );
    }

    #[test]
    fn repeated_headings_do_not_grow_ids() {
        let md = "# Notes\n\ntext\n\n# Notes\n\nmore\n\n## Sub\n\n## Sub\n";
        let back = round_trip(md);
        assert!(!back.contains("{#"), "heading ids leaked: {back:?}");
        assert_eq!(back.matches("# Notes").count(), 2);
    }

    #[test]
    fn markdown_to_docx_strips_heading_ids_and_loosens_lists() {
        let (mut doc, _) = read_document(
            MARKDOWN,
            b"# Title {#custom}\n\n- a\n- b\n",
            &ReaderOptions::default(),
        )
        .unwrap();
        md2docx(&mut doc);
        let mut ids = Vec::new();
        let mut plain = 0;
        for_each_block(&mut doc.blocks, &mut |b| match b {
            Block::Header(_, a, _) => ids.push(a.id.to_string()),
            Block::Plain(_) => plain += 1,
            _ => {}
        });
        assert_eq!(ids, [""]);
        assert_eq!(plain, 0, "tight items must become paragraphs");
    }

    #[test]
    fn a_reference_docx_is_accepted() {
        let first = md_to_docx("# One\n", None).unwrap();
        let second = md_to_docx("# Two\n\nedited\n", Some(first)).unwrap();
        let back = docx_to_md(&second).unwrap();
        assert!(
            back.contains("# Two") && back.contains("edited"),
            "{back:?}"
        );
    }

    #[test]
    fn obsidian_lists_without_a_blank_line_before_them_are_lists() {
        let back = round_trip("Title over list:\n- a\n- b\n");
        assert!(back.contains("- a\n- b"), "{back:?}");
    }

    #[test]
    fn wrapping_is_preserved_going_in_and_never_added_coming_out() {
        let long = "word ".repeat(60);
        let md = format!("{}\n", long.trim_end());
        assert_eq!(round_trip(&md), md);
    }

    #[test]
    fn unicode_and_paths_with_spaces_in_content_are_fine() {
        let md = "# Caf\u{e9} \u{201c}notes\u{201d}\n\nSee [Meeting Notes](Meeting%20Notes/Jan%20Session.md) \u{1f600}.\n";
        let back = round_trip(md);
        assert!(
            back.contains("Caf\u{e9}") && back.contains("\u{1f600}"),
            "{back:?}"
        );
        assert!(
            back.contains("Meeting%20Notes/Jan%20Session.md"),
            "{back:?}"
        );
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(docx_to_md(b"not a zip").is_err());
    }
}
