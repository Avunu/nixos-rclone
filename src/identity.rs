//! File identity, to recognise a rename the watcher did not see.
//!
//! Events can be lost (queue overflow, the daemon was down) or arrive unpaired
//! (`mkdir new && mv old new/`: the destination directory is not watched yet
//! when the file lands in it). Either way a synced file vanishes from one path
//! and a file appears at another, which is indistinguishable from a delete plus
//! a create, except that a rename keeps the file's *inode* and *birth time*.
//!
//! Both are needed: filesystems reuse inode numbers, so a new file that
//! inherits a deleted one's would otherwise be "followed" into its
//! predecessor's remote copy, sharing and history included.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use rustix::fs::{AtFlags, CWD, StatxFlags, statx};

use crate::filter::Filter;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    pub ino: u64,
    /// Birth time in nanoseconds since the epoch.
    pub birth_ns: i128,
}

/// Identity of the regular file at `path`, not following symlinks. `None` if
/// the filesystem does not record birth times, which disables pairing for it.
pub fn file_id(path: &Path) -> Option<FileId> {
    let s = statx(
        CWD,
        path,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::INO | StatxFlags::BTIME | StatxFlags::TYPE,
    )
    .ok()?;
    if !StatxFlags::from_bits_truncate(s.stx_mask).contains(StatxFlags::BTIME) {
        return None;
    }
    // Regular files only.
    if s.stx_mode as u32 & 0o170000 != 0o100000 {
        return None;
    }
    Some(FileId {
        ino: s.stx_ino,
        birth_ns: s.stx_btime.tv_sec as i128 * 1_000_000_000 + s.stx_btime.tv_nsec as i128,
    })
}

/// The identity of every synced-looking file, by path, as of when it was last
/// looked at.
#[derive(Debug, Default, Clone)]
pub struct Snapshot {
    by_path: HashMap<String, FileId>,
}

impl Snapshot {
    /// Walk `root`, recording every file the filter keeps.
    pub fn scan(root: &Path, filter: &Filter) -> Self {
        let mut s = Self::default();
        s.walk(root, "", filter);
        s
    }

    fn walk(&mut self, dir: &Path, rel: &str, filter: &Filter) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let child = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if !crate::watch::pushable(&child) {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if filter.includes_dir(&child) {
                    self.walk(&entry.path(), &child, filter);
                }
            } else if ft.is_file()
                && filter.includes_file(&child)
                && let Some(id) = file_id(&entry.path())
            {
                self.by_path.insert(child, id);
            }
        }
    }

    pub fn get(&self, path: &str) -> Option<FileId> {
        self.by_path.get(path).copied()
    }

    pub fn set(&mut self, path: &str, id: Option<FileId>) {
        match id {
            Some(id) => {
                self.by_path.insert(path.to_string(), id);
            }
            None => {
                self.by_path.remove(path);
            }
        }
    }

    pub fn remove(&mut self, path: &str) {
        self.by_path.remove(path);
    }

    pub fn rename(&mut self, from: &str, to: &str) {
        if let Some(id) = self.by_path.remove(from) {
            self.by_path.insert(to.to_string(), id);
        }
    }

    pub fn len(&self) -> usize {
        self.by_path.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter() -> Filter {
        Filter::new(&["skip/**"]).unwrap()
    }

    #[test]
    fn a_rename_keeps_identity_and_a_copy_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::write(r.join("a.txt"), "x").unwrap();
        let before = file_id(&r.join("a.txt")).expect("this filesystem records birth times");

        fs::rename(r.join("a.txt"), r.join("b.txt")).unwrap();
        assert_eq!(file_id(&r.join("b.txt")), Some(before));

        fs::copy(r.join("b.txt"), r.join("c.txt")).unwrap();
        assert_ne!(file_id(&r.join("c.txt")), Some(before));
    }

    #[test]
    fn scan_records_kept_files_by_relative_path() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("d/e")).unwrap();
        fs::create_dir_all(r.join("skip")).unwrap();
        fs::write(r.join("top.txt"), "x").unwrap();
        fs::write(r.join("d/e/deep.txt"), "x").unwrap();
        fs::write(r.join("skip/no.txt"), "x").unwrap();

        let s = Snapshot::scan(r, &filter());
        assert_eq!(s.len(), 2);
        assert!(s.get("top.txt").is_some());
        assert!(s.get("d/e/deep.txt").is_some());
        assert!(s.get("skip/no.txt").is_none());
    }

    #[test]
    fn directories_and_symlinks_have_no_file_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir(r.join("d")).unwrap();
        fs::write(r.join("f"), "x").unwrap();
        std::os::unix::fs::symlink("f", r.join("link")).unwrap();
        assert!(file_id(&r.join("d")).is_none());
        assert!(file_id(&r.join("link")).is_none());
        assert!(file_id(&r.join("missing")).is_none());
    }

    #[test]
    fn rename_and_remove_follow_the_file() {
        let mut s = Snapshot::default();
        let id = FileId {
            ino: 7,
            birth_ns: 42,
        };
        s.set("a", Some(id));
        s.rename("a", "b");
        assert_eq!((s.get("a"), s.get("b")), (None, Some(id)));
        s.remove("b");
        assert!(s.is_empty());
    }
}
