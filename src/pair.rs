//! The per-pair service: one `rclone rcd` child, a pull loop on a timer, a
//! watcher that pushes local changes the moment they happen, and a control
//! socket.
//!
//! Everything that touches the pair is serialised through this one task, so a
//! bisync, a push or a conversion can never overlap another.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use notify::RecursiveMode;
use notify_debouncer_full::{DebounceEventResult, new_debouncer};
use sd_notify::NotifyState;
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::bisync::{self, DEFAULT_MAX_DELETE, FailureKind, Mode};
use crate::config::PairConfig;
use crate::ctl::{self, Call, Request, Response, State, Status};
use crate::filter::Filter;
use crate::identity::Snapshot;
use crate::markdown::mirror::{self, Mirror};
use crate::mounts::install_file;
use crate::push::{self, Pusher, State as PushState};
use crate::rc::Rc;
use crate::rc::ops::RcRemote;
use crate::rc::process::{Rcd, RcdSpec, backend_env};
use crate::watch::{self, Change, Normalized};

/// Where the service finds its private directories.
pub struct Runtime {
    /// `$RUNTIME_DIRECTORY`: holds the rc socket, control socket, config copy.
    pub dir: PathBuf,
    /// `$STATE_DIRECTORY`: survives restarts; holds renames not yet pushed.
    pub state: Option<PathBuf>,
    /// `$CREDENTIALS_DIRECTORY`, when the unit was given the rclone config.
    pub credentials: Option<PathBuf>,
}

impl Runtime {
    /// From the environment systemd sets for a unit with `RuntimeDirectory=`,
    /// `StateDirectory=` and `LoadCredential=`.
    pub fn from_env() -> Result<Self> {
        let first = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|v| PathBuf::from(v.split(':').next().unwrap_or_default()))
        };
        Ok(Self {
            dir: first("RUNTIME_DIRECTORY").context(
                "RUNTIME_DIRECTORY is not set (is this running as a systemd unit with RuntimeDirectory=?)",
            )?,
            state: first("STATE_DIRECTORY"),
            credentials: std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from),
        })
    }
}

const RETRY_MIN: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(300);
/// After the kernel drops events there is no telling what changed; a full pass
/// finds out. Shortly, so a burst that overflowed the queue has finished.
const RESCAN_DELAY: Duration = Duration::from_secs(5);

struct Pair {
    cfg: PairConfig,
    rc: Arc<Rc>,
    status: Status,
    /// `None` when pushing is switched off, or the watcher could not start.
    pusher: Option<Pusher<RcRemote>>,
    /// bisync's two listings, once known.
    listings: Option<(PathBuf, PathBuf)>,
    /// Pushes to try again, and when.
    retry: Vec<Change>,
    retry_at: Option<Instant>,
    retry_backoff: Duration,
    pending_file: Option<PathBuf>,
    /// markdownSync, when enabled.
    mirror: Option<Arc<Mirror>>,
    /// What pushing remembers between batches (see [`push::Pusher::push`]).
    state: PushState,
    known_file: Option<PathBuf>,
    /// A pass owed after pushes, for remotes that rewrite modtimes (see
    /// [`Pair::settle`]), or after the kernel lost events.
    reconcile_at: Option<Instant>,
}

