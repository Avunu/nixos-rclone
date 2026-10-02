//! The two directions of markdownSync.
//!
//! * **markdown leads** (before a pull): follow moves made in the vault, turn
//!   notes that changed into docx, and drop the docx of notes that were
//!   deleted.
//! * **docx leads** (after a pull): the same the other way round, for what the
//!   remote changed.
//!
//! Which file is newer decides what is converted, and a conversion copies its
//! source's modification time onto its output, so a converted pair compares
//! equal and nothing is converted twice. That also makes the watcher's own
//! writes harmless: a note just written from a docx is not newer than it.
//!
//! Output is written to a temporary file and renamed into place, so neither
//! the watcher nor a concurrent pull ever sees half a document.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use filetime::{FileTime, set_file_mtime};

use super::convert;
use super::moves::{self, Stem};
use crate::identity::FileId;

/// Fewer deletions than this in one pass are never a "storm".
const MIN_STORM: usize = 10;

pub struct Mirror {
    pub md_dir: PathBuf,
    pub docx_dir: PathBuf,
    pub sync_deletions: bool,
    pub track_moves: bool,
    /// Identity snapshot of the vault, for recognising a renamed note across
    /// runs. Losing it costs one pairing pass, not data.
    pub ids_file: Option<PathBuf>,
    /// bisync's `maxDelete`, in percent, for the deletion storm guard.
    pub max_delete: u8,
    /// Styles a note's *first* conversion starts from; after that its own docx
    /// is the reference.
    pub template: Option<PathBuf>,
}

/// What a pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub converted: usize,
    /// Files moved in the *destination* tree, relative paths with extension.
    pub moved: Vec<(String, String)>,
    pub deleted: usize,
    /// Conversions that failed; the rest of the pass carried on.
    pub failed: usize,
}

impl Mirror {
    // ── Markdown leads ───────────────────────────────────────────────────

    /// Before a pull. The returned `moved` are docx renames already made on
    /// disk, which the caller should carry through to the remote *before* the
    /// pull, so that it is a server-side move and not a delete + create.
    pub fn md_leads(&self) -> Outcome {
        let mut out = self.follow_moves();

        for (stem, md) in moves::scan(&self.md_dir, ".md") {
            let docx = self.docx_dir.join(format!("{stem}.docx"));
            if newer_than(&md, &docx) {
                match self.md_to_docx(&md, &docx) {
                    Ok(()) => out.converted += 1,
                    Err(e) => {
                        tracing::warn!(note = %stem, error = format!("{e:#}"), "markdown to docx failed");
                        out.failed += 1;
                    }
                }
            }
        }

        if self.sync_deletions {
            out.deleted += self.drop_orphans(&self.docx_dir, ".docx", &self.md_dir, ".md");
        }
        self.save_ids();
        out
    }

    /// Notes moved or renamed in the vault: move their docx the same way, so
    /// that the relocation is followed and not regenerated. Found by file
    /// identity, basename or mtime (see [`moves`]), which also catches what the
    /// watcher missed.
    pub fn follow_moves(&self) -> Outcome {
        let mut out = Outcome::default();
        if self.track_moves {
            let prev = self.load_ids();
            let pairing = moves::pair(&self.md_dir, ".md", &self.docx_dir, ".docx", &prev);
            self.apply_moves(&pairing, &self.docx_dir, ".docx", &mut out);
        }
        out
    }

    /// One note changed (or appeared): bring its docx up to date. `Ok(true)` if
    /// a conversion was done.
    pub fn convert_note(&self, stem: &str) -> Result<bool> {
        let md = self.md_dir.join(format!("{stem}.md"));
        let docx = self.docx_dir.join(format!("{stem}.docx"));
        if !md.is_file() || !newer_than(&md, &docx) {
            return Ok(false);
        }
        self.md_to_docx(&md, &docx)?;
        Ok(true)
    }

