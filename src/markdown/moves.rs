//! Following moves between the markdown tree and the docx tree.
//!
//! The two trees are projected onto each other *by path*, so relocating a file
//! on one side reads as "deleted here, created there" on the other, and the
//! stale counterpart regenerates the document at its old path at the next
//! conversion: the file ends up at both paths, in both trees, for good. This
//! pairs the orphaned counterpart with the newly appeared file and says how to
//! move it, so the relocation is followed instead of duplicated.
//!
//! A port of `mirror_moves` from the shell implementation. Three passes, in
//! this order, each acting only on unambiguous 1:1 matches:
//!
//! 1. **identity**: the source file's inode and birth time, which a rename
//!    keeps however much the file was edited around it. The destination side
//!    is regenerated or re-downloaded rather than renamed, so this needs a
//!    snapshot of which file used to sit at the orphan's path.
//! 2. **basename**: survives a relocation, even if the file was edited in transit.
//! 3. **mtime**: survives a rename, which changes the basename but not the
//!    timestamp (conversion copies the source's mtime onto its output).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::identity::{FileId, file_id};

/// A path without its extension, relative to the tree, `/`-separated.
pub type Stem = String;

/// Every file under `dir` with extension `ext` (`.md`), by stem. Hidden files
/// and directories (`.obsidian`, `.trash`, …) are not part of the tree.
pub fn scan(dir: &Path, ext: &str) -> BTreeMap<Stem, PathBuf> {
    let mut out = BTreeMap::new();
    walk(dir, "", ext, &mut out);
    out
}

fn walk(dir: &Path, rel: &str, ext: &str, out: &mut BTreeMap<Stem, PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if ft.is_dir() {
            walk(&entry.path(), &child, ext, out);
        } else if ft.is_file()
            && let Some(stem) = child.strip_suffix(ext)
            && !stem.is_empty()
            && !stem.ends_with('/')
        {
            out.insert(stem.to_string(), entry.path());
        }
    }
}

/// A move to perform in the destination tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Move {
    pub from: Stem,
    pub to: Stem,
    /// Which pass paired them (for the log).
    pub by: &'static str,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pairing {
    pub moves: Vec<Move>,
    /// Destination files whose source counterpart vanished and could not be
    /// matched: a genuine delete, or renamed *and* edited in one window.
    pub unpaired_orphans: Vec<Stem>,
    /// Source files with no destination counterpart yet and no match.
    pub unpaired_new: Vec<Stem>,
}

