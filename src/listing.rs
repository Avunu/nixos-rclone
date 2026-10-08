//! Reading and patching rclone bisync's `.lst` listing files.
//!
//! bisync keeps one listing per side, recording each file as it stood after
//! the last sync, and finds changes by comparing against them. It also has no
//! rename tracking: a file moved on one side reads as "deleted here, created
//! there". On a remote like Google Drive that loses the file's identity, its
//! sharing and its history, and `--track-renames` cannot help (it pairs files
//! by size first, and an imported Google Doc reports none).
//!
//! So when the daemon performs a rename on the remote itself, it also renames
//! the entry in both listings. bisync then finds the file unchanged at its new
//! path, or changed only on one side if it was edited as well.
//!
//! A line looks like (see `cmd/bisync/listing.go` in rclone):
//!
//! ```text
//! -        3009805 md5:378840336ab14afa9c6b8d887e68a340 -  2006-01-02T15:04:05.000000000+0000 "12 - Wait.mp3"
//! ```
//!
//! Only the quoted path at the end is ever touched; every other byte of every
//! line is preserved, so an edit cannot corrupt what it does not understand.

use std::path::Path;

use anyhow::{Context, Result};
use unicode_general_category::{GeneralCategory as Gc, get_general_category};

/// An in-memory listing: its lines, verbatim, with each entry's path decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    raw: String,
    /// The decoded path, for an entry line; `None` for the header or anything
    /// unrecognised, which is left alone.
    path: Option<String>,
    /// bisync lists directories too (with `createEmptySrcDirs`), flagged `d`.
    is_dir: bool,
}

impl Listing {
    pub fn parse(text: &str) -> Self {
        let lines = text
            .split_inclusive('\n')
            .map(|raw| {
                let entry = entry_path(raw.trim_end_matches('\n'));
                Line {
                    is_dir: entry.as_ref().is_some_and(|e| e.is_dir),
                    path: entry.map(|e| e.path),
                    raw: raw.to_string(),
                }
            })
            .collect();
        Self { lines }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading listing {}", path.display()))?;
        Ok(Self::parse(&text))
    }

    pub fn to_text(&self) -> String {
        self.lines.iter().map(|l| l.raw.as_str()).collect()
    }

    /// Write back atomically: bisync must never find a half-written listing.
    pub fn save(&self, path: &Path) -> Result<()> {
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, self.to_text())
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }

    /// Whether the listing has a *file* at `path`.
    pub fn contains(&self, path: &str) -> bool {
        self.lines
            .iter()
            .any(|l| !l.is_dir && l.path.as_deref() == Some(path))
    }

    /// Size and modification time recorded for `path`.
    pub fn stat(&self, path: &str) -> Option<EntryStat> {
        let line = self
            .lines
            .iter()
            .find(|l| !l.is_dir && l.path.as_deref() == Some(path))?;
        let mut f = line.raw.split_whitespace();
        let (_flags, size, _hash, _id, time) =
            (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
        Some(EntryStat {
            size: size.parse().ok()?,
            mtime_ns: parse_time_ns(time)?,
        })
    }

    /// Number of files.
    pub fn len(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| !l.is_dir && l.path.is_some())
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every *file* path, in listing order.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.lines
            .iter()
            .filter(|l| !l.is_dir)
            .filter_map(|l| l.path.as_deref())
    }

    /// Every directory path, in listing order.
    pub fn dir_paths(&self) -> impl Iterator<Item = &str> {
        self.lines
            .iter()
            .filter(|l| l.is_dir)
            .filter_map(|l| l.path.as_deref())
    }

    /// Rename the entry `from` to `to`. Returns whether it was present.
    /// `None` if `to` cannot be written faithfully (see [`quote`]); nothing is
    /// changed then.
    pub fn rename(&mut self, from: &str, to: &str) -> Option<bool> {
        let quoted = quote(to)?;
        let mut found = false;
        for l in &mut self.lines {
            if !l.is_dir && l.path.as_deref() == Some(from) {
                // Everything up to the opening quote is kept as it is.
                let head = &l.raw[..l.raw.rfind(" \"").map_or(0, |i| i + 1)];
                let nl = if l.raw.ends_with('\n') { "\n" } else { "" };
                l.raw = format!("{head}{quoted}{nl}");
                l.path = Some(to.to_string());
                found = true;
            }
        }
        Some(found)
    }

    /// Rename the directory entry `from`, and the directory entries beneath
    /// it, to live under `to`. Files are renamed one by one, see [`Self::rename`].
    pub fn rename_dirs(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        for l in &mut self.lines {
            if !l.is_dir {
                continue;
            }
            let Some(old) = l.path.as_deref() else {
                continue;
            };
            let new = if old == from {
                to.to_string()
            } else if let Some(rest) = old.strip_prefix(&prefix) {
                format!("{to}/{rest}")
            } else {
                continue;
            };
            let Some(quoted) = quote(&new) else { continue };
            let head = &l.raw[..l.raw.rfind(" \"").map_or(0, |i| i + 1)];
            let nl = if l.raw.ends_with('\n') { "\n" } else { "" };
            l.raw = format!("{head}{quoted}{nl}");
            l.path = Some(new);
        }
    }

    /// Drop the entry (file or directory) for `path`. Returns whether it was
    /// present.
    pub fn remove(&mut self, path: &str) -> bool {
        let before = self.lines.len();
        self.lines.retain(|l| l.path.as_deref() != Some(path));
        self.lines.len() != before
    }
}