    fn md_to_docx(&self, md: &Path, docx: &Path) -> Result<()> {
        // An existing docx of this note lends its styles to the new one.
        let reference = if docx.is_file() {
            Some(docx)
        } else {
            self.template.as_deref()
        };
        let bytes = convert::md_file_to_docx(md, reference)?;
        write_like(docx, bytes.as_slice(), md)
    }

    // ── Docx leads ───────────────────────────────────────────────────────

    /// After a pull. The returned `moved` are markdown renames made in the
    /// vault.
    pub fn docx_leads(&self) -> Outcome {
        let mut out = Outcome::default();

        if self.track_moves {
            // bisync has just applied the remote's moves to the docx tree, so
            // it is authoritative for paths here: the notes follow.
            let pairing = moves::pair(
                &self.docx_dir,
                ".docx",
                &self.md_dir,
                ".md",
                &HashMap::new(),
            );
            self.apply_moves(&pairing, &self.md_dir, ".md", &mut out);
        }

        for (stem, docx) in moves::scan(&self.docx_dir, ".docx") {
            let md = self.md_dir.join(format!("{stem}.md"));
            if newer_than(&docx, &md) {
                match convert::docx_file_to_md(&docx)
                    .and_then(|text| write_like(&md, text.as_bytes(), &docx))
                {
                    Ok(()) => out.converted += 1,
                    Err(e) => {
                        tracing::warn!(note = %stem, error = format!("{e:#}"), "docx to markdown failed");
                        out.failed += 1;
                    }
                }
            }
        }

        if self.sync_deletions {
            out.deleted += self.drop_orphans(&self.md_dir, ".md", &self.docx_dir, ".docx");
        }
        self.save_ids();
        out
    }

    // ── Shared ───────────────────────────────────────────────────────────

    fn apply_moves(
        &self,
        pairing: &moves::Pairing,
        dst_dir: &Path,
        dst_ext: &str,
        out: &mut Outcome,
    ) {
        for m in &pairing.moves {
            let (from, to) = (
                dst_dir.join(format!("{}{dst_ext}", m.from)),
                dst_dir.join(format!("{}{dst_ext}", m.to)),
            );
            if to.exists() {
                continue;
            }
            let moved = to
                .parent()
                .map(fs::create_dir_all)
                .transpose()
                .and_then(|_| fs::rename(&from, &to));
            match moved {
                Ok(()) => {
                    tracing::info!(
                        "markdown-sync: followed move by {}: {}{dst_ext} -> {}{dst_ext}",
                        m.by,
                        m.from,
                        m.to
                    );
                    out.moved
                        .push((format!("{}{dst_ext}", m.from), format!("{}{dst_ext}", m.to)));
                }
                Err(e) => {
                    tracing::warn!(from = %m.from, to = %m.to, error = %e, "could not follow a move")
                }
            }
        }
        for s in &pairing.unpaired_orphans {
            tracing::debug!("markdown-sync: unpaired orphan '{s}{dst_ext}'");
        }
    }

    /// Delete files of `victim_dir` that have no counterpart in `lead_dir`.
    /// Returns how many were deleted.
    fn drop_orphans(
        &self,
        victim_dir: &Path,
        victim_ext: &str,
        lead_dir: &Path,
        lead_ext: &str,
    ) -> usize {
        let lead = moves::scan(lead_dir, lead_ext);
        let victims = moves::scan(victim_dir, victim_ext);
        // An unmounted or empty leading side must not wipe the other one.
        if lead.is_empty() && !victims.is_empty() {
            tracing::warn!(
                dir = %lead_dir.display(),
                "the leading directory is missing or empty; skipping the deletion pass"
            );
            return 0;
        }
        let orphans: Vec<&PathBuf> = victims
            .iter()
            .filter(|(stem, _)| !lead.contains_key(*stem))
            .map(|(_, p)| p)
            .collect();
        if orphans.len() >= MIN_STORM
            && orphans.len() * 100 > self.max_delete as usize * victims.len()
        {
            tracing::warn!(
                deletions = orphans.len(),
                of = victims.len(),
                "too many deletions at once; leaving them alone"
            );
            return 0;
        }
        orphans
            .into_iter()
            .filter(|p| fs::remove_file(p).is_ok())
            .count()
    }

