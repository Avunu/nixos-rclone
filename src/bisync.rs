//! Running `sync/bisync` through the rc API.

use std::path::PathBuf;

use serde_json::{Map, Value, json};

use crate::config::PairConfig;
use crate::rc::{JobOutcome, Rc};

/// rclone's own command-line default for `--max-delete`, in percent.
pub const DEFAULT_MAX_DELETE: u8 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Rebuild the listings from scratch, keeping the newer copy of any file
    /// that differs (what `--resync --resync-mode newer` did in the init unit).
    Resync,
}

/// The JSON body of a `sync/bisync` call for this pair.
pub fn params(cfg: &PairConfig, mode: Mode) -> Value {
    let b = &cfg.bisync;
    let mut p = Map::new();
    p.insert("path1".into(), json!(cfg.local_path));
    p.insert("path2".into(), json!(cfg.remote));
    p.insert("workdir".into(), json!(cfg.workdir));
    p.insert("createEmptySrcDirs".into(), json!(b.create_empty_src_dirs));
    p.insert("resilient".into(), json!(b.resilient));
    p.insert("recover".into(), json!(b.recover));
    p.insert("maxLock".into(), json!(b.max_lock));
    p.insert("compare".into(), json!(b.compare));
    p.insert("conflictResolve".into(), json!(cfg.conflict.resolve));
    p.insert("conflictLoser".into(), json!(cfg.conflict.loser));
    // Always sent: unlike the command line, the rc API does not default this
    // to 50 - an omitted maxDelete means 0%, which aborts on any deletion.
    p.insert(
        "maxDelete".into(),
        json!(b.max_delete.unwrap_or(DEFAULT_MAX_DELETE)),
    );
    if mode == Mode::Resync {
        p.insert("resync".into(), json!(true));
        p.insert("resyncMode".into(), json!("newer"));
    }

    // Per-call overrides of rclone's global options and filter. JSON objects
    // keyed by Go field name, which every rcd version accepts.
    if cfg.google_drive.is_some() {
        // Drive reports modtimes it rewrites after upload, and no cheap hashes
        // for Docs; see the module's settle pass for the rest.
        p.insert("slowHashSyncOnly".into(), json!(true));
        p.insert("_config".into(), json!({ "FixCase": true }));
    }
    if !cfg.excludes.is_empty() {
        p.insert("_filter".into(), json!({ "ExcludeRule": cfg.excludes }));
    }

    for (k, v) in &b.extra_params {
        p.insert(k.clone(), v.clone());
    }
    Value::Object(p)
}

/// What a bisync call left behind.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub output: String,
    pub listing1: Option<PathBuf>,
    pub listing2: Option<PathBuf>,
}