/// What a listing records about a file, enough to tell whether a file on disk
/// is the one bisync last saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryStat {
    pub size: i64,
    /// Nanoseconds since the Unix epoch.
    pub mtime_ns: i128,
}

/// `2006-01-02T15:04:05.000000000-0700` → nanoseconds since the epoch.
fn parse_time_ns(t: &str) -> Option<i128> {
    let (date, rest) = t.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    // The zone is the last 5 characters: +hhmm or -hhmm.
    let (clock, zone) = rest.split_at(rest.len().checked_sub(5)?);
    let sign = match zone.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let off_h: i64 = zone[1..3].parse().ok()?;
    let off_m: i64 = zone[3..5].parse().ok()?;
    let (hms, frac) = clock.split_once('.')?;
    let mut c = hms.split(':');
    let (hh, mm, ss): (i64, i64, i64) = (
        c.next()?.parse().ok()?,
        c.next()?.parse().ok()?,
        c.next()?.parse().ok()?,
    );
    if frac.len() != 9 {
        return None;
    }
    let nanos: i64 = frac.parse().ok()?;

    // Days since 1970-01-01 (Howard Hinnant's civil-from-days, inverted).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    let secs = days * 86_400 + hh * 3600 + mm * 60 + ss - sign * (off_h * 3600 + off_m * 60);
    Some(secs as i128 * 1_000_000_000 + nanos as i128)
}

struct Entry {
    path: String,
    is_dir: bool,
}

/// The decoded path of an entry line, or `None` if it is not one.
fn entry_path(line: &str) -> Option<Entry> {
    // Six fields, the last a quoted string that may itself contain spaces: the
    // path starts at the first `"` that follows the (quote-free) time field.
    let start = line.find(" \"")? + 1;
    let (head, quoted) = line.split_at(start);
    // flags size hash id time: size and hash never contain a quote.
    let mut fields = head.split_whitespace();
    let flags = fields.next()?;
    if fields.count() != 4 {
        return None;
    }
    Some(Entry {
        path: unquote(quoted)?,
        is_dir: flags == "d",
    })
}

// ── Go string quoting ────────────────────────────────────────────────────

/// Go's `strconv.Unquote` for a double-quoted string: the exact inverse of
/// what rclone wrote with `%q`.
pub fn unquote(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut bytes: Vec<u8> = Vec::with_capacity(inner.len());
    let mut it = inner.chars();
    while let Some(c) = it.next() {
        match c {
            '"' | '\n' => return None,
            '\\' => match it.next()? {
                'a' => bytes.push(0x07),
                'b' => bytes.push(0x08),
                'f' => bytes.push(0x0c),
                'n' => bytes.push(b'\n'),
                'r' => bytes.push(b'\r'),
                't' => bytes.push(b'\t'),
                'v' => bytes.push(0x0b),
                '\\' => bytes.push(b'\\'),
                '"' => bytes.push(b'"'),
                'x' => bytes.push(hex(&mut it, 2)? as u8),
                'u' => push_char(&mut bytes, hex(&mut it, 4)?)?,
                'U' => push_char(&mut bytes, hex(&mut it, 8)?)?,
                d @ '0'..='7' => {
                    let mut v = d.to_digit(8)?;
                    for _ in 0..2 {
                        v = v * 8 + it.next()?.to_digit(8)?;
                    }
                    if v > 255 {
                        return None;
                    }
                    bytes.push(v as u8);
                }
                _ => return None,
            },
            c => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    String::from_utf8(bytes).ok()
}

fn hex(it: &mut std::str::Chars<'_>, n: usize) -> Option<u32> {
    let mut v = 0u32;
    for _ in 0..n {
        v = v.checked_mul(16)? + it.next()?.to_digit(16)?;
    }
    Some(v)
}

fn push_char(out: &mut Vec<u8>, cp: u32) -> Option<()> {
    let c = char::from_u32(cp)?;
    let mut buf = [0u8; 4];
    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    Some(())
}

/// Go's `strconv.Quote`: what rclone's `%q` writes. Printable runes stay as
/// they are; the rest are escaped.
///
/// `None` for a string that is not valid UTF-8 territory we can reason about
/// (never the case for `&str`, kept so callers treat quoting as fallible).
pub fn quote(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{07}' => out.push_str("\\a"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0b}' => out.push_str("\\v"),
            c if is_print(c) => out.push(c),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c if (c as u32) < 0x10000 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push_str(&format!("\\U{:08x}", c as u32)),
        }
    }
    out.push('"');
    Some(out)
}