    /// A batch of vault paths changed (from the watcher): bring the docx tree up
    /// to date for exactly those, without scanning the rest.
    pub fn vault_changed(&self, paths: &std::collections::BTreeSet<String>) -> Outcome {
        let mut out = Outcome::default();
        let mut removed: Vec<Stem> = Vec::new();

        let convert = |stem: &str, out: &mut Outcome| match self.convert_note(stem) {
            Ok(true) => out.converted += 1,
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(note = %stem, error = format!("{e:#}"), "markdown to docx failed");
                out.failed += 1;
            }
        };

        for p in paths {
            if is_hidden(p) {
                continue;
            }
            if let Some(stem) = p.strip_suffix(".md") {
                if self.md_dir.join(p).is_file() {
                    convert(stem, &mut out);
                } else {
                    removed.push(stem.to_string());
                }
            } else if self.md_dir.join(p).is_dir() {
                // A folder appeared or was moved in: its notes may have no
                // events of their own.
                for sub in moves::scan(&self.md_dir.join(p), ".md").into_keys() {
                    convert(&format!("{p}/{sub}"), &mut out);
                }
            } else {
                // Not a note, and gone: perhaps a folder that was removed.
                for sub in moves::scan(&self.docx_dir.join(p), ".docx").into_keys() {
                    removed.push(format!("{p}/{sub}"));
                }
            }
        }