pub async fn run(cfg: PairConfig, rt: Runtime) -> Result<()> {
    let config = if cfg.config_credential {
        let src = rt
            .credentials
            .as_ref()
            .context("configCredential is set but CREDENTIALS_DIRECTORY is not")?
            .join("rclone.conf");
        // rclone persists token refreshes into its config, which a read-only
        // credential would reject; hence the writable copy.
        let dst = rt.dir.join("rclone.conf");
        install_file(&src, &dst).context("staging rclone config")?;
        Some(dst)
    } else {
        None
    };

    let (mut rcd, rc) = Rcd::spawn(&RcdSpec {
        rclone: cfg.rclone.clone(),
        socket: rt.dir.join("rc.sock"),
        config,
        env: backend_env(&cfg),
    })
    .await?;
    let version = rc.version().await.unwrap_or_default();
    tracing::info!(pair = %cfg.name, rclone = %version, "rcd is up");
    let rc = Arc::new(rc);

    std::fs::create_dir_all(&cfg.local_path)
        .with_context(|| format!("creating {}", cfg.local_path.display()))?;

    let ctl_path = rt.dir.join("ctl.sock");
    let _ = std::fs::remove_file(&ctl_path);
    let listener = UnixListener::bind(&ctl_path).context("binding control socket")?;
    let (calls_tx, mut calls) = mpsc::channel::<Call>(8);
    tokio::spawn(ctl::serve(listener, calls_tx));

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let stop = async {
        tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
    };
    tokio::pin!(stop);

    // Watch the tree. Failing to is not fatal: the pair still pulls on its
    // timer, which is all it did before there was a watcher.
    let (events_tx, mut events) = mpsc::unbounded_channel::<Normalized>();
    let _watcher = if cfg.push.enable {
        match start_watcher(&cfg, &rt, events_tx) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::error!(pair = %cfg.name, error = format!("{e:#}"), "cannot watch {}; local changes will wait for the next pull", cfg.local_path.display());
                None
            }
        }
    } else {
        None
    };

    // The pusher also carries vault renames to the remote before a pull, so it
    // exists whether or not the watcher does.
    let pusher = match Filter::new(&cfg.excludes) {
        Ok(filter) => Some(Pusher {
            remote: RcRemote {
                rc: rc.clone(),
                local: cfg.local_path.to_string_lossy().into_owned(),
                remote: cfg.remote.clone(),
            },
            root: cfg.local_path.clone(),
            filter,
            max_delete: cfg.bisync.max_delete.unwrap_or(DEFAULT_MAX_DELETE),
        }),
        Err(e) => {
            tracing::error!(pair = %cfg.name, error = %e, "bad exclude pattern; not pushing");
            None
        }
    };

    let pending_file = rt.state.as_ref().map(|d| d.join("pending-renames.json"));
    let retry = pending_file
        .as_deref()
        .map(push::load_pending)
        .unwrap_or_default();
    if !retry.is_empty() {
        tracing::info!(pair = %cfg.name, renames = retry.len(), "resuming renames not yet pushed");
    }

    let known_file = rt.state.as_ref().map(|d| d.join("pushed-since-pull.json"));
    let known = known_file
        .as_deref()
        .map(push::load_known)
        .unwrap_or_default();

    let ids = if _watcher.is_some() {
        let (root, excludes) = (cfg.local_path.clone(), cfg.excludes.clone());
        tokio::task::spawn_blocking(move || {
            Filter::new(&excludes)
                .map(|f| Snapshot::scan(&root, &f))
                .unwrap_or_default()
        })
        .await
        .unwrap_or_default()
    } else {
        Snapshot::default()
    };

    let mirror = cfg.markdown_sync.as_ref().map(|m| {
        Arc::new(Mirror {
            md_dir: m.path.clone(),
            docx_dir: cfg.local_path.clone(),
            sync_deletions: m.sync_deletions,
            track_moves: m.track_moves,
            ids_file: rt.state.as_ref().map(|d| d.join("markdown-ids.json")),
            max_delete: cfg.bisync.max_delete.unwrap_or(DEFAULT_MAX_DELETE),
            template: m.reference_doc.clone(),
        })
    });
    let (vault_tx, mut vault_events) = mpsc::unbounded_channel::<Normalized>();
    let _vault_watcher = match (&mirror, cfg.push.enable) {
        (Some(m), true) => {
            match start_dir_watcher(
                &cfg.name,
                &m.md_dir,
                cfg.push_debounce(),
                Vec::new(),
                vault_tx,
            ) {
                Ok(w) => Some(w),
                Err(e) => {
                    tracing::error!(pair = %cfg.name, error = format!("{e:#}"), "cannot watch the vault; notes will convert at the next pull");
                    None
                }
            }
        }
        _ => None,
    };

    let mut pair = Pair {
        mirror,
        state: PushState { known, ids },
        known_file,
        listings: bisync::listing_paths(&cfg),
        retry_at: (!retry.is_empty()).then(|| Instant::now() + RETRY_MIN),
        retry,
        retry_backoff: RETRY_MIN,
        pending_file,
        reconcile_at: None,
        pusher,
        cfg,
        rc,
        status: Status::new(),
    };
    pair.status.state = State::Idle;
    pair.status.pending = pair.retry.len() as u64;

    let _ = sd_notify::notify(&[NotifyState::Ready]);
    if let Some(period) = sd_notify::watchdog_enabled() {
        tokio::spawn(watchdog(rt.dir.join("rc.sock"), period));
    }

    let mut next = Instant::now() + pair.cfg.pull_on_boot() + jitter(pair.cfg.pull_jitter());
    let far = Duration::from_secs(86_400 * 365);
    let outcome = loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next) => {
                let r = tokio::select! {
                    r = pair.pass(false) => r,
                    _ = &mut stop => break Ok(()),
                };
                if let Err(e) = r {
                    tracing::warn!(pair = %pair.cfg.name, error = %e, "scheduled pull failed");
                }
                next = Instant::now() + pair.cfg.pull_interval() + jitter(pair.cfg.pull_jitter());
            }
            Some(n) = events.recv() => {
                tokio::select! {
                    _ = pair.on_events(n) => {}
                    _ = &mut stop => break Ok(()),
                }
            }
            Some(n) = vault_events.recv() => {
                tokio::select! {
                    _ = pair.on_vault_events(n) => {}
                    _ = &mut stop => break Ok(()),
                }
            }
            _ = tokio::time::sleep_until(pair.retry_at.unwrap_or_else(|| Instant::now() + far)),
                if pair.retry_at.is_some() => {
                tokio::select! {
                    _ = pair.push_changes(Vec::new()) => {}
                    _ = &mut stop => break Ok(()),
                }
            }
            _ = tokio::time::sleep_until(pair.reconcile_at.unwrap_or_else(|| Instant::now() + far)),
                if pair.reconcile_at.is_some() => {
                pair.reconcile_at = None;
                let r = tokio::select! {
                    r = pair.pass(false) => r,
                    _ = &mut stop => break Ok(()),
                };
                if let Err(e) = r {
                    tracing::warn!(pair = %pair.cfg.name, error = %e, "follow-up pull failed");
                }
            }
            Some((req, reply)) = calls.recv() => {
                let result = match req {
                    Request::Status => Ok(()),
                    Request::Sync | Request::Resync => {
                        tokio::select! {
                            r = pair.pass(req == Request::Resync) => r,
                            _ = &mut stop => break Ok(()),
                        }
                    }
                };
                let _ = reply.send(Response {
                    ok: result.is_ok(),
                    message: result.err(),
                    status: pair.status.clone(),
                });
            }
            status = rcd.exited() => {
                break Err(anyhow::anyhow!("rclone rcd exited unexpectedly: {}", status?));
            }
            _ = &mut stop => break Ok(()),
        }
    };

    let _ = sd_notify::notify(&[NotifyState::Stopping]);
    rcd.shutdown(Duration::from_secs(20)).await;
    let _ = std::fs::remove_file(&ctl_path);
    outcome
}

