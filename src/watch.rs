//! Turning filesystem events into changes the push engine understands.
//!
//! Events only ever say *that something happened to a path*. What the daemon
//! does about it is decided later, from the state of the disk at that moment
//! (see [`crate::push`]): a file created and deleted within one debounce window
//! was never there, and a file deleted then recreated is an overwrite.
//!
//! The one thing an event does carry that the disk cannot is a rename: that
//! `a` became `b`, and not that `a` vanished and `b` appeared.

use std::path::{Path, PathBuf};

use notify::EventKind;
use notify::event::{ModifyKind, RenameMode};
use notify_debouncer_full::DebouncedEvent;

/// One thing worth acting on, with paths relative to the sync root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Something happened to this path; look at the disk to see what.
    Dirty(String),
    /// `from` became `to` (a file or a directory).
    Rename { from: String, to: String },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Normalized {
    pub changes: Vec<Change>,
    /// The kernel dropped events (queue overflow) or the watcher lost track:
    /// what changed is unknown, and only a full pass can find out.
    pub rescan: bool,
}

/// Whether the daemon pushes a file of this name itself.
///
/// rclone does not use a file's on-disk name internally: it presents control
/// characters as Unicode "control pictures" (a tab becomes `\u{2409}`), and
/// quotes names that already contain one. Its listings and API speak that
/// form, so for such names the on-disk path and rclone's path differ.
/// Reimplementing the encoder to bridge them would be a second source of
/// truth; these names are rare, and the next pull handles them correctly, so
/// they simply wait for it.
pub fn pushable(rel: &str) -> bool {
    // `.name.tmp` is how this daemon stages a file before renaming it into
    // place; whoever else leaves such a file around is not syncing it either.
    let leaf = rel.rsplit('/').next().unwrap_or(rel);
    if leaf.starts_with('.') && leaf.ends_with(".tmp") {
        return false;
    }
    !rel.chars()
        .any(|c| c.is_control() || ('\u{2400}'..='\u{2421}').contains(&c) || c == '\u{201b}')
}

/// `root`-relative `/`-separated form of `path`, if it is under `root`, is
/// valid UTF-8, and is not under any of `ignore`.
fn relative(path: &Path, root: &Path, ignore: &[PathBuf]) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    if ignore.iter().any(|i| path.starts_with(i)) {
        return None;
    }
    // A name that is not UTF-8 cannot be sent through the rc API as it is.
    // bisync's own pass copes with it, so leave it to that.
    let s = rel.to_str()?;
    if s.is_empty() || !pushable(s) {
        return None;
    }
    Some(s.replace(std::path::MAIN_SEPARATOR, "/"))
}

