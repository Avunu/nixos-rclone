//! markdownSync end to end: a vault of notes, the docx tree, and the remote,
//! with the real daemon, a real `rclone rcd` and real inotify.

use std::fs;
use std::time::Duration;

use filetime::{FileTime, set_file_mtime};
use rclone_remotes::markdown::convert;

mod common;
use common::{Harness, Options, inode, put, read, tree};

const LIVE: Options = Options {
    settle: false,
    push: true,
    markdown: true,
};
/// Pulls only: the vault is handled when a pull runs.
const PULL_ONLY: Options = Options {
    settle: false,
    push: false,
    markdown: true,
};

fn note_from_docx(path: &std::path::Path) -> String {
    convert::docx_file_to_md(path).unwrap()
}

/// A pair that has synced once with these notes, and settled.
async fn synced(opts: Options, notes: &[(&str, &str)]) -> Harness {
    let h = Harness::start(opts).await;
    for (name, body) in notes {
        put(&h.vault, name, body);
    }
    h.sync_ok().await;
    tokio::time::sleep(Duration::from_millis(900)).await;
    h
}

#[tokio::test(flavor = "multi_thread")]
async fn notes_become_docx_on_the_remote() {
    let h = synced(
        LIVE,
        &[
            ("Hello World.md", "# Hello\n\nbody\n"),
            ("Meeting Notes/Jan Session.md", "# Jan\n"),
        ],
    )
    .await;
    assert_eq!(
        tree(&h.remote),
        ["Hello World.docx", "Meeting Notes/Jan Session.docx"]
    );
    assert_eq!(tree(&h.local), tree(&h.remote));
    assert_eq!(
        note_from_docx(&h.remote.join("Hello World.docx")),
        "# Hello\n\nbody\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edited_note_reaches_the_remote_at_once() {
    let h = synced(
        LIVE,
        &[("note.md", "# Note\n\nv1\n"), ("other.md", "# Other\n")],
    )
    .await;
    put(&h.vault, "note.md", "# Note\n\nv2 edited in the vault\n");
    h.eventually("the edit as docx on the remote", || {
        h.remote.join("note.docx").exists()
            && note_from_docx(&h.remote.join("note.docx")).contains("v2 edited")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_note_reaches_the_remote_at_once() {
    let h = synced(LIVE, &[("seed.md", "# Seed\n")]).await;
    put(&h.vault, "Brand New/Fresh Note.md", "# Fresh\n");
    h.eventually("the new note's docx on the remote", || {
        h.remote.join("Brand New/Fresh Note.docx").exists()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renamed_note_is_a_server_side_move_of_its_docx() {
    let h = synced(
        LIVE,
        &[("Statement on Roles.md", "# S\n"), ("keep.md", "# K\n")],
    )
    .await;
    let ino = inode(&h.remote, "Statement on Roles.docx").unwrap();

    fs::rename(
        h.vault.join("Statement on Roles.md"),
        h.vault.join("Scriptural Basis.md"),
    )
    .unwrap();
    h.eventually("renamed docx on the remote", || {
        h.remote.join("Scriptural Basis.docx").exists()
            && !h.remote.join("Statement on Roles.docx").exists()
    })
    .await;
    // The same remote file, not a new upload: on Google Drive, the same file
    // ID, sharing and history.
    assert_eq!(inode(&h.remote, "Scriptural Basis.docx"), Some(ino));

    // And the next pull leaves it alone, and brings no duplicate back.
    h.sync_ok().await;
    assert_eq!(inode(&h.remote, "Scriptural Basis.docx"), Some(ino));
    assert_eq!(tree(&h.remote), ["Scriptural Basis.docx", "keep.docx"]);
    assert_eq!(tree(&h.vault), ["Scriptural Basis.md", "keep.md"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_note_moved_into_a_new_folder_and_a_whole_folder_move() {
    let h = synced(
        LIVE,
        &[
            ("Proj/a.md", "# A\n"),
            ("Proj/deep/b.md", "# B\n"),
            ("keep.md", "# K\n"),
        ],
    )
    .await;
    let (ia, ib) = (
        inode(&h.remote, "Proj/a.docx").unwrap(),
        inode(&h.remote, "Proj/deep/b.docx").unwrap(),
    );

    fs::create_dir_all(h.vault.join("Archive")).unwrap();
    fs::rename(h.vault.join("Proj"), h.vault.join("Archive/Proj")).unwrap();
    h.eventually("folder moved on the remote", || {
        h.remote.join("Archive/Proj/deep/b.docx").exists() && !h.remote.join("Proj").exists()
    })
    .await;
    assert_eq!(inode(&h.remote, "Archive/Proj/a.docx"), Some(ia));
    assert_eq!(inode(&h.remote, "Archive/Proj/deep/b.docx"), Some(ib));
    h.sync_ok().await;
    assert_eq!(
        tree(&h.vault),
        ["Archive/Proj/a.md", "Archive/Proj/deep/b.md", "keep.md"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_note_is_deleted_everywhere() {
    let h = synced(
        LIVE,
        &[
            ("gone.md", "# G\n"),
            ("keep.md", "# K\n"),
            ("also.md", "# A\n"),
        ],
    )
    .await;
    fs::remove_file(h.vault.join("gone.md")).unwrap();
    h.eventually("the docx gone from the remote", || {
        !h.remote.join("gone.docx").exists()
    })
    .await;
    assert!(!h.local.join("gone.docx").exists());
    assert!(h.remote.join("keep.docx").exists());
    h.sync_ok().await;
    assert_eq!(tree(&h.vault), ["also.md", "keep.md"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_changes_come_back_as_notes() {
    let h = synced(
        LIVE,
        &[("note.md", "# Note\n\nv1\n"), ("other.md", "# O\n")],
    )
    .await;

    // Someone edits the document on the remote, and adds another.
    let edited = convert::md_to_docx("# Note\n\nv2 from Google Docs\n", None).unwrap();
    fs::write(h.remote.join("note.docx"), edited).unwrap();
    let future = FileTime::from_unix_time(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 120,
        0,
    );
    set_file_mtime(h.remote.join("note.docx"), future).unwrap();
    fs::write(
        h.remote.join("added remotely.docx"),
        convert::md_to_docx("# Added\n", None).unwrap(),
    )
    .unwrap();

    h.sync_ok().await;
    assert_eq!(
        read(&h.vault, "note.md").as_deref(),
        Some("# Note\n\nv2 from Google Docs\n")
    );
    assert_eq!(
        read(&h.vault, "added remotely.md").as_deref(),
        Some("# Added\n")
    );

    // What the pull wrote into the vault is not converted back and pushed again.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        note_from_docx(&h.remote.join("note.docx")),
        "# Note\n\nv2 from Google Docs\n"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_remote_move_moves_the_note() {
    let h = synced(LIVE, &[("Old Name.md", "# Old\n"), ("keep.md", "# K\n")]).await;
    fs::create_dir_all(h.remote.join("Archive")).unwrap();
    fs::rename(
        h.remote.join("Old Name.docx"),
        h.remote.join("Archive/New Name.docx"),
    )
    .unwrap();
    h.sync_ok().await;
    assert_eq!(tree(&h.vault), ["Archive/New Name.md", "keep.md"]);
    assert_eq!(tree(&h.local), ["Archive/New Name.docx", "keep.docx"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_watcher_a_pull_still_follows_a_vault_rename_to_the_remote() {
    // Pushing off, so nothing sees the rename as it happens: the pull must
    // find it by file identity, even though the note was edited as well.
    let h = synced(
        PULL_ONLY,
        &[
            ("Draft.md", "# Draft\n"),
            ("Other.md", "# O\n"),
            ("Third.md", "# T\n"),
        ],
    )
    .await;
    let ino = inode(&h.remote, "Draft.docx").unwrap();

    fs::rename(h.vault.join("Draft.md"), h.vault.join("Final.md")).unwrap();
    put(&h.vault, "Final.md", "# Draft\n\nedited as well\n");

    h.sync_ok().await;
    assert_eq!(tree(&h.remote), ["Final.docx", "Other.docx", "Third.docx"]);
    // (Not asserted by inode here: the edit rewrites the file, and rclone's
    // local backend replaces a file it updates.)
    let _ = ino;
    assert!(note_from_docx(&h.remote.join("Final.docx")).contains("edited as well"));
    assert_eq!(tree(&h.vault), ["Final.md", "Other.md", "Third.md"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn hidden_vault_folders_are_left_alone() {
    let h = synced(LIVE, &[("note.md", "# N\n")]).await;
    put(&h.vault, ".obsidian/workspace.md", "# not a note\n");
    put(&h.vault, ".trash/old.md", "# trash\n");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    h.sync_ok().await;
    assert_eq!(tree(&h.remote), ["note.docx"]);
}