type Watcher = notify_debouncer_full::Debouncer<
    notify::RecommendedWatcher,
    notify_debouncer_full::RecommendedCache,
>;

fn start_watcher(
    cfg: &PairConfig,
    rt: &Runtime,
    tx: mpsc::UnboundedSender<Normalized>,
) -> Result<Watcher> {
    // The pair's own bookkeeping may live inside the tree it syncs.
    let ignore = vec![
        cfg.workdir.clone(),
        rt.dir.clone(),
        rt.state.clone().unwrap_or_default(),
    ]
    .into_iter()
    .filter(|p| !p.as_os_str().is_empty())
    .collect::<Vec<_>>();
    start_dir_watcher(&cfg.name, &cfg.local_path, cfg.push_debounce(), ignore, tx)
}

/// Watch `root` recursively, sending debounced, normalised changes to `tx`.
fn start_dir_watcher(
    name: &str,
    root: &std::path::Path,
    debounce: Duration,
    ignore: Vec<PathBuf>,
    tx: mpsc::UnboundedSender<Normalized>,
) -> Result<Watcher> {
    let watch_root = root.to_path_buf();
    let pair_name = name.to_string();
    let mut debouncer =
        new_debouncer(
            debounce,
            None,
            move |result: DebounceEventResult| match result {
                Ok(events) => {
                    let n = watch::normalize(&events, &watch_root, &ignore);
                    if !n.changes.is_empty() || n.rescan {
                        let _ = tx.send(n);
                    }
                }
                Err(errors) => {
                    for e in errors {
                        tracing::warn!(pair = %pair_name, error = %e, "watcher error");
                    }
                    // Something went wrong that may have cost us events.
                    let _ = tx.send(Normalized {
                        changes: Vec::new(),
                        rescan: true,
                    });
                }
            },
        )
        .context("creating the file watcher")?;
    debouncer
        .watch(root, RecursiveMode::Recursive)
        .with_context(|| {
            format!(
                "watching {} (is fs.inotify.max_user_watches large enough for this tree?)",
                root.display()
            )
        })?;
    tracing::info!(pair = %name, path = %root.display(), "watching for changes");
    Ok(debouncer)
}

