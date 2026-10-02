//! Pushing local changes to the remote the moment they happen.
//!
//! The watcher reports *that* paths changed; this module decides what to do
//! from the state of the disk when it looks, once the burst of events has gone
//! quiet:
//!
//! * a file that exists is uploaded (in place, so a Google Drive document keeps
//!   its file ID);
//! * a file that is gone, and that the remote is known to hold, is deleted
//!   there;
//! * a rename the watcher saw is carried out on the remote as a server-side
//!   move, and recorded in bisync's listings so the next pull reads it as the
//!   same file at a new path instead of replaying delete + create.
//!
//! "Known to the remote" means *listed in bisync's last listing of it, or
//! uploaded by this module since*: a file the remote never had is nothing to
//! move or delete there, and until the pair has synced once (no listings)
//! nothing is pushed at all.
//!
//! Uploads are not written into the listings. The next pull finds such a file
//! new on both sides and, the two being identical, has nothing to do; for
//! remotes that rewrite modtimes after an upload the settle pass reconciles.
//! Until that pull, the set of paths pushed since the last one stands in for
//! the listing (see [`Pusher::push`]).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::filter::Filter;
use crate::identity::{self, Snapshot};
use crate::listing::Listing;
use crate::watch::Change;

/// Fewer deletions than this in one batch are never "a storm".
const MIN_STORM: usize = 10;

/// Why a remote operation failed, as far as retrying is concerned.
#[derive(Debug, Clone)]
pub struct OpError {
    /// The thing operated on is not there.
    pub not_found: bool,
    /// Worth retrying later: the network or rcd, not the request.
    pub transient: bool,
    pub message: String,
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The remote side of a pair, as seen by the push engine. Paths are relative to
/// the sync root on both sides.
pub trait Remote: Send + Sync {
    fn copy_file(&self, rel: &str) -> impl Future<Output = Result<(), OpError>> + Send;
    fn move_file(&self, from: &str, to: &str) -> impl Future<Output = Result<(), OpError>> + Send;
    fn delete_file(&self, rel: &str) -> impl Future<Output = Result<(), OpError>> + Send;
    /// Remove an empty directory; failing because it is not empty is expected.
    fn remove_dir(&self, rel: &str) -> impl Future<Output = Result<(), OpError>> + Send;
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub uploaded: usize,
    pub moved: usize,
    pub deleted: usize,
    /// Work to try again later (transient failures).
    pub retry: Vec<Change>,
    /// Deletions withheld because there were suspiciously many; carries the
    /// count. They are left to the next pull, which applies bisync's own
    /// `maxDelete` check.
    pub storm: Option<usize>,
    /// Remote directories that a move or delete may have emptied.
    prune: BTreeSet<String>,
}

impl Report {
    /// Note that `path`'s parent directories may now be empty on the remote.
    fn prune_parents_of(&mut self, path: &str) {
        let mut dir = path;
        while let Some(i) = dir.rfind('/') {
            dir = &dir[..i];
            self.prune.insert(dir.to_string());
        }
    }
}

impl Report {
    pub fn did_anything(&self) -> bool {
        self.uploaded + self.moved + self.deleted > 0
    }
}

pub struct Pusher<R> {
    pub remote: R,
    pub root: PathBuf,
    pub filter: Filter,
    /// bisync's `maxDelete`, in percent.
    pub max_delete: u8,
}

/// What the push engine remembers between batches.
#[derive(Debug, Default, Clone)]
pub struct State {
    /// Paths uploaded since the last successful pull: the remote has them, the
    /// listings do not yet.
    pub known: BTreeSet<String>,
    /// Identity of local files, to recognise a rename the watcher missed.
    pub ids: Snapshot,
}

/// What is at a path on disk, not following symlinks (rclone does not either).
enum Local {
    File,
    Dir,
    Absent,
    /// A symlink, socket, device: not synced.
    Other,
}

fn local_kind(path: &Path) -> Local {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Local::File,
        Ok(m) if m.is_dir() => Local::Dir,
        Ok(_) => Local::Other,
        Err(_) => Local::Absent,
    }
}