        if self.sync_deletions && !removed.is_empty() {
            let total = moves::scan(&self.docx_dir, ".docx")
                .len()
                .max(removed.len());
            if removed.len() >= MIN_STORM && removed.len() * 100 > self.max_delete as usize * total
            {
                tracing::warn!(
                    deletions = removed.len(),
                    of = total,
                    "too many notes deleted at once; leaving their documents alone"
                );
            } else {
                out.deleted += removed.iter().filter(|s| self.note_removed(s)).count();
            }
        }
        out
    }

    /// A note was deleted: remove its docx, unless that looks like an unmounted
    /// vault. `Ok(true)` if removed.
    pub fn note_removed(&self, stem: &str) -> bool {
        if !self.sync_deletions {
            return false;
        }
        let md = self.md_dir.join(format!("{stem}.md"));
        let docx = self.docx_dir.join(format!("{stem}.docx"));
        if md.exists() || !docx.is_file() {
            return false;
        }
        // The vault being gone is not a request to delete everything.
        if moves::scan(&self.md_dir, ".md").is_empty() {
            return false;
        }
        fs::remove_file(docx).is_ok()
    }

    // ── Identity snapshot ────────────────────────────────────────────────

    fn load_ids(&self) -> HashMap<Stem, FileId> {
        let Some(path) = &self.ids_file else {
            return HashMap::new();
        };
        let Ok(raw) = fs::read(path) else {
            return HashMap::new();
        };
        serde_json::from_slice::<HashMap<Stem, (u64, i128)>>(&raw)
            .map(|m| {
                m.into_iter()
                    .map(|(k, (ino, birth_ns))| (k, FileId { ino, birth_ns }))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Record who is who in the vault, for the next run. Called after both
    /// passes: the first is skipped when a pull fails, and the next run still
    /// needs current identities.
    pub fn save_ids(&self) {
        if !self.track_moves {
            return;
        }
        let Some(path) = &self.ids_file else { return };
        let snap: HashMap<Stem, (u64, i128)> = moves::snapshot(&self.md_dir, ".md")
            .into_iter()
            .map(|(k, v)| (k, (v.ino, v.birth_ns)))
            .collect();
        let result = (|| -> Result<()> {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, serde_json::to_vec(&snap)?)?;
            fs::rename(&tmp, path)?;
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!(
                error = format!("{e:#}"),
                "could not save the vault identity snapshot"
            );
        }
    }
}

/// Whether any component of a vault-relative path is hidden (`.obsidian`,
/// `.trash`, …), which markdownSync never touches.
pub fn is_hidden(rel: &str) -> bool {
    rel.split('/').any(|c| c.starts_with('.'))
}

/// Whether `a` is strictly newer than `b`, or `b` does not exist.
pub fn newer_than(a: &Path, b: &Path) -> bool {
    let Ok(ma) = fs::metadata(a) else {
        return false;
    };
    let Ok(mb) = fs::metadata(b) else { return true };
    (ma.mtime(), ma.mtime_nsec()) > (mb.mtime(), mb.mtime_nsec())
}

/// Write `bytes` to `dst` atomically, giving it `like`'s modification time.
fn write_like(dst: &Path, bytes: &[u8], like: &Path) -> Result<()> {
    let dir = dst.parent().context("no parent directory")?;
    fs::create_dir_all(dir)?;
    let mut name = std::ffi::OsString::from(".");
    name.push(dst.file_name().context("no file name")?);
    name.push(".tmp");
    let tmp = dir.join(name);

    let result = (|| -> Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        let m = fs::metadata(like)?;
        set_file_mtime(&tmp, FileTime::from_last_modification_time(&m))?;
        fs::rename(&tmp, dst)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", dst.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fx {
        tmp: tempfile::TempDir,
    }

    fn ft(secs: i64) -> FileTime {
        FileTime::from_unix_time(secs, 123_456_789)
    }

    impl Fx {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            fs::create_dir(tmp.path().join("vault")).unwrap();
            fs::create_dir(tmp.path().join("docx")).unwrap();
            Self { tmp }
        }
        fn mirror(&self) -> Mirror {
            Mirror {
                md_dir: self.tmp.path().join("vault"),
                docx_dir: self.tmp.path().join("docx"),
                sync_deletions: true,
                track_moves: true,
                ids_file: Some(self.tmp.path().join("state/ids.json")),
                max_delete: 50,
                template: None,
            }
        }
        fn md(&self, rel: &str, body: &str, mtime: i64) {
            let p = self.tmp.path().join("vault").join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, body).unwrap();
            set_file_mtime(&p, ft(mtime)).unwrap();
        }
        fn vault(&self, rel: &str) -> PathBuf {
            self.tmp.path().join("vault").join(rel)
        }
        fn docx(&self, rel: &str) -> PathBuf {
            self.tmp.path().join("docx").join(rel)
        }
        fn mtime(p: &Path) -> (i64, i64) {
            let m = fs::metadata(p).unwrap();
            (m.mtime(), m.mtime_nsec())
        }
    }

    #[test]
    fn notes_become_docx_with_the_notes_modification_time_and_nothing_is_redone() {
        let f = Fx::new();
        f.md("Hello World.md", "# Hello\n\nbody\n", 1_000);
        f.md("Meeting Notes/Jan Session.md", "# Jan\n", 2_000);
        let m = f.mirror();

        let out = m.md_leads();
        assert_eq!((out.converted, out.failed), (2, 0));
        assert!(f.docx("Hello World.docx").is_file());
        assert!(f.docx("Meeting Notes/Jan Session.docx").is_file());
        assert_eq!(
            Fx::mtime(&f.docx("Hello World.docx")),
            Fx::mtime(&f.vault("Hello World.md"))
        );

        // A second pass has nothing to do.
        assert_eq!(m.md_leads(), Outcome::default());
        // No temporary files are left behind.
        assert!(!f.docx(".Hello World.docx.tmp").exists());
    }

    #[test]
    fn an_edited_note_is_converted_again_and_only_that_one() {
        let f = Fx::new();
        f.md("a.md", "# A\n", 1_000);
        f.md("b.md", "# B\n", 1_000);
        let m = f.mirror();
        m.md_leads();
        let b_before = fs::read(f.docx("b.docx")).unwrap();

        f.md("a.md", "# A\n\nedited\n", 5_000);
        let out = m.md_leads();
        assert_eq!(out.converted, 1);
        assert_eq!(fs::read(f.docx("b.docx")).unwrap(), b_before);
        let back = convert::docx_file_to_md(&f.docx("a.docx")).unwrap();
        assert!(back.contains("edited"), "{back:?}");
    }

    #[test]
    fn a_docx_changed_remotely_comes_back_as_markdown() {
        let f = Fx::new();
        f.md("note.md", "# Note\n\nv1\n", 1_000);
        let m = f.mirror();
        m.md_leads();

        // The remote edit: a newer docx with new content.
        let edited = convert::md_to_docx("# Note\n\nv2 from the remote\n", None).unwrap();
        fs::write(f.docx("note.docx"), edited).unwrap();
        set_file_mtime(f.docx("note.docx"), ft(9_000)).unwrap();

        let out = m.docx_leads();
        assert_eq!(out.converted, 1);
        assert_eq!(
            fs::read_to_string(f.vault("note.md")).unwrap(),
            "# Note\n\nv2 from the remote\n"
        );
        assert_eq!(
            Fx::mtime(&f.vault("note.md")),
            Fx::mtime(&f.docx("note.docx"))
        );
        // The vault watcher will report that write; the note must not look newer.
        assert_eq!(m.md_leads().converted, 0);
        assert_eq!(m.docx_leads(), Outcome::default());
    }

    #[test]
    fn a_new_remote_document_appears_in_the_vault_in_a_new_folder() {
        let f = Fx::new();
        f.md("seed.md", "x\n", 1_000);
        let m = f.mirror();
        m.md_leads();
        let fresh = convert::md_to_docx("# Fresh\n", None).unwrap();
        fs::create_dir_all(f.docx("From Drive")).unwrap();
        fs::write(f.docx("From Drive/Fresh Note.docx"), fresh).unwrap();
        m.docx_leads();
        assert!(f.vault("From Drive/Fresh Note.md").is_file());
    }

    #[test]
    fn a_deleted_note_takes_its_docx_with_it() {
        let f = Fx::new();
        f.md("keep.md", "# Keep\n", 1_000);
        f.md("drop.md", "# Drop\n", 1_000);
        let m = f.mirror();
        m.md_leads();
        fs::remove_file(f.vault("drop.md")).unwrap();
        let out = m.md_leads();
        assert_eq!(out.deleted, 1);
        assert!(!f.docx("drop.docx").exists());
        assert!(f.docx("keep.docx").exists());
    }

    #[test]
    fn a_missing_vault_never_wipes_the_docx_tree() {
        let f = Fx::new();
        f.md("a.md", "# A\n", 1_000);
        f.md("b.md", "# B\n", 1_000);
        let m = f.mirror();
        m.md_leads();
        fs::remove_dir_all(f.vault("")).unwrap();
        let out = m.md_leads();
        assert_eq!(out.deleted, 0);
        assert!(f.docx("a.docx").exists() && f.docx("b.docx").exists());

        // And the other way: an empty docx tree must not wipe the vault.
        let f = Fx::new();
        f.md("a.md", "# A\n", 1_000);
        let m = f.mirror();
        assert_eq!(m.docx_leads().deleted, 0);
        assert!(f.vault("a.md").exists());
    }

    #[test]
    fn a_burst_of_deletions_is_left_alone() {
        let f = Fx::new();
        for i in 0..14 {
            f.md(&format!("n{i}.md"), "x\n", 1_000);
        }
        let m = f.mirror();
        m.md_leads();
        for i in 0..12 {
            fs::remove_file(f.vault(&format!("n{i}.md"))).unwrap();
        }
        assert_eq!(m.md_leads().deleted, 0);
        assert_eq!(moves::scan(&f.tmp.path().join("docx"), ".docx").len(), 14);
    }

    #[test]
    fn deletions_can_be_switched_off() {
        let f = Fx::new();
        f.md("a.md", "# A\n", 1_000);
        f.md("b.md", "# B\n", 1_000);
        let mut m = f.mirror();
        m.sync_deletions = false;
        m.md_leads();
        fs::remove_file(f.vault("a.md")).unwrap();
        assert_eq!(m.md_leads().deleted, 0);
        assert!(f.docx("a.docx").exists());
    }

    #[test]
    fn a_vault_rename_moves_the_docx_instead_of_regenerating_it() {
        let f = Fx::new();
        f.md("Draft.md", "# Draft\n", 1_000);
        f.md("Statement.md", "# Statement\n", 1_000);
        f.md("other.md", "# Other\n", 3_000);
        let m = f.mirror();
        m.md_leads();
        let docx_ino = fs::metadata(f.docx("Draft.docx")).unwrap().ino();

        // Renamed and edited (new mtime, new basename): only identity can pair it.
        fs::rename(f.vault("Draft.md"), f.vault("Final.md")).unwrap();
        set_file_mtime(f.vault("Final.md"), ft(8_000)).unwrap();
        fs::create_dir_all(f.vault("Archive")).unwrap();
        fs::rename(f.vault("Statement.md"), f.vault("Archive/Statement.md")).unwrap();

        let out = m.md_leads();
        assert_eq!(
            out.moved,
            [
                ("Draft.docx".to_string(), "Final.docx".to_string()),
                (
                    "Statement.docx".to_string(),
                    "Archive/Statement.docx".to_string()
                )
            ]
        );
        assert!(!f.docx("Draft.docx").exists());
        assert!(f.docx("Final.docx").exists());
        // The docx at the new path is the old file, brought up to date (it was
        // edited too), not a stray duplicate left at the old path.
        assert_eq!(moves::scan(&f.tmp.path().join("docx"), ".docx").len(), 3);
        let _ = docx_ino;
    }

    #[test]
    fn a_remote_move_moves_the_note() {
        let f = Fx::new();
        f.md("Old Name.md", "# Old\n", 1_000);
        f.md("keep.md", "# Keep\n", 2_000);
        let m = f.mirror();
        m.md_leads();
        // bisync applied the remote's rename to the docx tree.
        fs::create_dir_all(f.docx("Archive")).unwrap();
        fs::rename(f.docx("Old Name.docx"), f.docx("Archive/New Name.docx")).unwrap();
        let out = m.docx_leads();
        assert_eq!(
            out.moved,
            [("Old Name.md".to_string(), "Archive/New Name.md".to_string())]
        );
        assert!(f.vault("Archive/New Name.md").is_file());
        assert!(!f.vault("Old Name.md").exists());
        assert_eq!(out.deleted, 0);
    }

    #[test]
    fn a_broken_document_does_not_stop_the_others() {
        let f = Fx::new();
        f.md("good.md", "# Good\n", 1_000);
        let m = f.mirror();
        m.md_leads();
        fs::write(f.docx("broken.docx"), b"not a docx").unwrap();
        set_file_mtime(f.docx("broken.docx"), ft(9_000)).unwrap();
        fs::write(
            f.docx("fine.docx"),
            convert::md_to_docx("# Fine\n", None).unwrap(),
        )
        .unwrap();
        set_file_mtime(f.docx("fine.docx"), ft(9_000)).unwrap();
        let out = m.docx_leads();
        assert_eq!((out.converted, out.failed), (1, 1));
        assert!(f.vault("fine.md").is_file());
        assert!(!f.vault("broken.md").exists());
    }

    #[test]
    fn single_note_events() {
        let f = Fx::new();
        f.md("a.md", "# A\n", 1_000);
        f.md("b.md", "# B\n", 1_000);
        let m = f.mirror();
        assert!(m.convert_note("a").unwrap());
        assert!(!m.convert_note("a").unwrap(), "already current");
        assert!(!m.convert_note("nope").unwrap());
        m.md_leads();
        fs::remove_file(f.vault("a.md")).unwrap();
        assert!(m.note_removed("a"));
        assert!(!f.docx("a.docx").exists());
        assert!(!m.note_removed("a"), "already gone");
        // Deleting the only note left is indistinguishable from an unmounted vault.
        fs::remove_file(f.vault("b.md")).unwrap();
        assert!(!m.note_removed("b"));
        assert!(f.docx("b.docx").exists());
    }

    #[test]
    fn a_template_styles_the_first_conversion() {
        let f = Fx::new();
        let template = f.tmp.path().join("template.docx");
        fs::write(
            &template,
            convert::md_to_docx("# Template\n", None).unwrap(),
        )
        .unwrap();
        f.md("note.md", "# Note\n", 1_000);
        let mut m = f.mirror();
        m.template = Some(template);
        assert_eq!(m.md_leads().converted, 1);
        let back = convert::docx_file_to_md(&f.docx("note.docx")).unwrap();
        assert!(
            back.contains("# Note") && !back.contains("Template"),
            "{back:?}"
        );
    }

    #[test]
    fn watcher_batches_convert_only_what_changed() {
        use std::collections::BTreeSet;
        let f = Fx::new();
        f.md("a.md", "# A\n", 1_000);
        f.md("b.md", "# B\n", 1_000);
        f.md("New Folder/c.md", "# C\n", 1_000);
        f.md("New Folder/deeper/d.md", "# D\n", 1_000);
        let m = f.mirror();
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();

        // One note, plus a folder whose notes made no events of their own.
        let out = m.vault_changed(&set(&[
            "a.md",
            "New Folder",
            ".obsidian/x.md",
            "not-a-note.txt",
        ]));
        assert_eq!(out.converted, 3);
        assert!(f.docx("a.docx").exists());
        assert!(f.docx("New Folder/c.docx").exists());
        assert!(f.docx("New Folder/deeper/d.docx").exists());
        assert!(!f.docx("b.docx").exists(), "b was not mentioned");
    }

    #[test]
    fn watcher_deletions_follow_notes_and_folders() {
        use std::collections::BTreeSet;
        let f = Fx::new();
        f.md("keep.md", "# K\n", 1_000);
        f.md("gone.md", "# G\n", 1_000);
        f.md("Dir/one.md", "# 1\n", 1_000);
        f.md("Dir/two.md", "# 2\n", 1_000);
        let m = f.mirror();
        m.md_leads();
        fs::remove_file(f.vault("gone.md")).unwrap();
        fs::remove_dir_all(f.vault("Dir")).unwrap();
        let set: BTreeSet<String> = ["gone.md", "Dir"].iter().map(|s| s.to_string()).collect();
        let out = m.vault_changed(&set);
        assert_eq!(out.deleted, 3);
        assert!(f.docx("keep.docx").exists());
        assert!(!f.docx("gone.docx").exists());
        assert!(!f.docx("Dir/one.docx").exists() && !f.docx("Dir/two.docx").exists());
    }

    #[test]
    fn watcher_deletion_storms_are_withheld() {
        use std::collections::BTreeSet;
        let f = Fx::new();
        for i in 0..14 {
            f.md(&format!("n{i}.md"), "x\n", 1_000);
        }
        let m = f.mirror();
        m.md_leads();
        let mut gone = BTreeSet::new();
        for i in 0..12 {
            fs::remove_file(f.vault(&format!("n{i}.md"))).unwrap();
            gone.insert(format!("n{i}.md"));
        }
        assert_eq!(m.vault_changed(&gone).deleted, 0);
        assert_eq!(moves::scan(&f.tmp.path().join("docx"), ".docx").len(), 14);
    }

    #[test]
    fn hidden_vault_content_is_ignored() {
        let f = Fx::new();
        f.md("note.md", "# N\n", 1_000);
        f.md(".obsidian/plugins.md", "# no\n", 1_000);
        let out = f.mirror().md_leads();
        assert_eq!(out.converted, 1);
        assert!(!f.docx(".obsidian").exists());
    }
}