/// Pair orphaned destination files with newly appeared source files.
///
/// `prev_ids` is the identity snapshot of the source tree from the last run
/// (see [`snapshot`]), keyed by stem; empty disables the identity pass.
pub fn pair(
    src_dir: &Path,
    src_ext: &str,
    dst_dir: &Path,
    dst_ext: &str,
    prev_ids: &HashMap<Stem, FileId>,
) -> Pairing {
    let src = scan(src_dir, src_ext);
    let dst = scan(dst_dir, dst_ext);

    // A move shows up as exactly one orphan and one new file. An empty source
    // (an unmounted vault) yields no new stems, so a missing side can never
    // set off a wave of moves.
    let mut orphans: Vec<Option<Stem>> = dst
        .keys()
        .filter(|s| !src.contains_key(*s))
        .cloned()
        .map(Some)
        .collect();
    let mut fresh: Vec<Option<Stem>> = src
        .keys()
        .filter(|s| !dst.contains_key(*s))
        .cloned()
        .map(Some)
        .collect();
    if orphans.is_empty() || fresh.is_empty() {
        return Pairing::default();
    }

    let mtime = |p: &Path| -> Option<i128> {
        fs::metadata(p)
            .ok()
            .map(|m| m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128)
    };
    let basename = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();

    let mut result = Pairing::default();
    for mode in ["identity", "basename", "mtime"] {
        if mode == "identity" && prev_ids.is_empty() {
            continue;
        }
        // Keys are compared as strings so one table serves all three passes.
        let okey = |stem: &str| -> Option<String> {
            match mode {
                "identity" => prev_ids
                    .get(stem)
                    .map(|i| format!("{}:{}", i.ino, i.birth_ns)),
                "basename" => Some(basename(stem)),
                _ => mtime(&dst[stem]).map(|t| t.to_string()),
            }
        };
        let fkey = |stem: &str| -> Option<String> {
            match mode {
                "identity" => file_id(&src[stem]).map(|i| format!("{}:{}", i.ino, i.birth_ns)),
                "basename" => Some(basename(stem)),
                _ => mtime(&src[stem]).map(|t| t.to_string()),
            }
        };

        let mut o: HashMap<String, Vec<usize>> = HashMap::new();
        let mut f: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, s) in orphans.iter().enumerate() {
            if let Some(k) = s.as_deref().and_then(okey) {
                o.entry(k).or_default().push(i);
            }
        }
        for (i, s) in fresh.iter().enumerate() {
            if let Some(k) = s.as_deref().and_then(fkey) {
                f.entry(k).or_default().push(i);
            }
        }

        let mut keys: Vec<&String> = o.keys().collect();
        keys.sort();
        for key in keys {
            let ([oi], Some([fi])) = (o[key].as_slice(), f.get(key).map(Vec::as_slice)) else {
                continue;
            };
            let (from, to) = (orphans[*oi].clone().unwrap(), fresh[*fi].clone().unwrap());
            // Never clobber: something already at the new path means this is
            // not the simple relocation it looks like.
            if dst.contains_key(&to) {
                tracing::warn!(from = %from, to = %to, "not moving: the destination already exists");
                continue;
            }
            result.moves.push(Move { from, to, by: mode });
            orphans[*oi] = None;
            fresh[*fi] = None;
        }
    }

    // Passes find matches in the order of keys that include inode numbers, so
    // sort: the result must not depend on which filesystem the files are on.
    result.moves.sort_by(|a, b| a.from.cmp(&b.from));
    result.unpaired_orphans = orphans.into_iter().flatten().collect();
    result.unpaired_new = fresh.into_iter().flatten().collect();
    result
}

