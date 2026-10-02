//! My reading and writing of bisync listings, checked against the real thing:
//! rclone (Go) writes a listing of awkward file names, and `Listing` must read
//! every path back and reproduce the exact quoting.

use std::collections::BTreeSet;

use rclone_remotes::bisync::listing_paths;
use rclone_remotes::listing::{Listing, quote};

mod common;
use common::{Harness, Options, put};

fn awkward_names() -> Vec<String> {
    vec![
        "plain.txt".into(),
        "with space.txt".into(),
        "quote\"inside.txt".into(),
        "back\\slash.txt".into(),
        "tab\there.txt".into(),
        "caf\u{e9}.txt".into(),
        "e\u{301}combining.txt".into(),
        "\u{201c}smart\u{201d} \u{2013} dash.txt".into(),
        "emoji \u{1f600}\u{1f3f3}\u{fe0f}\u{200d}\u{1f308}.txt".into(),
        "\u{65e5}\u{672c}\u{8a9e}.txt".into(),
        "\u{5e2}\u{5d1}\u{5e8}\u{5d9}\u{5ea}.txt".into(),
        "zero\u{200b}width.txt".into(),
        "nbsp\u{a0}here.txt".into(),
        "line\u{2028}sep.txt".into(),
        "private\u{e000}use.txt".into(),
        "bell\u{7}.txt".into(),
        "del\u{7f}.txt".into(),
        "#hash & amp=1?.txt".into(),
        "Sub Dir/nested \"q\"/file.txt".into(),
        "100% [done] {ok} (1).txt".into(),
    ]
}

/// How rclone presents a local file name: control characters become the
/// matching Unicode control picture (U+2400 + code point; DEL is U+2421).
fn rclone_form(name: &str) -> String {
    name.chars()
        .map(|c| match c as u32 {
            n @ 0..=0x1f => char::from_u32(0x2400 + n).unwrap(),
            0x7f => '\u{2421}',
            _ => c,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn we_read_and_write_listings_exactly_as_rclone_does() {
    let h = Harness::start(Options::default()).await;
    let names = awkward_names();
    for n in &names {
        put(&h.local, n, n);
    }
    h.sync_ok().await;

    let (l1, l2) = listing_paths(&h.cfg).expect("listing paths derived from the config");
    for lst in [l1, l2] {
        let text = std::fs::read_to_string(&lst).unwrap();
        let listing = Listing::parse(&text);

        // Every path decodes to the real file name, except that rclone
        // presents control characters as Unicode control pictures.
        let got: BTreeSet<String> = listing.paths().map(str::to_string).collect();
        let want: BTreeSet<String> = names.iter().map(|n| rclone_form(n)).collect();
        assert_eq!(got, want, "listing {} decoded differently", lst.display());
        // Directories are listed too, and are not files.
        let dirs: Vec<&str> = listing.dir_paths().collect();
        assert_eq!(dirs, ["Sub Dir", "Sub Dir/nested \"q\""]);

        // And quoting a path gives back exactly what Go wrote.
        for line in text.lines().skip(1) {
            let start = line.find(" \"").unwrap() + 1;
            let (head, quoted) = line.split_at(start);
            let path = rclone_remotes::listing::unquote(quoted).unwrap();
            assert_eq!(
                quote(&path).unwrap(),
                quoted,
                "quoting {path:?} differs from Go's"
            );
            assert!(
                head.split_whitespace().count() == 5,
                "unexpected line shape: {line}"
            );
        }

        // Reading then writing changes nothing.
        assert_eq!(listing.to_text(), text);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_listing_patched_by_us_is_accepted_by_rclone() {
    // The strongest check of all: rename an entry in both listings the way the
    // daemon does, move the file on both sides, and have real bisync read our
    // edited listings and find nothing to do.
    let h = Harness::start(Options::default()).await;
    let from = "Statement on \"Gender\" Roles \u{2013} caf\u{e9}.docx";
    let to = "Archive/Scriptural Basis.docx";
    put(&h.local, from, "contents");
    put(&h.local, "keep.docx", "keep");
    h.sync_ok().await;

    let (l1, l2) = listing_paths(&h.cfg).unwrap();
    for lst in [&l1, &l2] {
        let mut l = Listing::load(lst).unwrap();
        assert_eq!(l.rename(from, to), Some(true));
        l.save(lst).unwrap();
    }
    for dir in [&h.local, &h.remote] {
        std::fs::create_dir_all(dir.join("Archive")).unwrap();
        std::fs::rename(dir.join(from), dir.join(to)).unwrap();
    }
    let ino = common::inode(&h.remote, to).unwrap();

    let r = h.sync_ok().await;
    assert!(r.ok);
    assert_eq!(
        common::inode(&h.remote, to),
        Some(ino),
        "bisync re-created the file"
    );
    assert_eq!(common::tree(&h.local), common::tree(&h.remote));
    assert_eq!(
        common::tree(&h.local),
        ["Archive/Scriptural Basis.docx", "keep.docx"]
    );
}