impl Pair {
    // ── Local changes ────────────────────────────────────────────────────

    async fn on_events(&mut self, n: Normalized) {
        tracing::debug!(pair = %self.cfg.name, changes = ?n.changes, "local changes");
        if n.rescan {
            tracing::warn!(pair = %self.cfg.name, "filesystem events were lost; scheduling a full pass");
            self.reconcile_at = Some(Instant::now() + RESCAN_DELAY);
        }
        self.push_changes(n.changes).await;
    }

    /// Push `changes` (and whatever is waiting to be retried).
    async fn push_changes(&mut self, changes: Vec<Change>) {
        self.retry_at = None;
        let mut all = std::mem::take(&mut self.retry);
        all.extend(changes);
        if all.is_empty() {
            return;
        }
        let (Some(pusher), Some((l1, l2))) = (&self.pusher, self.listings.clone()) else {
            // Not watching, or the pair has never synced: its first pass will
            // find these changes anyway.
            return;
        };

        // What bisync's own last pass wrote reports back as events; skip it.
        let all = pusher.drop_echoes(all, &l1);
        if all.is_empty() {
            return;
        }

        match pusher.push(all, &l1, &l2, &mut self.state).await {
            Ok(rep) => {
                self.status.pushed.uploaded += rep.uploaded as u64;
                self.status.pushed.moved += rep.moved as u64;
                self.status.pushed.deleted += rep.deleted as u64;
                if rep.did_anything() {
                    tracing::info!(
                        pair = %self.cfg.name,
                        uploaded = rep.uploaded, moved = rep.moved, deleted = rep.deleted,
                        "pushed local changes"
                    );
                    if self.cfg.settle.enable {
                        // Remotes like Google Drive restamp a file seconds after
                        // an upload; pull that back before the next real change
                        // can collide with it.
                        self.reconcile_at =
                            Some(Instant::now() + Duration::from_secs(self.cfg.settle.delay));
                    }
                }
                if let Some(n) = rep.storm {
                    tracing::warn!(pair = %self.cfg.name, deletions = n, "withheld a burst of deletions; the next pull will judge them");
                }
                self.retry = rep.retry;
            }
            Err(e) => {
                // Most likely the listings are missing: the pair is locked out
                // or being resynced, and the pass that fixes that covers these.
                tracing::debug!(pair = %self.cfg.name, error = format!("{e:#}"), "not pushing");
                self.retry.clear();
            }
        }

        self.save_known();
        self.status.pending = self.retry.len() as u64;
        if let Some(path) = &self.pending_file
            && let Err(e) = push::save_pending(path, &self.retry)
        {
            tracing::warn!(pair = %self.cfg.name, error = format!("{e:#}"), "could not save pending renames");
        }
        if self.retry.is_empty() {
            self.retry_backoff = RETRY_MIN;
        } else {
            tracing::warn!(pair = %self.cfg.name, pending = self.retry.len(), retry_in = ?self.retry_backoff, "some pushes failed; will retry");
            self.retry_at = Some(Instant::now() + self.retry_backoff);
            self.retry_backoff = (self.retry_backoff * 2).min(RETRY_MAX);
        }
    }