/// Identity of every file under `dir` with extension `ext`, keyed by stem: what
/// the next run's [`pair`] recognises a renamed file by.
pub fn snapshot(dir: &Path, ext: &str) -> HashMap<Stem, FileId> {
    scan(dir, ext)
        .into_iter()
        .filter_map(|(stem, p)| file_id(&p).map(|id| (stem, id)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{FileTime, set_file_mtime};

    struct Trees {
        tmp: tempfile::TempDir,
    }

    /// `secs`/`nanos` since the epoch, so mtimes are exact and comparable.
    fn ft(secs: i64, nanos: u32) -> FileTime {
        FileTime::from_unix_time(secs, nanos)
    }

    impl Trees {
        fn new() -> Self {
            Self {
                tmp: tempfile::tempdir().unwrap(),
            }
        }
        fn s(&self) -> PathBuf {
            self.tmp.path().join("vault")
        }
        fn d(&self) -> PathBuf {
            self.tmp.path().join("docx")
        }
        /// A file with a given mtime; a genuine move or rename keeps it.
        fn mk(&self, side: &Path, rel: &str, mtime: FileTime) {
            let p = side.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, rel).unwrap();
            set_file_mtime(&p, mtime).unwrap();
        }
        fn pair(&self, prev: &HashMap<Stem, FileId>) -> Pairing {
            fs::create_dir_all(self.s()).unwrap();
            fs::create_dir_all(self.d()).unwrap();
            pair(&self.s(), ".md", &self.d(), ".docx", prev)
        }
    }

    fn mv(p: &Pairing, from: &str) -> Option<(String, &'static str)> {
        p.moves
            .iter()
            .find(|m| m.from == from)
            .map(|m| (m.to.clone(), m.by))
    }

    /// The scenarios of the old `move-tracking` check, unchanged.
    #[test]
    fn follows_relocations_renames_and_leaves_deletes_and_creates_alone() {
        let t = Trees::new();
        let (s, d) = (t.s(), t.d());

        // Steady state, including the same basename in two folders: neither
        // may be disturbed.
        t.mk(
            &s,
            "Advisors Thank-you letter.md",
            ft(1_687_276_205, 100_000_000),
        );
        t.mk(
            &d,
            "Advisors Thank-you letter.docx",
            ft(1_687_276_205, 100_000_000),
        );
        t.mk(
            &s,
            "Letters/Advisors Thank-you letter.md",
            ft(1_704_165_845, 200_000_000),
        );
        t.mk(
            &d,
            "Letters/Advisors Thank-you letter.docx",
            ft(1_704_165_845, 200_000_000),
        );

        // 1. relocated AND edited in transit -> paired by basename
        t.mk(
            &d,
            "Project Plan - Alpha & Beta.docx",
            ft(1_734_833_944, 500_000_000),
        );
        t.mk(
            &s,
            "Archive/Project Plan - Alpha & Beta.md",
            ft(1_786_099_150, 900_000_000),
        );
        // 2. whole-directory move
        t.mk(&d, "Proj/a.docx", ft(1_741_000_000, 300_000_000));
        t.mk(&s, "Archive/Proj/a.md", ft(1_741_000_000, 300_000_000));
        // 3. pure rename in place -> basename differs, paired by mtime
        t.mk(&d, "Old Name.docx", ft(1_777_957_505, 500_000_000));
        t.mk(&s, "New Name.md", ft(1_777_957_505, 500_000_000));
        // 4. renamed AND relocated at once
        t.mk(
            &d,
            "Notes from Planning Discussion.docx",
            ft(1_758_877_772, 700_000_000),
        );
        t.mk(
            &s,
            "Archive/Planning Notes.md",
            ft(1_758_877_772, 700_000_000),
        );
        // 5/6. a real delete and a real create, which must NOT be paired
        t.mk(&d, "DeletedInVault.docx", ft(1_767_229_261, 0));
        t.mk(&s, "BrandNew.md", ft(1_770_000_122, 0));
        // 7. same basename moved twice over -> disambiguated by mtime
        t.mk(&d, "x/Dup.docx", ft(1_780_000_000, 600_000_000));
        t.mk(&s, "p/Dup.md", ft(1_780_000_000, 600_000_000));
        t.mk(&d, "y/Dup.docx", ft(1_783_000_000, 700_000_000));
        t.mk(&s, "q/Dup.md", ft(1_783_000_000, 700_000_000));

        let p = t.pair(&HashMap::new());

        let want = [
            (
                "Project Plan - Alpha & Beta",
                "Archive/Project Plan - Alpha & Beta",
            ),
            ("Proj/a", "Archive/Proj/a"),
            ("Old Name", "New Name"),
            ("Notes from Planning Discussion", "Archive/Planning Notes"),
            ("x/Dup", "p/Dup"),
            ("y/Dup", "q/Dup"),
        ];
        for (from, to) in want {
            assert_eq!(mv(&p, from).map(|m| m.0).as_deref(), Some(to), "{from}");
        }
        assert_eq!(p.moves.len(), want.len(), "{:?}", p.moves);
        // A delete is not a move, and neither is a create.
        assert_eq!(p.unpaired_orphans, ["DeletedInVault"]);
        assert_eq!(p.unpaired_new, ["BrandNew"]);
        // The steady-state pairs were never candidates.
        assert!(mv(&p, "Advisors Thank-you letter").is_none());
        assert!(mv(&p, "Letters/Advisors Thank-you letter").is_none());
    }

    #[test]
    fn the_basename_pass_beats_mtime_and_names_the_pass_that_paired() {
        let t = Trees::new();
        t.mk(&t.d(), "Mediation.docx", ft(1_000, 0));
        t.mk(&t.s(), "Archive/Mediation.md", ft(2_000, 0));
        t.mk(&t.d(), "Old.docx", ft(3_000, 0));
        t.mk(&t.s(), "New.md", ft(3_000, 0));
        let p = t.pair(&HashMap::new());
        assert_eq!(
            mv(&p, "Mediation"),
            Some(("Archive/Mediation".into(), "basename"))
        );
        assert_eq!(mv(&p, "Old"), Some(("New".into(), "mtime")));
    }

    #[test]
    fn identity_pairs_a_file_renamed_and_edited_beyond_any_other_clue() {
        let t = Trees::new();
        let (s, d) = (t.s(), t.d());
        t.mk(&s, "Statement on Gender Roles.md", ft(1_000, 0));
        t.mk(&d, "Statement on Gender Roles.docx", ft(1_000, 0));
        t.mk(&s, "Gone.md", ft(1_001, 0));
        t.mk(&d, "Gone.docx", ft(1_001, 0));
        // Last run: remember who was who.
        let prev = snapshot(&s, ".md");
        assert_eq!(prev.len(), 2);

        // Renamed, edited (new mtime, so mtime cannot pair it), new basename.
        fs::rename(
            s.join("Statement on Gender Roles.md"),
            s.join("Scriptural Basis of Godly Femininity and Masculinity.md"),
        )
        .unwrap();
        set_file_mtime(
            s.join("Scriptural Basis of Godly Femininity and Masculinity.md"),
            ft(9_999, 0),
        )
        .unwrap();
        // Plus an honest delete and create.
        fs::remove_file(s.join("Gone.md")).unwrap();
        t.mk(&s, "Brand New.md", ft(5_000, 0));

        let p = t.pair(&prev);
        assert_eq!(
            mv(&p, "Statement on Gender Roles"),
            Some((
                "Scriptural Basis of Godly Femininity and Masculinity".into(),
                "identity"
            ))
        );
        assert_eq!(p.unpaired_orphans, ["Gone"]);
        assert_eq!(p.unpaired_new, ["Brand New"]);
    }

    #[test]
    fn a_recycled_inode_is_not_a_rename() {
        // Delete a note, create a different one: filesystems may hand the new
        // file the old inode number, but never the old birth time.
        let t = Trees::new();
        let (s, d) = (t.s(), t.d());
        t.mk(&s, "Old.md", ft(1_000, 0));
        t.mk(&d, "Old.docx", ft(1_000, 0));
        let prev = snapshot(&s, ".md");
        fs::remove_file(s.join("Old.md")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        t.mk(&s, "Unrelated.md", ft(7_777, 0));
        let p = t.pair(&prev);
        assert!(p.moves.is_empty(), "{:?}", p.moves);
    }

    #[test]
    fn nothing_is_clobbered_and_ambiguity_is_left_alone() {
        let t = Trees::new();
        let (s, d) = (t.s(), t.d());
        // Two orphans and two new files all sharing one mtime: ambiguous.
        t.mk(&d, "a.docx", ft(1_000, 0));
        t.mk(&d, "b.docx", ft(1_000, 0));
        t.mk(&s, "c.md", ft(1_000, 0));
        t.mk(&s, "e.md", ft(1_000, 0));
        let p = t.pair(&HashMap::new());
        assert!(p.moves.is_empty());
        assert_eq!(p.unpaired_orphans, ["a", "b"]);
        assert_eq!(p.unpaired_new, ["c", "e"]);
    }

    #[test]
    fn an_empty_or_missing_source_never_moves_anything() {
        let t = Trees::new();
        t.mk(&t.d(), "a.docx", ft(1_000, 0));
        t.mk(&t.d(), "b.docx", ft(2_000, 0));
        // The vault is unmounted: no source files at all.
        let p = t.pair(&HashMap::new());
        assert_eq!(p, Pairing::default());
    }

    #[test]
    fn hidden_files_and_directories_are_not_part_of_the_tree() {
        let t = Trees::new();
        t.mk(&t.s(), "note.md", ft(1, 0));
        t.mk(&t.s(), ".obsidian/workspace.md", ft(1, 0));
        t.mk(&t.s(), ".hidden.md", ft(1, 0));
        t.mk(&t.s(), "sub/.trash/old.md", ft(1, 0));
        t.mk(&t.s(), "sub/kept.md", ft(1, 0));
        t.mk(&t.s(), "notes.md.bak", ft(1, 0));
        let stems: Vec<_> = scan(&t.s(), ".md").into_keys().collect();
        assert_eq!(stems, ["note", "sub/kept"]);
    }

    #[test]
    fn paths_with_spaces_are_handled() {
        // Was the `paths-with-spaces` check.
        let t = Trees::new();
        t.mk(&t.s(), "Hello World.md", ft(1, 0));
        t.mk(&t.s(), "Meeting Notes/Jan Session.md", ft(2, 0));
        let stems: Vec<_> = scan(&t.s(), ".md").into_keys().collect();
        assert_eq!(stems, ["Hello World", "Meeting Notes/Jan Session"]);
    }
}
