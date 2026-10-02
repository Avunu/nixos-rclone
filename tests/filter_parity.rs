//! The watcher's exclude filter must agree with rclone's own, file for file:
//! compare `Filter::includes_file` against `rclone lsf --exclude` over a tree
//! of awkward paths.

use std::collections::BTreeSet;
use std::fs;
use std::process::Command;

use rclone_remotes::filter::Filter;

const PATHS: &[&str] = &[
    "a.txt",
    "Sub Dir/b.docx",
    "recycle/x",
    "notes/#recycle.txt",
    "#recycle/old.docx",
    "#recycle/deep/er/x",
    "share/#recycle/x",
    "@eaDir/SYNOFILE_THUMB.jpg",
    "docs/@eaDir/x/y.jpg",
    "docs/@eaDir.txt",
    "$RECYCLE.BIN/x",
    ".DS_Store",
    "docs/.DS_Store",
    "docs/Thumbs.db",
    ".AppleDouble/inside.txt",
    ".AppleDouble.txt",
    ".Trashes/501/x",
    "root-only.txt",
    "sub/root-only.txt",
    "scratch.tmp",
    "any/where/scratch.tmp",
    "scratch.tmp.keep",
    "build/out/o.bin",
    "src/build/o.bin",
    "abc",
    "a/c",
    "a-c",
    "y.log",
    "z.log",
    "cache/a",
    "x/cache/a/b",
    "cache.txt",
    "old.bak1",
    "old.bak",
    "UPPER.TMP",
    "unicode/caf\u{e9}.tmp",
    "weird [name].txt",
    "brace{x}.txt",
];

const PATTERNS: &[&str] = &[
    ".AppleDouble",
    ".DS_Store",
    ".Spotlight-V100",
    ".Trashes",
    "@eaDir/**",
    "#recycle/**",
    "$RECYCLE.BIN/**",
    "Thumbs.db",
    "/root-only.txt",
    "*.tmp",
    "build/**",
    "a?c",
    "{x,y}.log",
    "cache/",
    "{{.*\\.bak[0-9]}}",
    "weird \\[name\\].txt",
];

#[test]
fn filter_agrees_with_rclone_lsf() {
    let tmp = tempfile::tempdir().unwrap();
    for p in PATHS {
        let path = tmp.path().join(p);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }

    let mut cmd = Command::new("rclone");
    cmd.args(["lsf", "-R", "--files-only"]);
    for pat in PATTERNS {
        cmd.arg("--exclude").arg(pat);
    }
    let out = cmd.arg(tmp.path()).output().expect("rclone on PATH");
    assert!(
        out.status.success(),
        "rclone lsf failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let theirs: BTreeSet<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();

    let f = Filter::new(PATTERNS).unwrap();
    let ours: BTreeSet<String> = PATHS
        .iter()
        .filter(|p| f.includes_file(p))
        .map(|p| p.to_string())
        .collect();

    assert_eq!(
        ours,
        theirs,
        "\nonly we keep:    {:?}\nonly rclone keeps: {:?}",
        ours.difference(&theirs).collect::<Vec<_>>(),
        theirs.difference(&ours).collect::<Vec<_>>()
    );
}