    // ── Vault changes ────────────────────────────────────────────────────

    /// Notes changed in the vault: convert them now, follow renames onto the
    /// docx and through to the remote, and propagate deletions.
    async fn on_vault_events(&mut self, n: Normalized) {
        let Some(m) = self.mirror.clone() else { return };
        tracing::debug!(pair = %self.cfg.name, changes = ?n.changes, "vault changes");

        let mut renames: Vec<Change> = Vec::new();
        let mut dirty: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for change in n.changes {
            match change {
                Change::Dirty(p) => {
                    dirty.insert(p);
                }
                Change::Rename { from, to } => {
                    let (from_note, to_note) = (from.strip_suffix(".md"), to.strip_suffix(".md"));
                    let (docx_from, docx_to) = match (from_note, to_note) {
                        (Some(f), Some(t)) => (format!("{f}.docx"), format!("{t}.docx")),
                        // A folder (or a note given another extension).
                        _ if m.md_dir.join(&to).is_dir() => (from.clone(), to.clone()),
                        _ => {
                            dirty.insert(from);
                            dirty.insert(to);
                            continue;
                        }
                    };
                    if mirror::is_hidden(&from) || mirror::is_hidden(&to) {
                        dirty.insert(to);
                        continue;
                    }
                    if m.track_moves && self.follow_rename(&m, &docx_from, &docx_to) {
                        renames.push(Change::Rename {
                            from: docx_from,
                            to: docx_to,
                        });
                    }
                    // It may have been edited as well.
                    dirty.insert(to);
                }
            }
        }

        // A move the watcher saw only half of (`mkdir new && mv old new/` lands
        // in a directory it is not watching yet) arrives as "something
        // vanished" and "something appeared", possibly in separate batches.
        // Pair them up by identity before converting anything, so a note that
        // merely moved is not regenerated at its new path.
        let looks_like_a_move = |p: &String| {
            let abs = m.md_dir.join(p);
            !abs.exists()
                || abs.is_dir()
                || p.strip_suffix(".md")
                    .is_some_and(|stem| !m.docx_dir.join(format!("{stem}.docx")).exists())
        };
        if m.track_moves && dirty.iter().any(looks_like_a_move) {
            let m2 = m.clone();
            if let Ok(out) = tokio::task::spawn_blocking(move || m2.follow_moves()).await {
                renames.extend(
                    out.moved
                        .into_iter()
                        .map(|(from, to)| Change::Rename { from, to }),
                );
            }
        }

        // Carry the renames to the remote first: the docx files the conversions
        // below write must find them already moved.
        if !renames.is_empty() {
            self.push_changes(renames).await;
        }
        let m2 = m.clone();
        match tokio::task::spawn_blocking(move || m2.vault_changed(&dirty)).await {
            Ok(out) if out.converted + out.deleted + out.failed > 0 => {
                tracing::info!(pair = %self.cfg.name, converted = out.converted, deleted = out.deleted, failed = out.failed, "vault changes applied");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(pair = %self.cfg.name, error = %e, "vault conversion task failed")
            }
        }
        let m3 = m.clone();
        let _ = tokio::task::spawn_blocking(move || m3.save_ids()).await;
    }