pub fn normalize(events: &[DebouncedEvent], root: &Path, ignore: &[PathBuf]) -> Normalized {
    let mut out = Normalized::default();
    for ev in events {
        let paths = &ev.event.paths;
        match &ev.event.kind {
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) if paths.len() == 2 => {
                match (
                    relative(&paths[0], root, ignore),
                    relative(&paths[1], root, ignore),
                ) {
                    (Some(from), Some(to)) => out.changes.push(Change::Rename { from, to }),
                    // Moved across the boundary of what we watch or filter: it
                    // appeared or disappeared as far as the remote is concerned.
                    (Some(p), None) | (None, Some(p)) => out.changes.push(Change::Dirty(p)),
                    (None, None) => {}
                }
            }
            EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(_)
            | EventKind::Any
            | EventKind::Other => {
                if ev.event.need_rescan() {
                    out.rescan = true;
                }
                for p in paths {
                    if let Some(rel) = relative(p, root, ignore) {
                        out.changes.push(Change::Dirty(rel));
                    }
                }
            }
            // Reads and opens do not change anything.
            EventKind::Access(_) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::Event;
    use notify::event::{CreateKind, DataChange, RemoveKind};
    use std::time::Instant;

    fn ev(kind: EventKind, paths: &[&str]) -> DebouncedEvent {
        let mut e = Event::new(kind);
        for p in paths {
            e = e.add_path(PathBuf::from(p));
        }
        DebouncedEvent::new(e, Instant::now())
    }

    const ROOT: &str = "/home/alice/sync";

    fn run(events: &[DebouncedEvent]) -> Normalized {
        normalize(
            events,
            Path::new(ROOT),
            &[PathBuf::from("/home/alice/sync/.cache")],
        )
    }

    #[test]
    fn creates_modifies_and_removes_are_dirty() {
        let n = run(&[
            ev(
                EventKind::Create(CreateKind::File),
                &["/home/alice/sync/a.txt"],
            ),
            ev(
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                &["/home/alice/sync/Sub Dir/b.txt"],
            ),
            ev(
                EventKind::Remove(RemoveKind::File),
                &["/home/alice/sync/c.txt"],
            ),
        ]);
        assert_eq!(
            n.changes,
            vec![
                Change::Dirty("a.txt".into()),
                Change::Dirty("Sub Dir/b.txt".into()),
                Change::Dirty("c.txt".into()),
            ]
        );
        assert!(!n.rescan);
    }

    #[test]
    fn a_joined_rename_is_a_rename() {
        let n = run(&[ev(
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[
                "/home/alice/sync/Old.docx",
                "/home/alice/sync/Archive/New.docx",
            ],
        )]);
        assert_eq!(
            n.changes,
            vec![Change::Rename {
                from: "Old.docx".into(),
                to: "Archive/New.docx".into()
            }]
        );
    }

    #[test]
    fn a_rename_across_the_root_is_just_a_change() {
        // Moved in from outside: appeared. Moved out: vanished.
        let n = run(&[
            ev(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/elsewhere/x.txt", "/home/alice/sync/x.txt"],
            ),
            ev(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/home/alice/sync/y.txt", "/elsewhere/y.txt"],
            ),
        ]);
        assert_eq!(
            n.changes,
            vec![Change::Dirty("x.txt".into()), Change::Dirty("y.txt".into())]
        );
    }

    #[test]
    fn unpaired_rename_halves_are_dirty() {
        let n = run(&[
            ev(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &["/home/alice/sync/gone.txt"],
            ),
            ev(
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                &["/home/alice/sync/arrived.txt"],
            ),
        ]);
        assert_eq!(
            n.changes,
            vec![
                Change::Dirty("gone.txt".into()),
                Change::Dirty("arrived.txt".into())
            ]
        );
    }

    #[test]
    fn ignored_and_outside_paths_are_dropped() {
        let n = run(&[
            ev(
                EventKind::Create(CreateKind::File),
                &["/home/alice/sync/.cache/x"],
            ),
            ev(EventKind::Create(CreateKind::File), &["/elsewhere/z"]),
            ev(EventKind::Create(CreateKind::Folder), &[ROOT]),
        ]);
        assert!(n.changes.is_empty(), "{:?}", n.changes);
    }

    #[test]
    fn non_utf8_names_are_left_to_bisync() {
        use std::os::unix::ffi::OsStrExt;
        let mut p = PathBuf::from(ROOT);
        p.push(std::ffi::OsStr::from_bytes(b"bad\xff.txt"));
        let e = Event::new(EventKind::Create(CreateKind::File)).add_path(p);
        let n = run(&[DebouncedEvent::new(e, Instant::now())]);
        assert!(n.changes.is_empty());
    }

    #[test]
    fn names_rclone_would_encode_are_left_to_the_pull() {
        assert!(pushable("plain \"quoted\" caf\u{e9} \u{1f600}.txt"));
        for bad in [
            "tab\there",
            "bell\u{7}",
            "del\u{7f}",
            "nl\nx",
            "\u{2409}picture",
            "q\u{201b}x",
        ] {
            assert!(!pushable(bad), "{bad:?}");
        }
        let n = run(&[ev(
            EventKind::Create(CreateKind::File),
            &["/home/alice/sync/tab\there.txt", "/home/alice/sync/ok.txt"],
        )]);
        assert_eq!(n.changes, vec![Change::Dirty("ok.txt".into())]);
    }

    #[test]
    fn the_daemons_own_staging_files_are_never_pushed() {
        assert!(!pushable(".note.docx.tmp"));
        assert!(!pushable("Sub Dir/.Hello World.docx.tmp"));
        assert!(
            pushable("notes.tmp"),
            "an ordinary file that happens to end in .tmp"
        );
        assert!(pushable(".hidden.docx"));
    }

    #[test]
    fn reads_are_ignored_and_overflow_asks_for_a_rescan() {
        let n = run(&[ev(
            EventKind::Access(notify::event::AccessKind::Read),
            &["/home/alice/sync/a.txt"],
        )]);
        assert!(n.changes.is_empty());

        let overflow = Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        let n = run(&[DebouncedEvent::new(overflow, Instant::now())]);
        assert!(n.rescan);
    }
}