impl<R: Remote> Pusher<R> {
    fn abs(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    /// Drop changes that are only the echo of bisync's own writes: events that
    /// arrived while a pass was running, for files that now match what that
    /// pass recorded. Anything else happened to the file after, or during, and
    /// is kept.
    pub fn drop_echoes(&self, changes: Vec<Change>, listing1: &Path) -> Vec<Change> {
        let Ok(l1) = Listing::load(listing1) else {
            return changes;
        };
        changes
            .into_iter()
            .filter(|c| match c {
                Change::Dirty(p) => !self.matches_listing(&l1, p),
                Change::Rename { .. } => true,
            })
            .collect()
    }

    fn matches_listing(&self, l1: &Listing, rel: &str) -> bool {
        use std::os::unix::fs::MetadataExt;
        let Some(want) = l1.stat(rel) else {
            return false;
        };
        let Ok(m) = fs::symlink_metadata(self.abs(rel)) else {
            return false;
        };
        let mtime = m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128;
        m.is_file() && m.len() as i64 == want.size && mtime == want.mtime_ns
    }

    /// Carry `changes` out against the remote, patching the listings as
    /// needed. Fails only if the listings cannot be read or written; a failed
    /// remote operation is reported, not returned.
    ///
    /// `st.known` is the set of paths this module has uploaded since the last
    /// successful pull, and `st.ids` the identity of local files; the caller
    /// clears the first when a pull succeeds and refreshes the second.
    pub async fn push(
        &self,
        changes: Vec<Change>,
        listing1: &Path,
        listing2: &Path,
        st: &mut State,
    ) -> Result<Report> {
        let mut l1 = Listing::load(listing1)?;
        let mut l2 = Listing::load(listing2)?;
        let before = (l1.to_text(), l2.to_text());
        let mut rep = Report::default();
        let mut dirty: BTreeSet<String> = BTreeSet::new();

        for change in changes {
            match change {
                Change::Dirty(p) => {
                    dirty.insert(p);
                }
                Change::Rename { from, to } => {
                    self.rename(&from, &to, &mut l1, &mut l2, st, &mut dirty, &mut rep)
                        .await;
                }
            }
        }

        // Turn dirty paths into work, looking at the disk as it is now.
        let mut uploads: BTreeSet<String> = BTreeSet::new();
        let mut deletes: BTreeSet<String> = BTreeSet::new();
        for p in &dirty {
            self.classify(p, &l2, &st.known, &mut uploads, &mut deletes);
        }

        // A synced file that vanished and a new one that appeared with the same
        // identity are one file renamed, whether or not the watcher saw it so.
        self.pair_by_identity(
            &mut deletes,
            &uploads,
            &mut l1,
            &mut l2,
            st,
            &mut dirty,
            &mut rep,
        )
        .await;

        for p in &uploads {
            match self.remote.copy_file(p).await {
                Ok(()) => {
                    rep.uploaded += 1;
                    st.known.insert(p.clone());
                    st.ids.set(p, identity::file_id(&self.abs(p)));
                }
                Err(e) if e.not_found => {
                    tracing::debug!(path = %p, "vanished before it could be uploaded");
                }
                Err(e) if e.transient => rep.retry.push(Change::Dirty(p.clone())),
                Err(e) => {
                    tracing::warn!(path = %p, error = %e, "upload failed; the next pull will retry")
                }
            }
        }

        self.delete(&deletes, &mut l1, &mut l2, st, &mut rep).await;

        // Don't leave emptied directories behind: the next pull would read an
        // empty remote directory as something to recreate locally. Deepest
        // first, and "not empty" is simply the expected answer.
        for d in std::mem::take(&mut rep.prune).iter().rev() {
            if self.remote.remove_dir(d).await.is_ok() {
                // Gone for good: the listings should not remember it either.
                l1.remove(d);
                l2.remove(d);
            }
        }

        if (l1.to_text(), l2.to_text()) != before {
            l1.save(listing1)?;
            l2.save(listing2)?;
        }
        Ok(rep)
    }

    /// Decide, from the disk, what a dirty path means.
    fn classify(
        &self,
        rel: &str,
        l2: &Listing,
        known: &BTreeSet<String>,
        uploads: &mut BTreeSet<String>,
        deletes: &mut BTreeSet<String>,
    ) {
        let on_remote = |p: &str| l2.contains(p) || known.contains(p);
        match local_kind(&self.abs(rel)) {
            Local::File => {
                if self.filter.includes_file(rel) {
                    uploads.insert(rel.to_string());
                }
            }
            // A directory appeared (or was renamed in). Files created in it
            // before the watcher reached it generate no events of their own.
            // Only those the remote does not have: the rest are unchanged.
            Local::Dir => {
                if self.filter.includes_dir(rel) {
                    self.walk_new_files(&self.abs(rel), rel, &on_remote, uploads);
                }
            }
            Local::Absent => {
                if self.filter.includes_file(rel) && on_remote(rel) {
                    deletes.insert(rel.to_string());
                }
                // A directory that went away: everything the remote holds
                // beneath it goes too.
                let prefix = format!("{rel}/");
                for p in l2
                    .paths()
                    .chain(known.iter().map(String::as_str))
                    .filter(|p| p.starts_with(&prefix))
                {
                    if matches!(local_kind(&self.abs(p)), Local::Absent)
                        && self.filter.includes_file(p)
                    {
                        deletes.insert(p.to_string());
                    }
                }
            }
            Local::Other => {}
        }
    }

    fn walk_new_files(
        &self,
        dir: &Path,
        rel: &str,
        on_remote: &dyn Fn(&str) -> bool,
        out: &mut BTreeSet<String>,
    ) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let child = format!("{rel}/{name}");
            if !crate::watch::pushable(&child) {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                if self.filter.includes_dir(&child) {
                    self.walk_new_files(&entry.path(), &child, on_remote, out);
                }
            } else if ft.is_file() && self.filter.includes_file(&child) && !on_remote(&child) {
                out.insert(child);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn rename(
        &self,
        from: &str,
        to: &str,
        l1: &mut Listing,
        l2: &mut Listing,
        st: &mut State,
        dirty: &mut BTreeSet<String>,
        rep: &mut Report,
    ) {
        let is_dir = matches!(local_kind(&self.abs(to)), Local::Dir);
        let kept = |p: &str| {
            if is_dir {
                self.filter.includes_dir(p)
            } else {
                self.filter.includes_file(p)
            }
        };
        // Into or out of an excluded place: to the remote it is a plain
        // appearance or disappearance.
        if !kept(from) || !kept(to) {
            dirty.insert(from.to_string());
            dirty.insert(to.to_string());
            return;
        }

        if is_dir {
            let prefix = format!("{from}/");
            let members: BTreeSet<String> = l2
                .paths()
                .chain(st.known.iter().map(String::as_str))
                .filter(|p| p.starts_with(&prefix))
                .map(str::to_string)
                .collect();
            for old in members {
                let new = format!("{to}/{}", &old[prefix.len()..]);
                self.move_one(&old, &new, l1, l2, st, dirty, rep).await;
            }
            // The directory entries follow, and whatever is new under the
            // destination is found; the now-empty source is pruned.
            l1.rename_dirs(from, to);
            l2.rename_dirs(from, to);
            dirty.insert(to.to_string());
            rep.prune.insert(from.to_string());
        } else {
            self.move_one(from, to, l1, l2, st, dirty, rep).await;
        }
    }

    /// Move one synced file on the remote, if that is safe; otherwise leave it
    /// as an upload of the new path and a delete of the old.
    #[allow(clippy::too_many_arguments)]
    async fn move_one(
        &self,
        from: &str,
        to: &str,
        l1: &mut Listing,
        l2: &mut Listing,
        st: &mut State,
        dirty: &mut BTreeSet<String>,
        rep: &mut Report,
    ) -> bool {
        let on_remote =
            |p: &str, l2: &Listing, known: &BTreeSet<String>| l2.contains(p) || known.contains(p);
        let fallback = |dirty: &mut BTreeSet<String>| {
            dirty.insert(from.to_string());
            dirty.insert(to.to_string());
            false
        };
        // Never synced: nothing there to move. Something already at the
        // destination: not the simple relocation it looks like, and a move
        // would overwrite it.
        if !on_remote(from, l2, &st.known) || on_remote(to, l2, &st.known) {
            return fallback(dirty);
        }
        if !matches!(local_kind(&self.abs(to)), Local::File) {
            return fallback(dirty);
        }

        match self.remote.move_file(from, to).await {
            Ok(()) => {
                rep.moved += 1;
                rep.prune_parents_of(from);
                l1.rename(from, to);
                l2.rename(from, to);
                if st.known.remove(from) {
                    st.known.insert(to.to_string());
                }
                st.ids.rename(from, to);
                // It may have been edited as well as renamed; an upload of an
                // identical file is a no-op, and of an edited one lands in place.
                dirty.insert(to.to_string());
                true
            }
            Err(e) if e.not_found => {
                // Already gone from the remote: forget it, and upload fresh.
                l1.remove(from);
                l2.remove(from);
                st.known.remove(from);
                dirty.insert(to.to_string());
                false
            }
            Err(e) if e.transient => {
                rep.retry.push(Change::Rename {
                    from: from.to_string(),
                    to: to.to_string(),
                });
                false
            }
            Err(e) => {
                tracing::warn!(from, to, error = %e, "remote rename failed; falling back to upload + delete");
                fallback(dirty)
            }
        }
    }

    /// Pair each pending deletion with a pending upload that is the same file
    /// (same inode and birth time) and carry it out as a move instead.
    #[allow(clippy::too_many_arguments)]
    async fn pair_by_identity(
        &self,
        deletes: &mut BTreeSet<String>,
        uploads: &BTreeSet<String>,
        l1: &mut Listing,
        l2: &mut Listing,
        st: &mut State,
        dirty: &mut BTreeSet<String>,
        rep: &mut Report,
    ) {
        if deletes.is_empty() || uploads.is_empty() {
            return;
        }
        // Only unambiguous matches: one vanished file, one new file.
        let mut by_id: std::collections::HashMap<identity::FileId, Vec<&String>> =
            std::collections::HashMap::new();
        for u in uploads {
            if let Some(id) = identity::file_id(&self.abs(u)) {
                by_id.entry(id).or_default().push(u);
            }
        }
        let mut gone: std::collections::HashMap<identity::FileId, Vec<String>> =
            std::collections::HashMap::new();
        for d in deletes.iter() {
            if let Some(id) = st.ids.get(d) {
                gone.entry(id).or_default().push(d.clone());
            }
        }
        for (id, olds) in gone {
            let (Some(news), [old]) = (by_id.get(&id), olds.as_slice()) else {
                continue;
            };
            let [new] = news.as_slice() else { continue };
            if self.move_one(old, new, l1, l2, st, dirty, rep).await {
                tracing::info!(from = %old, to = %new, "followed a rename the watcher did not report");
                deletes.remove(old);
            }
        }
    }

    async fn delete(
        &self,
        deletes: &BTreeSet<String>,
        l1: &mut Listing,
        l2: &mut Listing,
        st: &mut State,
        rep: &mut Report,
    ) {
        if deletes.is_empty() {
            return;
        }
        let total = l1.len().max(l2.len()).max(1);
        if deletes.len() >= MIN_STORM && deletes.len() * 100 > self.max_delete as usize * total {
            tracing::warn!(
                deletions = deletes.len(),
                known = total,
                "too many deletions at once; leaving them to the next pull"
            );
            rep.storm = Some(deletes.len());
            return;
        }

        for p in deletes {
            match self.remote.delete_file(p).await {
                Ok(()) => rep.deleted += 1,
                Err(e) if e.not_found => {}
                Err(e) if e.transient => {
                    rep.retry.push(Change::Dirty(p.clone()));
                    continue;
                }
                Err(e) => {
                    tracing::warn!(path = %p, error = %e, "remote delete failed; the next pull will retry");
                    continue;
                }
            }
            l1.remove(p);
            l2.remove(p);
            st.known.remove(p);
            st.ids.remove(p);
            rep.prune_parents_of(p);
        }
    }
}

// ── Pending renames across restarts ──────────────────────────────────────

/// Renames that have not reached the remote yet. If they were lost, the next
/// pull would replay each as delete + create, which is exactly what following
/// a rename exists to avoid.
pub fn save_pending(path: &Path, retry: &[Change]) -> Result<()> {
    let renames: Vec<(&str, &str)> = retry
        .iter()
        .filter_map(|c| match c {
            Change::Rename { from, to } => Some((from.as_str(), to.as_str())),
            Change::Dirty(_) => None,
        })
        .collect();
    if renames.is_empty() {
        let _ = fs::remove_file(path);
        return Ok(());
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(&renames)?)?;
    fs::rename(&tmp, path).context("saving pending renames")
}

/// Paths uploaded since the last successful pull, kept across restarts: if a
/// restart forgot them, deleting such a file afterwards would leave its remote
/// copy behind for the next pull to bring back.
pub fn save_known(path: &Path, known: &BTreeSet<String>) -> Result<()> {
    if known.is_empty() {
        let _ = fs::remove_file(path);
        return Ok(());
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, serde_json::to_vec(known)?)?;
    fs::rename(&tmp, path).context("saving pushed paths")
}

pub fn load_known(path: &Path) -> BTreeSet<String> {
    fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

pub fn load_pending(path: &Path) -> Vec<Change> {
    let Ok(raw) = fs::read(path) else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<(String, String)>>(&raw)
        .unwrap_or_default()
        .into_iter()
        .map(|(from, to)| Change::Rename { from, to })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Records what was asked of the remote, and can be told to fail.
    #[derive(Default)]
    struct Mock {
        calls: Mutex<Vec<String>>,
        fail: Mutex<BTreeMap<String, OpError>>,
    }

    impl Mock {
        fn fail_with(&self, call: &str, e: OpError) {
            self.fail.lock().unwrap().insert(call.to_string(), e);
        }
        fn record(&self, call: String) -> Result<(), OpError> {
            let r = match self.fail.lock().unwrap().get(&call) {
                Some(e) => Err(e.clone()),
                None => Ok(()),
            };
            self.calls.lock().unwrap().push(call);
            r
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Remote for &Mock {
        async fn copy_file(&self, rel: &str) -> Result<(), OpError> {
            self.record(format!("copy {rel}"))
        }
        async fn move_file(&self, from: &str, to: &str) -> Result<(), OpError> {
            self.record(format!("move {from} -> {to}"))
        }
        async fn delete_file(&self, rel: &str) -> Result<(), OpError> {
            self.record(format!("delete {rel}"))
        }
        async fn remove_dir(&self, rel: &str) -> Result<(), OpError> {
            self.record(format!("rmdir {rel}"))
        }
    }

    fn transient() -> OpError {
        OpError {
            not_found: false,
            transient: true,
            message: "connection refused".into(),
        }
    }
    fn missing() -> OpError {
        OpError {
            not_found: true,
            transient: false,
            message: "object not found".into(),
        }
    }
    fn refused() -> OpError {
        OpError {
            not_found: false,
            transient: false,
            message: "permission denied".into(),
        }
    }

    struct Fixture {
        tmp: tempfile::TempDir,
        mock: Mock,
        state: Mutex<State>,
    }

    const T: &str = "2026-01-01T00:00:00.000000000+0000";

    impl Fixture {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            fs::create_dir(tmp.path().join("root")).unwrap();
            Self {
                tmp,
                mock: Mock::default(),
                state: Mutex::default(),
            }
        }
        fn root(&self) -> PathBuf {
            self.tmp.path().join("root")
        }
        fn l1(&self) -> PathBuf {
            self.tmp.path().join("p.path1.lst")
        }
        fn l2(&self) -> PathBuf {
            self.tmp.path().join("p.path2.lst")
        }
        fn file(&self, rel: &str) {
            let p = self.root().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "x").unwrap();
        }
        /// Both listings know exactly `synced`.
        fn synced(&self, synced: &[&str]) {
            let text: String = std::iter::once("# bisync listing v1 from x\n".to_string())
                .chain(synced.iter().map(|p| {
                    format!(
                        "-           1 - - {T} {}\n",
                        crate::listing::quote(p).unwrap()
                    )
                }))
                .collect();
            fs::write(self.l1(), &text).unwrap();
            fs::write(self.l2(), &text).unwrap();
        }
        fn pusher(&self) -> Pusher<&Mock> {
            Pusher {
                remote: &self.mock,
                root: self.root(),
                filter: Filter::new(&["#recycle/**"]).unwrap(),
                max_delete: 50,
            }
        }
        async fn push(&self, changes: Vec<Change>) -> Report {
            let mut st = self.state.lock().unwrap().clone();
            let r = self
                .pusher()
                .push(changes, &self.l1(), &self.l2(), &mut st)
                .await
                .unwrap();
            *self.state.lock().unwrap() = st;
            r
        }
        fn known(&self) -> Vec<String> {
            self.state.lock().unwrap().known.iter().cloned().collect()
        }
        fn clear_known(&self) {
            self.state.lock().unwrap().known.clear();
        }
        /// What a pull leaves behind: the identity of every local file.
        fn scan_identities(&self) {
            self.state.lock().unwrap().ids =
                Snapshot::scan(&self.root(), &Filter::new::<&str>(&[]).unwrap());
        }
        fn l2_paths(&self) -> Vec<String> {
            Listing::load(&self.l2())
                .unwrap()
                .paths()
                .map(str::to_string)
                .collect()
        }
        fn l1_paths(&self) -> Vec<String> {
            Listing::load(&self.l1())
                .unwrap()
                .paths()
                .map(str::to_string)
                .collect()
        }
    }

    fn dirty(p: &str) -> Change {
        Change::Dirty(p.into())
    }
    fn rename(a: &str, b: &str) -> Change {
        Change::Rename {
            from: a.into(),
            to: b.into(),
        }
    }

    #[tokio::test]
    async fn modified_and_new_files_are_uploaded_once() {
        let f = Fixture::new();
        f.synced(&["old.txt"]);
        f.file("old.txt");
        f.file("Sub Dir/new.txt");
        let r = f
            .push(vec![
                dirty("old.txt"),
                dirty("Sub Dir/new.txt"),
                dirty("old.txt"),
            ])
            .await;
        assert_eq!(r.uploaded, 2);
        assert_eq!(f.mock.calls(), ["copy Sub Dir/new.txt", "copy old.txt"]);
    }

    #[tokio::test]
    async fn a_file_that_no_longer_exists_is_not_uploaded() {
        // Created and deleted inside one debounce window: never there.
        let f = Fixture::new();
        f.synced(&["keep.txt"]);
        f.file("keep.txt");
        let r = f.push(vec![dirty("tmp-1234")]).await;
        assert_eq!(r, Report::default());
        assert!(f.mock.calls().is_empty());
    }

    #[tokio::test]
    async fn a_synced_file_that_is_gone_is_deleted_remotely_and_forgotten() {
        let f = Fixture::new();
        f.synced(&["a.txt", "b.txt"]);
        f.file("b.txt");
        let r = f.push(vec![dirty("a.txt")]).await;
        assert_eq!(r.deleted, 1);
        assert_eq!(f.mock.calls(), ["delete a.txt"]);
        assert_eq!(f.l1_paths(), ["b.txt"]);
        assert_eq!(f.l2_paths(), ["b.txt"]);
    }

    #[tokio::test]
    async fn a_file_pushed_since_the_last_pull_can_be_deleted_and_renamed() {
        // The listings do not know it yet; the remote does.
        let f = Fixture::new();
        f.synced(&["old.txt"]);
        f.file("old.txt");
        f.file("fresh.txt");
        f.push(vec![dirty("fresh.txt")]).await;
        assert_eq!(f.known(), ["fresh.txt"]);

        // Renamed: moved on the remote, not uploaded again as a new file.
        fs::rename(f.root().join("fresh.txt"), f.root().join("renamed.txt")).unwrap();
        let r = f.push(vec![rename("fresh.txt", "renamed.txt")]).await;
        assert_eq!(r.moved, 1);
        assert_eq!(f.known(), ["renamed.txt"]);

        // Deleted: removed on the remote.
        fs::remove_file(f.root().join("renamed.txt")).unwrap();
        let r = f.push(vec![dirty("renamed.txt")]).await;
        assert_eq!(r.deleted, 1);
        assert!(f.known().is_empty());
        assert!(f.mock.calls().contains(&"delete renamed.txt".to_string()));
    }

    #[tokio::test]
    async fn once_a_pull_has_run_the_known_set_is_not_needed() {
        let f = Fixture::new();
        f.synced(&["a.txt"]);
        f.file("a.txt");
        f.push(vec![dirty("a.txt")]).await;
        f.clear_known(); // what a successful pull does
        fs::remove_file(f.root().join("a.txt")).unwrap();
        let r = f.push(vec![dirty("a.txt")]).await;
        assert_eq!(r.deleted, 1, "the listing knows it");
    }

    #[tokio::test]
    async fn a_file_the_remote_never_had_is_not_deleted() {
        let f = Fixture::new();
        f.synced(&["b.txt"]);
        f.file("b.txt");
        let r = f.push(vec![dirty("never-synced.txt")]).await;
        assert_eq!(r, Report::default());
        assert!(f.mock.calls().is_empty());
    }

    #[tokio::test]
    async fn delete_then_recreate_is_an_overwrite_not_delete_plus_create() {
        // On Google Drive the difference is the file's ID, sharing and history.
        let f = Fixture::new();
        f.synced(&["doc.docx"]);
        f.file("doc.docx");
        let r = f.push(vec![dirty("doc.docx"), dirty("doc.docx")]).await;
        assert_eq!((r.uploaded, r.deleted), (1, 0));
        assert_eq!(f.mock.calls(), ["copy doc.docx"]);
    }

    #[tokio::test]
    async fn a_rename_moves_the_remote_file_and_patches_both_listings() {
        let f = Fixture::new();
        f.synced(&["Old Name.docx", "Keep.docx"]);
        f.file("New Name.docx");
        f.file("Keep.docx");
        let r = f.push(vec![rename("Old Name.docx", "New Name.docx")]).await;
        assert_eq!(r.moved, 1);
        // Moved, then uploaded in case it was edited too (a no-op if not).
        assert_eq!(
            f.mock.calls(),
            ["move Old Name.docx -> New Name.docx", "copy New Name.docx"]
        );
        for paths in [f.l1_paths(), f.l2_paths()] {
            assert_eq!(paths, ["New Name.docx", "Keep.docx"]);
        }
    }

    #[tokio::test]
    async fn a_rename_the_watcher_missed_is_recognised_by_file_identity() {
        // `mkdir new && mv old new/`: the destination is not watched yet, so
        // only "old vanished" and "new appeared" are ever reported.
        let f = Fixture::new();
        f.synced(&["Proj/a.docx", "Other.docx"]);
        f.file("Proj/a.docx");
        f.file("Other.docx");
        f.scan_identities();

        fs::create_dir_all(f.root().join("Archive")).unwrap();
        fs::rename(
            f.root().join("Proj/a.docx"),
            f.root().join("Archive/a.docx"),
        )
        .unwrap();
        let r = f.push(vec![dirty("Proj/a.docx"), dirty("Archive")]).await;
        assert_eq!((r.moved, r.deleted), (1, 0));
        assert!(
            f.mock
                .calls()
                .contains(&"move Proj/a.docx -> Archive/a.docx".to_string())
        );
        assert!(f.l2_paths().contains(&"Archive/a.docx".to_string()));
    }

    #[tokio::test]
    async fn identity_is_not_guessed_when_it_is_ambiguous_or_absent() {
        let f = Fixture::new();
        f.synced(&["old.txt"]);
        f.file("old.txt");
        // No snapshot taken: nothing to pair by.
        fs::remove_file(f.root().join("old.txt")).unwrap();
        f.file("new.txt");
        let r = f.push(vec![dirty("old.txt"), dirty("new.txt")]).await;
        assert_eq!((r.moved, r.deleted, r.uploaded), (0, 1, 1));

        // A different file at the new path is not a rename of the old one.
        let f = Fixture::new();
        f.synced(&["old.txt"]);
        f.file("old.txt");
        f.scan_identities();
        fs::remove_file(f.root().join("old.txt")).unwrap();
        f.file("unrelated.txt"); // a fresh inode and birth time
        let r = f.push(vec![dirty("old.txt"), dirty("unrelated.txt")]).await;
        assert_eq!((r.moved, r.deleted, r.uploaded), (0, 1, 1));
    }

    #[tokio::test]
    async fn a_rename_of_something_never_synced_is_just_a_new_file() {
        let f = Fixture::new();
        f.synced(&["other.txt"]);
        f.file("other.txt");
        f.file("b.txt");
        let r = f.push(vec![rename("a.txt", "b.txt")]).await;
        assert_eq!((r.moved, r.uploaded), (0, 1));
        assert_eq!(f.mock.calls(), ["copy b.txt"]);
    }

    #[tokio::test]
    async fn a_rename_onto_an_existing_remote_file_is_not_a_move() {
        // `mv a b` where b existed: a move would overwrite it on the remote.
        let f = Fixture::new();
        f.synced(&["a.txt", "b.txt"]);
        f.file("b.txt");
        let r = f.push(vec![rename("a.txt", "b.txt")]).await;
        assert_eq!(r.moved, 0);
        assert_eq!(f.mock.calls(), ["copy b.txt", "delete a.txt"]);
    }

    #[tokio::test]
    async fn editor_style_atomic_save_uploads_the_target() {
        // write .tmp, rename over the real file
        let f = Fixture::new();
        f.synced(&["note.md"]);
        f.file("note.md");
        let r = f
            .push(vec![
                dirty(".note.md.swp"),
                rename(".note.md.swp", "note.md"),
            ])
            .await;
        assert_eq!((r.moved, r.uploaded, r.deleted), (0, 1, 0));
        assert_eq!(f.mock.calls(), ["copy note.md"]);
    }

    #[tokio::test]
    async fn a_directory_rename_moves_each_synced_file_and_uploads_the_rest() {
        let f = Fixture::new();
        f.synced(&["Proj/a.docx", "Proj/deep/b.docx", "Other/c.docx"]);
        f.file("Archive/Proj/a.docx");
        f.file("Archive/Proj/deep/b.docx");
        f.file("Archive/Proj/created-meanwhile.docx");
        f.file("Other/c.docx");
        let r = f.push(vec![rename("Proj", "Archive/Proj")]).await;
        assert_eq!(r.moved, 2);
        let calls = f.mock.calls();
        assert!(calls.contains(&"move Proj/a.docx -> Archive/Proj/a.docx".to_string()));
        assert!(calls.contains(&"move Proj/deep/b.docx -> Archive/Proj/deep/b.docx".to_string()));
        assert!(calls.contains(&"rmdir Proj".to_string()));
        assert!(calls.contains(&"copy Archive/Proj/created-meanwhile.docx".to_string()));
        assert!(!calls.iter().any(|c| c.contains("Other")), "{calls:?}");
        assert_eq!(
            f.l2_paths(),
            [
                "Archive/Proj/a.docx",
                "Archive/Proj/deep/b.docx",
                "Other/c.docx"
            ]
        );
    }

    #[tokio::test]
    async fn files_in_a_new_directory_are_found_without_events_of_their_own() {
        // The watcher can only start watching a new directory after it exists.
        let f = Fixture::new();
        f.synced(&["x.txt"]);
        f.file("x.txt");
        f.file("new/one.txt");
        f.file("new/sub/two.txt");
        let r = f.push(vec![dirty("new")]).await;
        assert_eq!(r.uploaded, 2);
        assert_eq!(f.mock.calls(), ["copy new/one.txt", "copy new/sub/two.txt"]);
    }

    #[tokio::test]
    async fn a_removed_directory_removes_what_the_remote_holds_beneath_it() {
        let f = Fixture::new();
        f.synced(&["d/a.txt", "d/sub/b.txt", "keep.txt"]);
        f.file("keep.txt");
        let r = f.push(vec![dirty("d")]).await;
        assert_eq!(r.deleted, 2);
        let calls = f.mock.calls();
        assert!(calls.contains(&"delete d/a.txt".to_string()));
        assert!(calls.contains(&"delete d/sub/b.txt".to_string()));
        // Emptied directories go too, deepest first.
        let rmdirs: Vec<_> = calls.iter().filter(|c| c.starts_with("rmdir")).collect();
        assert_eq!(rmdirs, ["rmdir d/sub", "rmdir d"]);
        assert_eq!(f.l2_paths(), ["keep.txt"]);
    }

    #[tokio::test]
    async fn excluded_paths_are_never_pushed() {
        let f = Fixture::new();
        f.synced(&["a.txt"]);
        f.file("a.txt");
        f.file("#recycle/trash.txt");
        let r = f
            .push(vec![
                dirty("#recycle/trash.txt"),
                dirty("#recycle"),
                dirty("a.txt"),
            ])
            .await;
        assert_eq!(r.uploaded, 1);
        assert_eq!(f.mock.calls(), ["copy a.txt"]);
    }

    #[tokio::test]
    async fn a_deletion_storm_is_withheld() {
        // 12 of 14 known files vanishing at once (an unmounted or emptied
        // directory): let the pull's own maxDelete check judge it.
        let f = Fixture::new();
        let names: Vec<String> = (0..14).map(|i| format!("f{i}.txt")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        f.synced(&refs);
        f.file("f12.txt");
        f.file("f13.txt");
        let changes = names.iter().map(|n| dirty(n)).collect();
        let r = f.push(changes).await;
        assert_eq!(r.deleted, 0);
        assert_eq!(r.storm, Some(12));
        assert!(!f.mock.calls().iter().any(|c| c.starts_with("delete")));
        assert_eq!(f.l2_paths().len(), 14, "listings untouched");
    }

    #[tokio::test]
    async fn a_handful_of_deletions_is_not_a_storm_even_in_a_small_tree() {
        let f = Fixture::new();
        f.synced(&["a", "b", "c", "d"]);
        f.file("d");
        let r = f.push(vec![dirty("a"), dirty("b"), dirty("c")]).await;
        assert_eq!(r.deleted, 3);
        assert_eq!(r.storm, None);
    }

    #[tokio::test]
    async fn transient_failures_are_queued_for_retry_and_permanent_ones_are_not() {
        let f = Fixture::new();
        f.synced(&["gone.txt", "renamed-from.txt"]);
        f.file("up.txt");
        f.file("denied.txt");
        f.file("renamed-to.txt");
        f.mock.fail_with("copy up.txt", transient());
        f.mock.fail_with("copy denied.txt", refused());
        f.mock.fail_with("delete gone.txt", transient());
        f.mock
            .fail_with("move renamed-from.txt -> renamed-to.txt", transient());
        let r = f
            .push(vec![
                dirty("up.txt"),
                dirty("denied.txt"),
                dirty("gone.txt"),
                rename("renamed-from.txt", "renamed-to.txt"),
            ])
            .await;
        assert_eq!(r.retry.len(), 3, "{:?}", r.retry);
        assert!(r.retry.contains(&dirty("up.txt")));
        assert!(r.retry.contains(&dirty("gone.txt")));
        assert!(
            r.retry
                .contains(&rename("renamed-from.txt", "renamed-to.txt"))
        );
        assert!(!r.did_anything());
        // Nothing was forgotten by the listings: the remote still has them.
        assert!(f.l2_paths().contains(&"gone.txt".to_string()));
        assert!(f.l2_paths().contains(&"renamed-from.txt".to_string()));
    }

    #[tokio::test]
    async fn a_rename_whose_source_is_already_gone_remotely_becomes_an_upload() {
        let f = Fixture::new();
        f.synced(&["a.txt"]);
        f.file("b.txt");
        f.mock.fail_with("move a.txt -> b.txt", missing());
        let r = f.push(vec![rename("a.txt", "b.txt")]).await;
        assert_eq!(r.uploaded, 1);
        assert_eq!(f.mock.calls(), ["move a.txt -> b.txt", "copy b.txt"]);
        assert!(f.l2_paths().is_empty());
    }

    #[tokio::test]
    async fn echoes_of_bisyncs_own_writes_are_dropped_but_later_edits_are_not() {
        use std::os::unix::fs::MetadataExt;
        let f = Fixture::new();
        f.file("downloaded.txt");
        f.file("edited.txt");
        let meta = |p: &str| fs::metadata(f.root().join(p)).unwrap();
        let line = |p: &str, size: u64, m: &fs::Metadata| {
            // Build the listing time from the file's real mtime.
            let ns = m.mtime_nsec();
            let secs = m.mtime();
            let t = format!("{}", secs * 1_000_000_000 + ns); // only for uniqueness
            let _ = t;
            let dt = chrono_like(secs, ns);
            format!("-           {size} - - {dt} \"{p}\"\n")
        };
        let l1 = format!(
            "# bisync listing v1 from x\n{}{}",
            line("downloaded.txt", 1, &meta("downloaded.txt")),
            // Listing says 99 bytes: the file was edited after the pass.
            line("edited.txt", 99, &meta("edited.txt")),
        );
        fs::write(f.l1(), l1).unwrap();
        fs::write(f.l2(), "# bisync listing v1 from x\n").unwrap();

        let kept = f.pusher().drop_echoes(
            vec![
                dirty("downloaded.txt"),
                dirty("edited.txt"),
                dirty("unknown.txt"),
                rename("a", "b"),
            ],
            &f.l1(),
        );
        assert_eq!(
            kept,
            [dirty("edited.txt"), dirty("unknown.txt"), rename("a", "b")]
        );
    }

    /// `secs`/`ns` since the epoch as bisync's listing time format, in UTC.
    fn chrono_like(secs: i64, ns: i64) -> String {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ns:09}+0000",
            rem / 3600,
            rem % 3600 / 60,
            rem % 60
        )
    }

    #[test]
    fn pending_renames_survive_a_restart_but_plain_changes_do_not() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("pending.json");
        assert!(load_pending(&p).is_empty());
        save_pending(&p, &[rename("a b.docx", "c\"d.docx"), dirty("x")]).unwrap();
        assert_eq!(load_pending(&p), [rename("a b.docx", "c\"d.docx")]);
        save_pending(&p, &[dirty("x")]).unwrap();
        assert!(!p.exists(), "nothing pending: no file");
    }

    #[test]
    fn the_known_set_survives_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("known.json");
        assert!(load_known(&p).is_empty());
        let set: BTreeSet<String> = ["a b.txt".to_string(), "c\"d.txt".to_string()].into();
        save_known(&p, &set).unwrap();
        assert_eq!(load_known(&p), set);
        save_known(&p, &BTreeSet::new()).unwrap();
        assert!(!p.exists());
    }
}