    /// Rename the docx (or docx folder) that goes with a renamed note or
    /// folder, if there is one and nothing is in the way.
    fn follow_rename(&self, m: &Mirror, from: &str, to: &str) -> bool {
        let (src, dst) = (m.docx_dir.join(from), m.docx_dir.join(to));
        if !src.exists() || dst.exists() {
            return false;
        }
        let moved = dst
            .parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| std::fs::rename(&src, &dst));
        match moved {
            Ok(()) => {
                tracing::info!("markdown-sync: followed move: {from} -> {to}");
                true
            }
            Err(e) => {
                tracing::warn!(from, to, error = %e, "could not follow a rename");
                false
            }
        }
    }

    // ── Pulling ──────────────────────────────────────────────────────────

    /// One pull. `force_resync` is `ctl resync`.
    async fn pass(&mut self, force_resync: bool) -> Result<(), String> {
        if self.status.state == State::NeedsResync && !force_resync {
            return Err(self
                .status
                .last_error
                .clone()
                .unwrap_or_else(|| "needs a resync (run `rclone-remotes ctl resync`)".into()));
        }
        self.status.state = State::Syncing;
        if let Err(e) = std::fs::create_dir_all(&self.cfg.local_path) {
            return self.failed(State::Failed, format!("creating local path: {e}"));
        }

        let result = if force_resync {
            self.resync("requested").await
        } else {
            self.normal().await
        };
        match result {
            Ok(()) => {
                self.status.state = State::Idle;
                self.status.last_error = None;
                self.status.last_success = now_secs();
                Ok(())
            }
            Err((state, msg)) => self.failed(state, msg),
        }
    }

    fn failed(&mut self, state: State, msg: String) -> Result<(), String> {
        self.status.state = state;
        self.status.last_error = Some(msg.clone());
        Err(msg)
    }

    /// A pass succeeded: note where it says the listings are, and everything
    /// pushed so far is in them now.
    fn remember(&mut self, report: &bisync::Report) {
        if let (Some(a), Some(b)) = (&report.listing1, &report.listing2) {
            self.listings = Some((a.clone(), b.clone()));
        }
        self.state.known.clear();
        self.save_known();
    }

    /// Re-read the identity of every local file: the pass has added, replaced
    /// and removed some. Cheap next to the pass itself, which lists both sides.
    async fn refresh_ids(&mut self) {
        if self.pusher.is_none() {
            return;
        }
        let (root, excludes) = (self.cfg.local_path.clone(), self.cfg.excludes.clone());
        let scanned = tokio::task::spawn_blocking(move || {
            Filter::new(&excludes).map(|f| Snapshot::scan(&root, &f))
        })
        .await;
        if let Ok(Ok(ids)) = scanned {
            self.state.ids = ids;
        }
    }

    fn save_known(&self) {
        if let Some(path) = &self.known_file
            && let Err(e) = push::save_known(path, &self.state.known)
        {
            tracing::warn!(pair = %self.cfg.name, error = format!("{e:#}"), "could not save pushed paths");
        }
    }

    async fn normal(&mut self) -> Result<(), (State, String)> {
        self.mirror_before_pull().await?;
        let result = match bisync::run(&self.rc, &self.cfg, Mode::Normal).await {
            Ok(report) => {
                self.remember(&report);
                self.refresh_ids().await;
                self.status.passes += 1;
                if self.cfg.settle.enable {
                    self.settle().await;
                }
                Ok(())
            }
            Err(f) => match f.kind {
                FailureKind::NoPriorListings => self.resync("no prior listings").await,
                FailureKind::CriticalLockout => Err((
                    State::NeedsResync,
                    format!(
                        "{f}; run `rclone-remotes ctl --name {} resync`",
                        self.cfg.name
                    ),
                )),
                FailureKind::Other => Err((State::Failed, f.to_string())),
            },
        };
        if result.is_ok() {
            self.mirror_after_pull().await;
        }
        result
    }

    /// Notes to docx, before the pull uploads them. Renames made in the vault
    /// are carried to the remote first, as server-side moves.
    async fn mirror_before_pull(&mut self) -> Result<(), (State, String)> {
        let Some(m) = self.mirror.clone() else {
            return Ok(());
        };
        let m2 = m.clone();
        let out = tokio::task::spawn_blocking(move || m2.md_leads())
            .await
            .map_err(|e| {
                (
                    State::Failed,
                    format!("markdown conversion task failed: {e}"),
                )
            })?;
        if out.converted + out.deleted + out.failed + out.moved.len() > 0 {
            tracing::info!(pair = %self.cfg.name, converted = out.converted, moved = out.moved.len(), deleted = out.deleted, failed = out.failed, "notes brought up to date before the pull");
        }

        if !out.moved.is_empty() {
            let renames = out
                .moved
                .into_iter()
                .map(|(from, to)| Change::Rename { from, to })
                .collect();
            self.push_changes(renames).await;
            // A rename that did not reach the remote would be replayed by the
            // pull as delete + create, which is what following it exists to
            // prevent. It is queued for retry; do the pull after it has gone.
            if self
                .retry
                .iter()
                .any(|c| matches!(c, Change::Rename { .. }))
            {
                return Err((
                    State::Failed,
                    "could not carry a rename to the remote; the pull waits for it".into(),
                ));
            }
        }
        Ok(())
    }

    /// Docx to notes, for what the pull brought in.
    async fn mirror_after_pull(&mut self) {
        let Some(m) = self.mirror.clone() else { return };
        match tokio::task::spawn_blocking(move || m.docx_leads()).await {
            Ok(out) if out.converted + out.deleted + out.failed + out.moved.len() > 0 => {
                tracing::info!(pair = %self.cfg.name, converted = out.converted, moved = out.moved.len(), deleted = out.deleted, failed = out.failed, "notes brought up to date after the pull");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(pair = %self.cfg.name, error = %e, "markdown conversion task failed")
            }
        }
    }

    async fn resync(&mut self, why: &str) -> Result<(), (State, String)> {
        tracing::info!(pair = %self.cfg.name, why, "initial resync");
        match bisync::run(&self.rc, &self.cfg, Mode::Resync).await {
            Ok(report) => {
                self.remember(&report);
                self.refresh_ids().await;
                self.status.passes += 1;
                self.status.resyncs += 1;
                Ok(())
            }
            Err(f) => Err((State::Failed, format!("resync failed: {f}"))),
        }
    }

    /// A second pass, for remotes that rewrite modtimes after an upload (Google
    /// Drive converting an imported file): it pulls the restamped copy back
    /// within this run instead of colliding with it at the next one.
    /// Non-fatal: it is an optimisation, and the first pass already succeeded.
    async fn settle(&mut self) {
        tokio::time::sleep(Duration::from_secs(self.cfg.settle.delay)).await;
        match bisync::run(&self.rc, &self.cfg, Mode::Normal).await {
            Ok(report) => {
                self.remember(&report);
                self.refresh_ids().await;
                self.status.passes += 1;
            }
            Err(f) => tracing::warn!(pair = %self.cfg.name, error = %f, "settle pass failed"),
        }
    }
}

fn now_secs() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Uniform-ish value in `0..=max`, without pulling in a RNG crate.
fn jitter(max: Duration) -> Duration {
    if max.is_zero() {
        return max;
    }
    let r = RandomState::new().build_hasher().finish();
    Duration::from_nanos((r as u128 % (max.as_nanos() + 1)) as u64)
}

/// Ping systemd's watchdog while rcd still answers. A pass can run for a long
/// time, so the check is on rcd's own responsiveness, not on the pull loop.
async fn watchdog(sock: PathBuf, period: Duration) {
    let Ok(rc) = Rc::unix(&sock) else { return };
    let mut tick = tokio::time::interval(period / 2);
    loop {
        tick.tick().await;
        if tokio::time::timeout(period / 4, rc.version())
            .await
            .is_ok_and(|r| r.is_ok())
        {
            let _ = sd_notify::notify(&[NotifyState::Watchdog]);
        }
    }
}
