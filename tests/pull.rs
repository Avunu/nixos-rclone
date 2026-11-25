//! Pulling: the real daemon driving a real `rclone rcd` over local directories.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use rclone_remotes::ctl::{Request, State};

mod common;
use common::{Harness, Options, put, read};

#[tokio::test(flavor = "multi_thread")]
async fn first_sync_resyncs_exactly_once_and_deletions_propagate() {
    let h = Harness::start(Options::default()).await;
    put(&h.local, "a.txt", "a");
    put(&h.local, "b.txt", "b");
    put(&h.local, "Sub Dir/c d.txt", "cd");

    // No listings yet: the daemon must initialise the pair by itself.
    let r = h.sync_ok().await;
    assert_eq!(r.status.resyncs, 1);
    assert_eq!(r.status.state, State::Idle);
    assert!(h.remote.join("a.txt").is_file());
    assert!(h.remote.join("Sub Dir/c d.txt").is_file());

    // Keep b.txt: bisync (correctly) refuses to sync a directory that became
    // completely empty.
    fs::remove_file(h.local.join("a.txt")).unwrap();
    let r = h.sync_ok().await;
    assert!(
        !h.remote.join("a.txt").exists(),
        "deletion did not propagate"
    );
    assert!(h.remote.join("b.txt").is_file());
    assert_eq!(r.status.resyncs, 1, "a later pass must not resync again");
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_changes_are_pulled() {
    let h = Harness::start(Options::default()).await;
    put(&h.local, "keep.txt", "keep");
    // Left alone: bisync aborts if *every* file changed on one side.
    put(&h.local, "untouched.txt", "same");
    h.sync_ok().await;

    put(&h.remote, "from-remote.txt", "hello");
    // Newer than anything the pair has recorded.
    put(&h.remote, "keep.txt", "edited remotely");
    let future = filetime::FileTime::from_unix_time(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            + 60,
        0,
    );
    filetime::set_file_mtime(h.remote.join("keep.txt"), future).unwrap();

    h.sync_ok().await;
    assert_eq!(
        fs::read_to_string(h.local.join("from-remote.txt")).unwrap(),
        "hello"
    );
    assert_eq!(
        fs::read_to_string(h.local.join("keep.txt")).unwrap(),
        "edited remotely"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn excludes_are_honoured() {
    let h = Harness::start(Options::default()).await;
    put(&h.local, "real.txt", "x");
    put(&h.local, "#recycle/trash.txt", "x");
    h.sync_ok().await;
    assert!(h.remote.join("real.txt").is_file());
    assert!(
        !h.remote.join("#recycle").exists(),
        "excluded directory was synced"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn settle_pass_runs_a_second_bisync() {
    let h = Harness::start(Options {
        settle: true,
        ..Default::default()
    })
    .await;
    put(&h.local, "a.txt", "a");
    let first = h.sync_ok().await; // initial resync: no settle pass
    let before = first.status.passes;

    let after = h.sync_ok().await;
    assert_eq!(
        after.status.passes - before,
        2,
        "one pull should run two passes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn critical_lockout_is_reported_and_resync_recovers() {
    let h = Harness::start(Options::default()).await;
    put(&h.local, "a.txt", "a");
    h.sync_ok().await;

    // What rclone does after a critical error: set the listings aside, which
    // locks the pair out until a resync.
    let mut renamed = 0;
    for e in fs::read_dir(&h.workdir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|x| x == "lst") {
            let mut to = p.clone().into_os_string();
            to.push("-err");
            fs::rename(&p, to).unwrap();
            renamed += 1;
        }
    }
    assert!(renamed >= 2, "expected bisync listings in the workdir");

    let r = h.ctl(Request::Sync).await;
    assert!(!r.ok);
    assert_eq!(r.status.state, State::NeedsResync);
    let resyncs = r.status.resyncs;

    // Not retried automatically, and says why.
    let again = h.ctl(Request::Sync).await;
    assert!(!again.ok);
    assert_eq!(again.status.resyncs, resyncs, "must not resync on its own");

    let fixed = h.ctl(Request::Resync).await;
    assert!(fixed.ok, "{:?}", fixed.message);
    assert_eq!(fixed.status.state, State::Idle);
    assert_eq!(fixed.status.resyncs, resyncs + 1);
    h.sync_ok().await;
}

/// The lock file of this pair's bisync session.
fn lock_path(h: &Harness) -> PathBuf {
    let listing = fs::read_dir(&h.workdir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .find(|n| n.ends_with(".path1.lst"))
        .expect("the pair has synced");
    h.workdir
        .join(format!("{}.lck", listing.trim_end_matches(".path1.lst")))
}

#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_empty_lock_no_longer_stops_pulls() {
    let h = Harness::start(Options::default()).await;
    put(&h.local, "a.txt", "a");
    h.sync_ok().await;

    // rclone would honour this forever, whatever maxLock says.
    let lock = lock_path(&h);
    let f = fs::File::create(&lock).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(86_400))
        .unwrap();

    put(&h.remote, "from-remote.txt", "r");
    let r = h.ctl(Request::Sync).await;
    assert!(r.ok, "{:?}", r.message);
    assert_eq!(r.status.state, State::Idle);
    assert!(!lock.exists());
    assert_eq!(read(&h.local, "from-remote.txt").as_deref(), Some("r"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lock_that_may_be_live_is_honoured_and_reported() {
    let h = Harness::start(Options::default()).await;
    put(&h.local, "a.txt", "a");
    h.sync_ok().await;

    let lock = lock_path(&h);
    fs::File::create(&lock).unwrap();
    let r = h.ctl(Request::Sync).await;
    assert!(!r.ok);
    assert_eq!(r.status.state, State::Locked);
    assert!(r.status.failing_since.is_some());
    let error = r.status.last_error.unwrap();
    assert!(error.contains(lock.to_str().unwrap()), "{error}");
    assert!(!error.contains('\u{1b}'), "colour codes: {error:?}");
    assert!(lock.exists(), "must not be removed");

    // Not sticky: once the lock has gone the next pull just works.
    fs::remove_file(&lock).unwrap();
    let r = h.sync_ok().await;
    assert_eq!(r.status.state, State::Idle);
    assert_eq!(r.status.failing_since, None);
}