impl Report {
    fn from_output(v: &Value) -> Self {
        let s = |k: &str| v.get(k).and_then(Value::as_str);
        Self {
            output: s("output").unwrap_or_default().to_string(),
            listing1: s("listing1").map(PathBuf::from),
            listing2: s("listing2").map(PathBuf::from),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureKind {
    /// No listings to compare against: either this pair has never synced, or
    /// the last run failed critically and rclone set them aside.
    NoPriorListings,
    /// rclone renamed the listings to `.lst-err` after a critical error, which
    /// locks out further runs until a resync. Never retried automatically.
    CriticalLockout,
    Other,
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
    pub report: Report,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // rclone's own text spans several lines of colourised tips; the first
        // line is the cause.
        write!(f, "{}", self.message.lines().next().unwrap_or(""))
    }
}

const NO_PRIOR: &str = "cannot find prior Path1 or Path2 listings";

pub fn classify(outcome: &JobOutcome, report: &Report) -> Option<Failure> {
    if outcome.success {
        return None;
    }
    // On failure the job's own `error` is just "bisync aborted"; the cause is
    // in the captured log.
    let log = strip_ansi(&report.output);
    let kind = if outcome.error.contains(NO_PRIOR) || log.contains(NO_PRIOR) {
        if report
            .listing1
            .iter()
            .chain(&report.listing2)
            .any(|l| err_sibling(l).exists())
        {
            FailureKind::CriticalLockout
        } else {
            FailureKind::NoPriorListings
        }
    } else {
        FailureKind::Other
    };
    Some(Failure {
        kind,
        message: cause(&outcome.error, &log),
        report: report.clone(),
    })
}

/// The most informative one-liner from a failed run's log: rclone's own
/// "critical error" line if it printed one, else its last ERROR line.
fn cause(error: &str, log: &str) -> String {
    let errors = || log.lines().filter(|l| l.contains(" ERROR : "));
    let line = errors()
        .find(|l| l.contains("critical error"))
        .or_else(|| errors().next_back());
    match line {
        // "2026/10/02 13:13:35 ERROR : Bisync critical error: …" → the part after it.
        Some(l) => l
            .split_once(" ERROR : ")
            .map_or(l, |(_, m)| m)
            .trim()
            .to_string(),
        None => error.to_string(),
    }
}

/// Remove ANSI colour escapes (`ESC [ … m`), which rclone writes into the
/// captured log.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `x.path1.lst` → `x.path1.lst-err`
fn err_sibling(listing: &std::path::Path) -> PathBuf {
    let mut s = listing.as_os_str().to_os_string();
    s.push("-err");
    PathBuf::from(s)
}

/// rclone's `bilib.CanonicalPath`: trim slashes, then `_` for whitespace and
/// `\ / : ? *`.
fn canonical(path: &str) -> String {
    path.trim_matches(['\\', '/'])
        .chars()
        .map(|c| {
            if c.is_whitespace() || "\\/:?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// rclone's `bilib.FsPath`, for the two kinds of path a pair uses: a local
/// directory, or `remote:path`.
fn fs_path(spec: &str) -> String {
    let mut p = match spec.find(':') {
        // `remote:path`, as opposed to a path that merely contains a colon.
        Some(i) if !spec[..i].contains('/') => {
            let (name, root) = spec.split_at(i);
            format!("{name}:{}", root[1..].trim_start_matches('/'))
        }
        _ => spec.to_string(),
    };
    if !p.ends_with('/') {
        p.push('/');
    }
    p
}

/// Where bisync keeps the two listings of this pair: `<workdir>/<session>.path{1,2}.lst`.
///
/// Normally the path comes from the config. For a remote rclone names
/// differently (an `alias` resolves to its target) it is found by looking for
/// the one listing in the workdir that starts with the local side's name.
/// `None` when the pair has never synced; every successful pass reports the
/// real paths anyway.
pub fn listing_paths(cfg: &PairConfig) -> Option<(PathBuf, PathBuf)> {
    let local = canonical(&fs_path(&cfg.local_path.to_string_lossy()));
    let session = format!("{local}..{}", canonical(&fs_path(&cfg.remote)));
    let pair = |base: &str| {
        (
            cfg.workdir.join(format!("{base}.path1.lst")),
            cfg.workdir.join(format!("{base}.path2.lst")),
        )
    };
    let (l1, l2) = pair(&session);
    if l1.is_file() && l2.is_file() {
        return Some((l1, l2));
    }
    let prefix = format!("{local}..");
    let mut found = std::fs::read_dir(&cfg.workdir)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with(&prefix) && n.ends_with(".path1.lst"))
        .map(|n| n.trim_end_matches(".path1.lst").to_string());
    let base = found.next()?;
    if found.next().is_some() {
        return None; // ambiguous
    }
    let (l1, l2) = pair(&base);
    (l1.is_file() && l2.is_file()).then_some((l1, l2))
}

/// Run one bisync pass. `Err` carries the failure rclone reported; a problem
/// talking to rcd at all is an `anyhow` error inside it.
pub async fn run(rc: &Rc, cfg: &PairConfig, mode: Mode) -> Result<Report, Failure> {
    let transport = |e: anyhow::Error| Failure {
        kind: FailureKind::Other,
        message: format!("{e:#}"),
        report: Report::default(),
    };
    let outcome = rc
        .run_job("sync/bisync", &params(cfg, mode))
        .await
        .map_err(transport)?;
    let report = Report::from_output(&outcome.output);
    match classify(&outcome, &report) {
        None => Ok(report),
        Some(f) => Err(f),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::File;

    fn pair(extra: &str) -> PairConfig {
        let json = crate::config::tests::PAIR.replace("\"extraParams\": {}", extra);
        File::parse(&json).unwrap().into_pair().unwrap()
    }

    #[test]
    fn maps_options_to_rc_params() {
        let p = params(&pair("\"extraParams\": {}"), Mode::Normal);
        assert_eq!(p["path1"], "/home/alice/.ssh");
        assert_eq!(p["path2"], "webdav:ssh");
        assert_eq!(p["workdir"], "/home/alice/.cache/rclone/bisync");
        assert_eq!(p["compare"], "size,modtime,checksum");
        assert_eq!(p["conflictResolve"], "newer");
        assert_eq!(p["conflictLoser"], "delete");
        assert_eq!(p["maxLock"], "5m");
        assert_eq!(p["resilient"], true);
        assert_eq!(p["recover"], true);
        assert_eq!(p["createEmptySrcDirs"], true);
        assert_eq!(p["_filter"]["ExcludeRule"][0], "#recycle/**");
        assert!(p.get("resync").is_none());
        assert_eq!(
            p["maxDelete"], 50,
            "rc does not default this like the CLI does"
        );
        assert!(p.get("_config").is_none(), "FixCase is Drive-only");
    }

    #[test]
    fn resync_mode_keeps_the_newer_file() {
        let p = params(&pair("\"extraParams\": {}"), Mode::Resync);
        assert_eq!(p["resync"], true);
        assert_eq!(p["resyncMode"], "newer");
    }

    #[test]
    fn google_drive_adds_its_flags() {
        let json = crate::config::tests::PAIR.replace(
            "\"googleDrive\": null",
            "\"googleDrive\": {\"exportFormats\": \"docx\", \"importFormats\": \"docx\", \"rootFolderId\": null}",
        );
        let cfg = File::parse(&json).unwrap().into_pair().unwrap();
        let p = params(&cfg, Mode::Normal);
        assert_eq!(p["slowHashSyncOnly"], true);
        assert_eq!(p["_config"]["FixCase"], true);
    }

    #[test]
    fn extra_params_win() {
        let p = params(
            &pair("\"extraParams\": {\"conflictResolve\": \"larger\", \"checkAccess\": true}"),
            Mode::Normal,
        );
        assert_eq!(p["conflictResolve"], "larger");
        assert_eq!(p["checkAccess"], true);
    }

    #[test]
    fn listing_names_follow_rclone() {
        // Names rclone really produced in the integration tests.
        assert_eq!(
            canonical(&fs_path("/tmp/.tmpqU5Cc1/local dir")),
            "tmp_.tmpqU5Cc1_local_dir"
        );
        assert_eq!(canonical(&fs_path("webdav:ssh")), "webdav_ssh");
        assert_eq!(canonical(&fs_path("gdrive:")), "gdrive_");
        assert_eq!(canonical(&fs_path("gdrive:/Vault/")), "gdrive_Vault");
        // A colon inside a local path is not a remote.
        assert_eq!(canonical(&fs_path("/home/a:b/c")), "home_a_b_c");
    }

    #[test]
    fn finds_listings_in_the_workdir() {
        let dir = tempfile::tempdir().unwrap();
        let json = crate::config::tests::PAIR
            .replace(
                "/home/alice/.cache/rclone/bisync",
                &dir.path().to_string_lossy(),
            )
            .replace("/home/alice/.ssh", "/home/alice/ssh dir");
        let cfg = File::parse(&json).unwrap().into_pair().unwrap();
        assert_eq!(listing_paths(&cfg), None, "never synced");

        let base = dir.path().join("home_alice_ssh_dir..webdav_ssh");
        std::fs::write(format!("{}.path1.lst", base.display()), "").unwrap();
        std::fs::write(format!("{}.path2.lst", base.display()), "").unwrap();
        let (l1, l2) = listing_paths(&cfg).unwrap();
        assert!(l1.ends_with("home_alice_ssh_dir..webdav_ssh.path1.lst"));
        assert!(l2.ends_with("home_alice_ssh_dir..webdav_ssh.path2.lst"));
    }

    #[test]
    fn finds_listings_named_for_an_alias_target() {
        let dir = tempfile::tempdir().unwrap();
        let json = crate::config::tests::PAIR
            .replace(
                "/home/alice/.cache/rclone/bisync",
                &dir.path().to_string_lossy(),
            )
            .replace("/home/alice/.ssh", "/home/alice/ssh dir");
        let cfg = File::parse(&json).unwrap().into_pair().unwrap();
        // rclone names an alias by what it resolves to, not by its own name.
        let base = dir.path().join("home_alice_ssh_dir..srv_data_ssh");
        std::fs::write(format!("{}.path1.lst", base.display()), "").unwrap();
        std::fs::write(format!("{}.path2.lst", base.display()), "").unwrap();
        assert!(listing_paths(&cfg).is_some());
        // Two candidates: ambiguous, so not guessed.
        let other = dir.path().join("home_alice_ssh_dir..elsewhere");
        std::fs::write(format!("{}.path1.lst", other.display()), "").unwrap();
        std::fs::write(format!("{}.path2.lst", other.display()), "").unwrap();
        assert_eq!(listing_paths(&cfg), None);
    }

    #[test]
    fn classifies_failures() {
        let ok = JobOutcome {
            success: true,
            error: String::new(),
            output: Value::Null,
        };
        assert!(classify(&ok, &Report::default()).is_none());

        // The real shape: the job's error is generic, the cause is in the log,
        // colourised.
        let missing = JobOutcome {
            success: false,
            error: "bisync aborted".into(),
            output: Value::Null,
        };
        let log = Report {
            output: format!(
                "2026/10/02 13:13:35 ERROR : \u{1b}[31mBisync critical error: {NO_PRIOR}, likely due to critical error on prior run \n\u{1b}[35mTip: ...\n2026/10/02 13:13:35 ERROR : \u{1b}[33mBisync aborted.\u{1b}[0m\n"
            ),
            ..Default::default()
        };
        let f = classify(&missing, &log).unwrap();
        assert_eq!(f.kind, FailureKind::NoPriorListings);
        assert_eq!(
            f.to_string(),
            format!("Bisync critical error: {NO_PRIOR}, likely due to critical error on prior run")
        );

        let dir = tempfile::tempdir().unwrap();
        let l1 = dir.path().join("x.path1.lst");
        std::fs::write(err_sibling(&l1), "").unwrap();
        let locked = Report {
            listing1: Some(l1),
            ..log.clone()
        };
        assert_eq!(
            classify(&missing, &locked).unwrap().kind,
            FailureKind::CriticalLockout
        );

        let other = JobOutcome {
            success: false,
            error: "boom".into(),
            output: Value::Null,
        };
        assert_eq!(
            classify(&other, &Report::default()).unwrap().kind,
            FailureKind::Other
        );
    }
}
