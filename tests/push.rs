//! Pushing: local changes must reach the remote without waiting for a pull.
//! The real daemon, a real `rclone rcd`, real inotify; the "remote" is a local
//! directory, where a server-side move is `rename(2)` and so keeps the inode.

use std::fs;
use std::time::Duration;

use rclone_remotes::ctl::Request;

mod common;
use common::{Harness, Options, inode, put, read, tree};

const PUSH: Options = Options {
    settle: false,
    push: true,
    markdown: false,
};

/// A pair that has synced once with `files`, so that pushing is allowed.
async fn synced(files: &[(&str, &str)]) -> Harness {
    let h = Harness::start(PUSH).await;
    for (p, body) in files {
        put(&h.local, p, body);
    }
    h.sync_ok().await;
    // Let the events from setting up (and from the pull's own writes) arrive,
    // so that what a test does next is not merged with them: the debouncer
    // rightly collapses "created, then renamed" into "created".
    tokio::time::sleep(Duration::from_millis(900)).await;
    h
}

#[tokio::test(flavor = "multi_thread")]
async fn create_modify_and_delete_reach_the_remote_without_a_pull() {
    let h = synced(&[("keep.txt", "keep")]).await;

    put(&h.local, "Sub Dir/new file.txt", "one");
    h.eventually("new file on the remote", || {
        read(&h.remote, "Sub Dir/new file.txt").as_deref() == Some("one")
    })
    .await;

    put(&h.local, "Sub Dir/new file.txt", "two, longer");
    h.eventually("modification on the remote", || {
        read(&h.remote, "Sub Dir/new file.txt").as_deref() == Some("two, longer")
    })
    .await;

    fs::remove_file(h.local.join("Sub Dir/new file.txt")).unwrap();
    h.eventually("deletion on the remote", || {
        !h.remote.join("Sub Dir/new file.txt").exists()
    })
    .await;
    assert!(
        h.remote.join("keep.txt").exists(),
        "unrelated file untouched"
    );

    let st = h.ctl(Request::Status).await.status;
    assert!(st.pushed.uploaded >= 2 && st.pushed.deleted >= 1, "{st:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rename_is_a_server_side_move_and_the_next_pull_does_not_undo_it() {
    let h = synced(&[("Old Name.docx", "contents"), ("Other.docx", "other")]).await;
    let ino = inode(&h.remote, "Old Name.docx").unwrap();

    fs::rename(h.local.join("Old Name.docx"), h.local.join("New Name.docx")).unwrap();
    h.eventually("rename on the remote", || {
        h.remote.join("New Name.docx").exists() && !h.remote.join("Old Name.docx").exists()
    })
    .await;
    // Same inode: moved, not deleted and re-created.
    assert_eq!(inode(&h.remote, "New Name.docx"), Some(ino));
    assert_eq!(h.ctl(Request::Status).await.status.pushed.moved, 1);

    // The listings were patched, so bisync finds nothing to do, and above all
    // does not replay the rename as delete + create.
    let before = tree(&h.remote);
    h.sync_ok().await;
    assert_eq!(
        inode(&h.remote, "New Name.docx"),
        Some(ino),
        "remote file was replaced"
    );
    assert_eq!(tree(&h.remote), before);
    assert_eq!(tree(&h.local), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rename_that_is_also_an_edit_lands_in_place() {
    let h = synced(&[("a.txt", "v1"), ("b.txt", "other")]).await;

    fs::rename(h.local.join("a.txt"), h.local.join("Archive.txt")).unwrap();
    put(&h.local, "Archive.txt", "v2, edited as well");
    h.eventually("renamed and edited file on the remote", || {
        read(&h.remote, "Archive.txt").as_deref() == Some("v2, edited as well")
    })
    .await;
    assert!(!h.remote.join("a.txt").exists());
    // Moved on the remote, then updated there. (Not asserted by inode: rclone's
    // local backend replaces a file it updates, where Google Drive keeps its ID.)
    assert_eq!(h.ctl(Request::Status).await.status.pushed.moved, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_directory_rename_moves_every_file_in_it() {
    let h = synced(&[
        ("Proj/a.docx", "a"),
        ("Proj/deep/b.docx", "b"),
        ("Other/c.docx", "c"),
    ])
    .await;
    let (ia, ib) = (
        inode(&h.remote, "Proj/a.docx").unwrap(),
        inode(&h.remote, "Proj/deep/b.docx").unwrap(),
    );

    fs::create_dir_all(h.local.join("Archive")).unwrap();
    fs::rename(h.local.join("Proj"), h.local.join("Archive/Proj")).unwrap();
    h.eventually("directory moved on the remote", || {
        h.remote.join("Archive/Proj/deep/b.docx").exists() && !h.remote.join("Proj").exists()
    })
    .await;
    assert_eq!(inode(&h.remote, "Archive/Proj/a.docx"), Some(ia));
    assert_eq!(inode(&h.remote, "Archive/Proj/deep/b.docx"), Some(ib));

    h.sync_ok().await;
    assert_eq!(
        inode(&h.remote, "Archive/Proj/a.docx"),
        Some(ia),
        "replayed as delete + create"
    );
    assert_eq!(tree(&h.remote), tree(&h.local));
}

#[tokio::test(flavor = "multi_thread")]
async fn files_dropped_into_a_new_directory_are_found() {
    // inotify can only watch a directory once it exists, so files created
    // straight away may produce no events of their own.
    let h = synced(&[("keep.txt", "keep")]).await;
    for i in 0..20 {
        put(&h.local, &format!("bulk/dir/f{i}.txt"), "x");
    }
    h.eventually("all 20 files on the remote", || {
        (0..20).all(|i| h.remote.join(format!("bulk/dir/f{i}.txt")).exists())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pushed_edit_is_not_a_conflict_at_the_next_pull() {
    let h = synced(&[("doc.txt", "v1"), ("other.txt", "o")]).await;
    put(&h.local, "doc.txt", "v2 edited locally");
    h.eventually("edit pushed", || {
        read(&h.remote, "doc.txt").as_deref() == Some("v2 edited locally")
    })
    .await;

    h.sync_ok().await;
    let want = ["doc.txt", "other.txt"];
    assert_eq!(tree(&h.local), want, "conflict debris locally");
    assert_eq!(tree(&h.remote), want, "conflict debris remotely");
    assert_eq!(
        read(&h.local, "doc.txt").as_deref(),
        Some("v2 edited locally")
    );
    assert_eq!(
        read(&h.remote, "doc.txt").as_deref(),
        Some("v2 edited locally")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn what_a_pull_writes_locally_is_not_pushed_back() {
    let h = synced(&[("keep.txt", "keep")]).await;
    let pushed = || async { h.ctl(Request::Status).await.status.pushed };
    let before = pushed().await;

    for i in 0..5 {
        put(&h.remote, &format!("from-remote-{i}.txt"), "hello");
    }
    h.sync_ok().await;
    assert_eq!(tree(&h.local).len(), 6);

    // Longer than the debounce, so the echo events have all arrived.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let after = pushed().await;
    assert_eq!(
        (after.uploaded, after.moved, after.deleted),
        (before.uploaded, before.moved, before.deleted),
        "the pull's own writes were pushed back"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn excluded_files_are_never_pushed() {
    let h = synced(&[("keep.txt", "keep")]).await;
    put(&h.local, "#recycle/trash.txt", "x");
    put(&h.local, "real.txt", "x");
    h.eventually("real file pushed", || h.remote.join("real.txt").exists())
        .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!h.remote.join("#recycle").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_pushed_before_the_pair_has_synced() {
    let h = Harness::start(PUSH).await;
    put(&h.local, "early.txt", "x");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !h.remote.join("early.txt").exists(),
        "pushed into an uninitialised pair"
    );

    // The first pass finds it.
    h.sync_ok().await;
    assert!(h.remote.join("early.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_deletions_is_left_to_the_pull() {
    let names: Vec<String> = (0..20).map(|i| format!("f{i:02}.txt")).collect();
    let files: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "x")).collect();
    let h = synced(&files).await;

    // 18 of 20 gone at once: more than half, and over the storm threshold.
    for n in &names[..18] {
        fs::remove_file(h.local.join(n)).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(tree(&h.remote).len(), 20, "mass deletion was pushed");
    assert_eq!(h.ctl(Request::Status).await.status.pushed.deleted, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn names_rclone_would_encode_wait_for_the_pull_and_arrive_intact() {
    // rclone calls a tab in a name U+2409, and the daemon does not translate:
    // such a file is left to the pull, which does.
    let h = synced(&[("keep.txt", "keep")]).await;
    put(&h.local, "tab\there.txt", "t");
    put(&h.local, "plain.txt", "p");
    h.eventually("the plain file pushed", || {
        h.remote.join("plain.txt").exists()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        tree(&h.remote),
        ["keep.txt", "plain.txt"],
        "the encoded name was pushed"
    );

    h.sync_ok().await;
    let remote = tree(&h.remote);
    assert!(
        remote
            .iter()
            .any(|n| n.starts_with("tab") && n.ends_with("here.txt")),
        "{remote:?}"
    );
    assert_eq!(remote.len(), 3);
    assert_eq!(tree(&h.local).len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn names_that_need_escaping_survive_every_hop() {
    let names = [
        "with space.txt",
        "quote\"d.txt",
        "back\\slash.txt",
        "caf\u{e9} \u{201c}smart\u{201d}.txt",
        "emoji \u{1f600}.txt",
        "#hash & amp=1?.txt",
        "100% [done] {ok} (1).txt",
    ];
    let h = synced(&[("keep.txt", "keep")]).await;
    for n in names {
        put(&h.local, n, n);
    }
    h.eventually("every awkward name on the remote", || {
        names
            .iter()
            .all(|n| read(&h.remote, n).as_deref() == Some(*n))
    })
    .await;

    // And a rename of one of them, which patches the listing by its quoted form.
    let ino = inode(&h.remote, "quote\"d.txt").unwrap();
    fs::rename(
        h.local.join("quote\"d.txt"),
        h.local.join("now \"plain\".txt"),
    )
    .unwrap();
    h.eventually("awkward rename on the remote", || {
        h.remote.join("now \"plain\".txt").exists()
    })
    .await;
    assert_eq!(inode(&h.remote, "now \"plain\".txt"), Some(ino));
    h.sync_ok().await;
    assert_eq!(inode(&h.remote, "now \"plain\".txt"), Some(ino));
}
