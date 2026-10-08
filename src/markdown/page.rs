//! Page setup carried over from a reference docx.
//!
//! carta's docx writer takes a reference document's styles, settings, fonts
//! and theme, but writes a bare section of its own: the page size, the margins
//! and above all the headers and footers (where a Google Doc keeps its page
//! numbers) are lost. pandoc kept them, so a note converted over its existing
//! docx must too, or every edit in Obsidian strips what was added in Google
//! Docs. This grafts the reference's final section, and the header and footer
//! parts it points at, onto the freshly written docx.

use anyhow::{Context, Result, anyhow};
use carta_core::container::zip::{self, ZipArchive};
use regex::Regex;
use std::collections::BTreeMap;
use std::sync::LazyLock;

const DOCUMENT: &str = "word/document.xml";
const DOCUMENT_RELS: &str = "word/_rels/document.xml.rels";
const CONTENT_TYPES: &str = "[Content_Types].xml";

static RELATIONSHIP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<Relationship\b[^>]*?/>").unwrap());
static RELATED_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"r:id="([^"]*)""#).unwrap());

/// `docx` with the final section (page setup, headers, footers) of `reference`.
/// A reference with nothing to carry returns `docx` as it was.
pub fn graft_page_setup(docx: Vec<u8>, reference: &[u8]) -> Result<Vec<u8>> {
    let Ok(reference) = zip::read_map(reference) else {
        // Not a docx we can read: the styles went through carta already, which
        // is as much as it could do with it.
        return Ok(docx);
    };
    let mut parts = zip::read_map(&docx).map_err(|e| anyhow!("reading the new docx: {e}"))?;

    let Some(ref_doc) = text(&reference, DOCUMENT) else {
        return Ok(docx);
    };
    let Some(section) = final_section(&ref_doc) else {
        return Ok(docx);
    };
    let ref_rels = text(&reference, DOCUMENT_RELS).unwrap_or_default();
    let mut rels = text(&parts, DOCUMENT_RELS).context("the new docx has no relationships")?;
    let mut types = text(&parts, CONTENT_TYPES).context("the new docx has no content types")?;
    let mut doc = text(&parts, DOCUMENT).context("the new docx has no body")?;

    // Every part the section points at: its relationship in the reference,
    // and a fresh one in the new document.
    let mut ids = BTreeMap::new();
    let mut new_rels = String::new();
    for rel in RELATIONSHIP.find_iter(&ref_rels) {
        let rel = rel.as_str();
        let (Some(id), Some(target)) = (attr(rel, "Id"), attr(rel, "Target")) else {
            continue;
        };
        if !section.contains(&format!("r:id=\"{id}\"")) || attr(rel, "TargetMode").is_some() {
            continue;
        }
        let name = format!("word/{target}");
        let Some(data) = reference.get(&name) else {
            continue;
        };
        let fresh = format!("rIdRef{}", ids.len() + 1);
        new_rels.push_str(&rel.replace(&format!("Id=\"{id}\""), &format!("Id=\"{fresh}\"")));
        ids.insert(id.to_string(), fresh);
        copy_part(&reference, &mut parts, &mut types, &name, data);
    }
    if ids.len() != RELATED_ID.captures_iter(&section).count() {
        // A reference the relationships do not explain: a section pointing at
        // nothing is worse than the bare one.
        return Ok(docx);
    }

    let section = RELATED_ID.replace_all(&section, |c: &regex::Captures| {
        format!("r:id=\"{}\"", ids[&c[1]])
    });
    let range = final_section_range(&doc).context("the new docx has no section")?;
    doc.replace_range(range, &section);
    rels = rels.replace("</Relationships>", &format!("{new_rels}</Relationships>"));

    parts.insert(DOCUMENT.into(), doc.into_bytes());
    parts.insert(DOCUMENT_RELS.into(), rels.into_bytes());
    parts.insert(CONTENT_TYPES.into(), types.into_bytes());

    // The content types first, as a package expects.
    let mut out = ZipArchive::new();
    let types = parts.remove(CONTENT_TYPES).unwrap();
    out.deflate(CONTENT_TYPES, &types)
        .map_err(|e| anyhow!("writing the docx: {e}"))?;
    for (name, data) in &parts {
        out.deflate(name, data)
            .map_err(|e| anyhow!("writing the docx: {e}"))?;
    }
    out.finish().map_err(|e| anyhow!("writing the docx: {e}"))
}

/// Copy a header or footer part with its own relationships (images) and
/// content types.
fn copy_part(
    reference: &BTreeMap<String, Vec<u8>>,
    parts: &mut BTreeMap<String, Vec<u8>>,
    types: &mut String,
    name: &str,
    data: &[u8],
) {
    parts.insert(name.to_string(), data.to_vec());
    add_content_type(reference, types, name);

    let (dir, file) = name.rsplit_once('/').unwrap_or(("", name));
    let rels_name = format!("{dir}/_rels/{file}.rels");
    let Some(original) = text(reference, &rels_name) else {
        return;
    };
    let mut rels = original.clone();
    for rel in RELATIONSHIP.find_iter(&original) {
        let rel = rel.as_str();
        let Some(target) = attr(rel, "Target") else {
            continue;
        };
        if attr(rel, "TargetMode").is_some() {
            continue;
        }
        let source = format!("{dir}/{target}");
        let Some(media) = reference.get(&source) else {
            continue;
        };
        // Named apart, so the body's own images cannot be overwritten.
        let (tdir, tfile) = target.rsplit_once('/').unwrap_or(("", target));
        let renamed = if tdir.is_empty() {
            format!("ref-{tfile}")
        } else {
            format!("{tdir}/ref-{tfile}")
        };
        rels = rels.replace(
            &format!("Target=\"{target}\""),
            &format!("Target=\"{renamed}\""),
        );
        let dest = format!("{dir}/{renamed}");
        parts.insert(dest.clone(), media.clone());
        add_content_type(reference, types, &dest);
    }
    parts.insert(rels_name, rels.into_bytes());
}

/// Declare `name` the way the reference declares it.
fn add_content_type(reference: &BTreeMap<String, Vec<u8>>, types: &mut String, name: &str) {
    let Some(ref_types) = text(reference, CONTENT_TYPES) else {
        return;
    };
    let part = format!("/{name}");
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e);
    // Names come from the reference, so a renamed media part is looked up by
    // the extension it keeps.
    let declaration = TYPE_DECLARATION
        .find_iter(&ref_types)
        .map(|m| m.as_str())
        .find(|d| attr(d, "PartName") == Some(part.as_str()))
        .or_else(|| {
            TYPE_DECLARATION
                .find_iter(&ref_types)
                .map(|m| m.as_str())
                .find(|d| d.starts_with("<Default") && attr(d, "Extension") == Some(ext))
        });
    let Some(declaration) = declaration else {
        return;
    };
    let already = if declaration.starts_with("<Default") {
        types.contains(&format!("Extension=\"{ext}\""))
    } else {
        types.contains(&format!("PartName=\"{part}\""))
    };
    if !already {
        *types = types.replace("</Types>", &format!("{declaration}</Types>"));
    }
}

static TYPE_DECLARATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(?:Default|Override)\b[^>]*?/>").unwrap());

fn text(parts: &BTreeMap<String, Vec<u8>>, name: &str) -> Option<String> {
    parts
        .get(name)
        .and_then(|b| String::from_utf8(b.clone()).ok())
}

/// The value of attribute `name` in the element text `tag`.
fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let len = tag[start..].find('"')?;
    Some(&tag[start..start + len])
}