/// Go's `unicode.IsPrint`: letters, marks, numbers, punctuation and symbols,
/// plus the ASCII space. Everything else (other spaces, controls, format
/// characters, unassigned) is escaped.
fn is_print(c: char) -> bool {
    if c == ' ' {
        return true;
    }
    matches!(
        get_general_category(c),
        Gc::UppercaseLetter
            | Gc::LowercaseLetter
            | Gc::TitlecaseLetter
            | Gc::ModifierLetter
            | Gc::OtherLetter
            | Gc::NonspacingMark
            | Gc::SpacingMark
            | Gc::EnclosingMark
            | Gc::DecimalNumber
            | Gc::LetterNumber
            | Gc::OtherNumber
            | Gc::ConnectorPunctuation
            | Gc::DashPunctuation
            | Gc::OpenPunctuation
            | Gc::ClosePunctuation
            | Gc::InitialPunctuation
            | Gc::FinalPunctuation
            | Gc::OtherPunctuation
            | Gc::MathSymbol
            | Gc::CurrencySymbol
            | Gc::ModifierSymbol
            | Gc::OtherSymbol
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "# bisync listing v1 from 2026-10-02T13:13:35.123456789+0000\n";
    const A: &str = "-        1234 md5:378840336ab14afa9c6b8d887e68a340 - 2026-01-01T00:00:00.000000000+0000 \"Quarterly Report.docx\"\n";
    const B: &str =
        "-           3 - - 2026-01-02T03:04:05.123456789+0000 \"Sub Dir/Nested.docx\"\n";

    #[test]
    fn roundtrips_byte_for_byte() {
        let text = format!("{HEADER}{A}{B}");
        let l = Listing::parse(&text);
        assert_eq!(l.to_text(), text);
        assert_eq!(
            l.paths().collect::<Vec<_>>(),
            ["Quarterly Report.docx", "Sub Dir/Nested.docx"]
        );
    }

    #[test]
    fn reads_size_and_time() {
        let l = Listing::parse(&format!("{HEADER}{A}{B}"));
        let a = l.stat("Quarterly Report.docx").unwrap();
        assert_eq!(a.size, 1234);
        // 2026-01-01T00:00:00Z
        assert_eq!(a.mtime_ns, 1_767_225_600 * 1_000_000_000);
        let b = l.stat("Sub Dir/Nested.docx").unwrap();
        assert_eq!(b.size, 3);
        assert_eq!(
            b.mtime_ns,
            (1_767_225_600 + 86_400 + 3 * 3600 + 4 * 60 + 5) * 1_000_000_000 + 123_456_789
        );
        assert!(l.stat("missing").is_none());
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn time_zones_and_the_epoch() {
        assert_eq!(parse_time_ns("1970-01-01T00:00:00.000000000+0000"), Some(0));
        assert_eq!(
            parse_time_ns("1970-01-01T01:00:00.000000000+0100"),
            Some(0),
            "an hour ahead of UTC is the same instant"
        );
        assert_eq!(parse_time_ns("1969-12-31T19:00:00.000000000-0500"), Some(0));
        assert_eq!(parse_time_ns("2024-02-29T12:00:00.5+0000"), None);
        assert_eq!(parse_time_ns("garbage"), None);
    }

    #[test]
    fn rename_touches_only_the_path() {
        let mut l = Listing::parse(&format!("{HEADER}{A}{B}"));
        assert_eq!(
            l.rename("Sub Dir/Nested.docx", "Archive/Nested.docx"),
            Some(true)
        );
        assert_eq!(
            l.to_text(),
            format!(
                "{HEADER}{A}-           3 - - 2026-01-02T03:04:05.123456789+0000 \"Archive/Nested.docx\"\n"
            )
        );
        assert!(!l.contains("Sub Dir/Nested.docx"));
        assert!(l.contains("Archive/Nested.docx"));
    }

    #[test]
    fn rename_and_remove_report_absence() {
        let mut l = Listing::parse(&format!("{HEADER}{A}"));
        assert_eq!(l.rename("nope.docx", "x.docx"), Some(false));
        assert!(!l.remove("nope.docx"));
        assert!(l.remove("Quarterly Report.docx"));
        assert_eq!(l.to_text(), HEADER);
    }

    #[test]
    fn directories_are_listed_but_are_not_files() {
        let dir = "d           0 - - 2026-01-01T00:00:00.000000000+0000 \"Sub Dir\"\n";
        let nested = "d           0 - - 2026-01-01T00:00:00.000000000+0000 \"Sub Dir/inner\"\n";
        let mut l = Listing::parse(&format!("{HEADER}{dir}{nested}{B}"));
        assert_eq!(l.paths().collect::<Vec<_>>(), ["Sub Dir/Nested.docx"]);
        assert_eq!(
            l.dir_paths().collect::<Vec<_>>(),
            ["Sub Dir", "Sub Dir/inner"]
        );
        assert!(!l.contains("Sub Dir"), "a directory is not a file");
        assert!(l.stat("Sub Dir").is_none());
        assert_eq!(l.len(), 1);

        l.rename_dirs("Sub Dir", "Archive/Sub Dir");
        assert_eq!(
            l.dir_paths().collect::<Vec<_>>(),
            ["Archive/Sub Dir", "Archive/Sub Dir/inner"]
        );
        // Files are untouched by a directory rename: they are moved one by one.
        assert!(l.contains("Sub Dir/Nested.docx"));
        assert!(l.remove("Archive/Sub Dir/inner"));
        assert_eq!(l.dir_paths().count(), 1);
    }

    #[test]
    fn unknown_lines_are_preserved() {
        let text = format!("{HEADER}garbage line\n{A}");
        let mut l = Listing::parse(&text);
        l.remove("Quarterly Report.docx");
        assert_eq!(l.to_text(), format!("{HEADER}garbage line\n"));
    }

    #[test]
    fn paths_containing_spaces_and_quotes() {
        // The size field is right-aligned, so a path with " \"" inside must
        // not confuse where the quoted part starts.
        let tricky = "weird \"quoted\" name.docx";
        let line = format!(
            "-          10 - - 2026-01-01T00:00:00.000000000+0000 {}\n",
            quote(tricky).unwrap()
        );
        let l = Listing::parse(&line);
        assert_eq!(l.paths().collect::<Vec<_>>(), [tricky]);
    }

    #[test]
    fn quote_matches_go() {
        // Expected values are what Go's strconv.Quote produces. `@` stands for
        // a backslash, so the escapes read the same as in Go's own tests.
        for (input, want) in [
            ("plain", "\"plain\""),
            ("a b", "\"a b\""),
            ("say \"hi\"", "\"say @\"hi@\"\""),
            ("back\\slash", "\"back@@slash\""),
            ("tab\there", "\"tab@there\""),
            ("new\nline", "\"new@nline\""),
            ("bell\u{7}", "\"bell@a\""),
            ("nul\u{0}", "\"nul@x00\""),
            ("del\u{7f}", "\"del@x7f\""),
            ("caf\u{e9}", "\"caf\u{e9}\""), // printable: kept
            ("\u{201c}smart\u{201d}", "\"\u{201c}smart\u{201d}\""),
            ("emoji \u{1f600}", "\"emoji \u{1f600}\""),
            ("zero\u{200b}width", "\"zero@u200bwidth\""), // Cf: escaped
            ("nbsp\u{a0}x", "\"nbsp@u00a0x\""),           // Zs other than ' ': escaped
            ("line\u{2028}sep", "\"line@u2028sep\""),
            ("\u{e000}private", "\"@ue000private\""),
            ("\u{10ffff}", "\"@U0010ffff\""),
        ] {
            let want = want.replace('@', "\\");
            assert_eq!(quote(input).unwrap(), want, "quoting {input:?}");
        }
    }

    #[test]
    fn unquote_inverts_quote() {
        for s in [
            "plain",
            "a b",
            "say \"hi\"",
            "back\\slash",
            "tab\there",
            "new\nline",
            "nul\u{0}del\u{7f}",
            "caf\u{e9}",
            "\u{201c}smart\u{201d}",
            "emoji 😀",
            "zero\u{200b}width\u{a0}nbsp\u{2028}",
            "\u{e000}\u{10ffff}",
            "",
        ] {
            assert_eq!(unquote(&quote(s).unwrap()).as_deref(), Some(s), "{s:?}");
        }
    }

    #[test]
    fn unquote_understands_what_go_may_write() {
        let go = |s: &str| format!("\"{}\"", s.replace('@', "\\"));
        assert_eq!(
            unquote(&go("@101@x42@u0043@U00000044")).as_deref(),
            Some("ABCD")
        );
        assert_eq!(unquote(&go("caf@u00e9")).as_deref(), Some("caf\u{e9}"));
        // Not valid Go string literals.
        assert_eq!(unquote("\"unterminated"), None);
        assert_eq!(unquote("\"bad\"quote\""), None);
        assert_eq!(unquote(&go("@q")), None);
        assert_eq!(unquote("noquotes"), None);
    }
}