/// Where the body's closing section properties lie in a document: the last
/// `w:sectPr`, which is the one outside any paragraph.
fn final_section_range(doc: &str) -> Option<std::ops::Range<usize>> {
    let mut at = 0;
    let mut start = None;
    while let Some(i) = doc[at..].find("<w:sectPr") {
        let i = at + i;
        // Not `w:sectPrChange`.
        if matches!(doc.as_bytes().get(i + 9), Some(b' ' | b'>' | b'/')) {
            start = Some(i);
        }
        at = i + 9;
    }
    let start = start?;
    let head_end = start + doc[start..].find('>')?;
    if doc.as_bytes()[head_end - 1] == b'/' {
        return Some(start..head_end + 1);
    }
    let close = "</w:sectPr>";
    let end = head_end + doc[head_end..].find(close)? + close.len();
    Some(start..end)
}

fn final_section(doc: &str) -> Option<String> {
    final_section_range(doc).map(|r| doc[r].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::convert;

    const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const WORD: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml";

    /// A docx as Google Docs exports it, with a page-number footer.
    fn with_footer(base: Vec<u8>) -> Vec<u8> {
        let mut parts = zip::read_map(&base).unwrap();
        let mut doc = text(&parts, DOCUMENT).unwrap();
        let range = final_section_range(&doc).unwrap();
        doc.replace_range(
            range,
            "<w:sectPr><w:footerReference w:type=\"default\" r:id=\"rId9\"/>\
             <w:pgSz w:w=\"12240\" w:h=\"15840\"/><w:pgMar w:top=\"720\"/></w:sectPr>",
        );
        parts.insert(DOCUMENT.into(), doc.into_bytes());
        let rels = text(&parts, DOCUMENT_RELS).unwrap().replace(
            "</Relationships>",
            &format!(
                "<Relationship Id=\"rId9\" Type=\"{REL}/footer\" Target=\"footer1.xml\"/>\
                 </Relationships>"
            ),
        );
        parts.insert(DOCUMENT_RELS.into(), rels.into_bytes());
        let types = text(&parts, CONTENT_TYPES).unwrap().replace(
            "</Types>",
            &format!(
                "<Override PartName=\"/word/footer1.xml\" ContentType=\"{WORD}.footer+xml\"/></Types>"
            ),
        );
        parts.insert(CONTENT_TYPES.into(), types.into_bytes());
        parts.insert(
            "word/footer1.xml".into(),
            b"<w:ftr><w:p><w:r><w:fldChar w:fldCharType=\"begin\"/><w:instrText>PAGE</w:instrText></w:r></w:p></w:ftr>".to_vec(),
        );
        let mut out = ZipArchive::new();
        for (name, data) in &parts {
            out.deflate(name, data).unwrap();
        }
        out.finish().unwrap()
    }

    #[test]
    fn a_footer_in_the_reference_survives_conversion() {
        let google = with_footer(convert::md_to_docx("# Note\n\nv1\n", None).unwrap());
        let next = convert::md_to_docx("# Note\n\nv2 edited in Obsidian\n", Some(google)).unwrap();

        let parts = zip::read_map(&next).unwrap();
        let footer = text(&parts, "word/footer1.xml").expect("footer part carried over");
        assert!(footer.contains("PAGE"));
        let doc = text(&parts, DOCUMENT).unwrap();
        assert!(doc.contains("v2 edited in Obsidian"), "new body kept");
        assert!(!doc.contains("v1"), "old body not kept");
        assert!(doc.contains("<w:pgMar w:top=\"720\"/>"), "page setup kept");
        let id = RELATED_ID.captures(&doc).expect("footer referenced")[1].to_string();
        let rels = text(&parts, DOCUMENT_RELS).unwrap();
        assert!(
            rels.contains(&format!("Id=\"{id}\"")) && rels.contains("Target=\"footer1.xml\""),
            "{rels}"
        );
        assert!(
            text(&parts, CONTENT_TYPES)
                .unwrap()
                .contains("/word/footer1.xml"),
        );
        assert!(convert::docx_to_md(&next).unwrap().contains("v2 edited"));
    }

    #[test]
    fn the_footer_keeps_surviving_every_later_conversion() {
        let mut docx = with_footer(convert::md_to_docx("# A\n", None).unwrap());
        for n in 0..3 {
            docx = convert::md_to_docx(&format!("# A\n\nedit {n}\n"), Some(docx)).unwrap();
        }
        let parts = zip::read_map(&docx).unwrap();
        assert!(parts.contains_key("word/footer1.xml"));
        let doc = text(&parts, DOCUMENT).unwrap();
        assert_eq!(doc.matches("<w:footerReference").count(), 1, "{doc}");
    }

    #[test]
    fn a_reference_without_a_footer_changes_nothing_but_styles() {
        let plain = convert::md_to_docx("# One\n", None).unwrap();
        let next = convert::md_to_docx("# Two\n", Some(plain)).unwrap();
        let parts = zip::read_map(&next).unwrap();
        assert!(!parts.contains_key("word/footer1.xml"));
    }
}
