//! Schema of the JSON files the NixOS module generates.
//!
//! Two kinds exist, told apart by a `kind` field:
//!
//! * `global` — one per system: config staging for FUSE mounts and resume
//!   recovery;
//! * `pair` — one per bisync pair, read by that pair's service. Separate files
//!   mean changing one pair restarts only its own unit.
//!
//! `deny_unknown_fields` is deliberate: the module and the binary ship together,
//! and `rclone-remotes validate` runs at build time, so drift between the Nix
//! options and this schema fails `nixos-rebuild` instead of being ignored at
//! runtime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

/// The only schema version this build understands.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum File {
    Global(Config),
    Pair(Box<PairConfig>),
}

impl File {
    pub fn load(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&raw).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let file: Self = serde_json::from_str(raw).context("parsing config")?;
        match &file {
            Self::Global(c) => c.validate()?,
            Self::Pair(p) => p.validate()?,
        }
        Ok(file)
    }

    pub fn into_global(self) -> Result<Config> {
        match self {
            Self::Global(c) => Ok(c),
            Self::Pair(_) => bail!("expected a global config, got a pair config"),
        }
    }

    pub fn into_pair(self) -> Result<PairConfig> {
        match self {
            Self::Pair(p) => Ok(*p),
            Self::Global(_) => bail!("expected a pair config, got a global config"),
        }
    }
}

fn check_version(version: u32) -> Result<()> {
    ensure!(
        version == SCHEMA_VERSION,
        "config schema version {version} not supported (this build speaks {SCHEMA_VERSION})"
    );
    Ok(())
}

// ── Global ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    /// Writable staging area for credential-backed rclone configs
    /// (`/run/rclone`); the module's mount options point into it.
    pub staging_dir: PathBuf,
    #[serde(default)]
    pub mounts: BTreeMap<String, Mount>,
    pub mount_reset: MountReset,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Mount {
    pub local_path: PathBuf,
    /// systemd unit stem of the mount (`utils.escapeSystemdPath localPath`).
    pub unit: String,
    /// Source rclone config to stage; `None` when the mount uses the owning
    /// user's own `rclone.conf`.
    pub config_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MountReset {
    /// Seconds to wait after resume for the network to settle.
    pub delay: u64,
}

impl Config {
    fn validate(&self) -> Result<()> {
        check_version(self.version)?;
        ensure!(
            self.staging_dir.is_absolute(),
            "stagingDir must be absolute: {}",
            self.staging_dir.display()
        );
        for name in self.mounts.keys() {
            // Names become file names under stagingDir.
            check_name(name)?;
        }
        for (name, m) in &self.mounts {
            ensure!(
                m.local_path.is_absolute(),
                "mounts.{name}.localPath must be absolute"
            );
            if let Some(c) = &m.config_file {
                ensure!(c.is_absolute(), "mounts.{name}.configFile must be absolute");
            }
        }
        Ok(())
    }

    /// Path the mount `name` reads its staged config from.
    pub fn staged_mount_config(&self, name: &str) -> PathBuf {
        self.staging_dir.join(format!("{name}.conf"))
    }
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.contains('/') || name.starts_with('.') {
        bail!("invalid remote name {name:?}");
    }
    Ok(())
}

// ── Pair ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairConfig {
    pub version: u32,
    pub name: String,
    /// The rclone binary to run as this pair's private `rcd`.
    pub rclone: PathBuf,
    /// Rclone remote path (Path2), e.g. `webdav:ssh`.
    pub remote: String,
    /// Local directory (Path1).
    pub local_path: PathBuf,
    /// Where bisync keeps its listings; reusing the historical default means an
    /// upgrade does not force a resync.
    pub workdir: PathBuf,
    /// Whether the unit was given the rclone config as a credential
    /// (`$CREDENTIALS_DIRECTORY/rclone.conf`). When false, rclone reads the
    /// running user's own `~/.config/rclone/rclone.conf`.
    pub config_credential: bool,
    #[serde(default)]
    pub excludes: Vec<String>,
    #[serde(default)]
    pub google_drive: Option<GoogleDrive>,
    #[serde(default)]
    pub sftp: Sftp,
    pub pull: Pull,
    pub conflict: Conflict,
    pub bisync: BisyncOptions,
    pub settle: Settle,
    pub push: Push,
    #[serde(default)]
    pub markdown_sync: Option<MarkdownSync>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Push {
    /// Watch the local tree and send changes to the remote as they happen,
    /// instead of waiting for the next pull.
    pub enable: bool,
    /// How long the tree must be quiet before a burst of changes is pushed
    /// (systemd time span).
    pub debounce: String,
}

/// Keep a directory of markdown notes in step with the synced docx tree.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MarkdownSync {
    /// The vault.
    pub path: PathBuf,
    pub sync_deletions: bool,
    pub track_moves: bool,
    /// A docx whose styles a note's first conversion starts from.
    #[serde(default)]
    pub reference_doc: Option<PathBuf>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GoogleDrive {
    pub export_formats: String,
    pub import_formats: String,
    pub root_folder_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Sftp {
    #[serde(default)]
    pub path_override: Option<String>,
    #[serde(default)]
    pub disable_hashcheck: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Pull {
    /// systemd time spans, parsed by [`crate::timespan`].
    pub interval: String,
    pub on_boot: String,
    pub jitter: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Conflict {
    pub resolve: String,
    pub loser: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BisyncOptions {
    pub compare: String,
    pub resilient: bool,
    pub recover: bool,
    pub create_empty_src_dirs: bool,
    pub max_lock: String,
    #[serde(default)]
    pub max_delete: Option<u8>,
    /// Raw rc parameters merged over everything above; the escape hatch that
    /// replaced `extraArgs`.
    #[serde(default)]
    pub extra_params: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settle {
    pub enable: bool,
    /// Seconds between the two passes.
    pub delay: u64,
}

impl PairConfig {
    fn validate(&self) -> Result<()> {
        check_version(self.version)?;
        check_name(&self.name)?;
        ensure!(self.rclone.is_absolute(), "rclone must be an absolute path");
        ensure!(
            self.local_path.is_absolute(),
            "localPath must be absolute: {}",
            self.local_path.display()
        );
        ensure!(
            self.workdir.is_absolute(),
            "workdir must be absolute: {}",
            self.workdir.display()
        );
        for (what, v) in [
            ("pull.interval", &self.pull.interval),
            ("pull.onBoot", &self.pull.on_boot),
            ("pull.jitter", &self.pull.jitter),
        ] {
            crate::timespan::parse(v).with_context(|| what.to_string())?;
        }
        let debounce = crate::timespan::parse(&self.push.debounce).context("push.debounce")?;
        ensure!(
            debounce >= std::time::Duration::from_millis(100),
            "push.debounce must be at least 100ms"
        );
        if let Some(m) = &self.markdown_sync {
            ensure!(
                m.path.is_absolute(),
                "markdownSync.path must be absolute: {}",
                m.path.display()
            );
            ensure!(
                m.path != self.local_path
                    && !m.path.starts_with(&self.local_path)
                    && !self.local_path.starts_with(&m.path),
                "markdownSync.path and localPath must be separate directories"
            );
            if let Some(r) = &m.reference_doc {
                ensure!(
                    r.is_absolute(),
                    "markdownSync.referenceDoc must be absolute"
                );
            }
        }
        ensure!(
            crate::timespan::parse(&self.pull.interval)? > std::time::Duration::ZERO,
            "pull.interval must be positive"
        );
        if let Some(d) = self.bisync.max_delete {
            ensure!(d <= 100, "bisync.maxDelete is a percentage (0-100)");
        }
        Ok(())
    }

    pub fn pull_interval(&self) -> std::time::Duration {
        crate::timespan::parse(&self.pull.interval).unwrap_or_default()
    }
    pub fn pull_on_boot(&self) -> std::time::Duration {
        crate::timespan::parse(&self.pull.on_boot).unwrap_or_default()
    }
    pub fn push_debounce(&self) -> std::time::Duration {
        crate::timespan::parse(&self.push.debounce).unwrap_or_default()
    }
    pub fn pull_jitter(&self) -> std::time::Duration {
        crate::timespan::parse(&self.pull.jitter).unwrap_or_default()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const GLOBAL: &str = r#"{
        "kind": "global",
        "version": 1,
        "stagingDir": "/run/rclone",
        "mounts": {"docs": {"localPath": "/mnt/docs", "unit": "mnt-docs", "configFile": "/etc/r.conf"}},
        "mountReset": {"delay": 15}
    }"#;

    pub(crate) const PAIR: &str = r##"{
        "kind": "pair",
        "version": 1,
        "name": "ssh",
        "rclone": "/nix/store/x-rclone/bin/rclone",
        "remote": "webdav:ssh",
        "localPath": "/home/alice/.ssh",
        "workdir": "/home/alice/.cache/rclone/bisync",
        "configCredential": true,
        "excludes": ["#recycle/**"],
        "googleDrive": null,
        "sftp": {"pathOverride": "@/volume1", "disableHashcheck": false},
        "pull": {"interval": "15min", "onBoot": "5min", "jitter": "5min"},
        "conflict": {"resolve": "newer", "loser": "delete"},
        "bisync": {"compare": "size,modtime,checksum", "resilient": true, "recover": true,
                   "createEmptySrcDirs": true, "maxLock": "5m", "maxDelete": null, "extraParams": {}},
        "settle": {"enable": true, "delay": 30},
        "push": {"enable": true, "debounce": "2s"}
    }"##;

    #[test]
    fn parses_global() {
        let c = File::parse(GLOBAL).unwrap().into_global().unwrap();
        assert_eq!(c.mount_reset.delay, 15);
        assert_eq!(
            c.staged_mount_config("docs"),
            PathBuf::from("/run/rclone/docs.conf")
        );
    }

    #[test]
    fn parses_pair() {
        let p = File::parse(PAIR).unwrap().into_pair().unwrap();
        assert_eq!(p.pull_interval().as_secs(), 900);
        assert_eq!(p.sftp.path_override.as_deref(), Some("@/volume1"));
        assert!(File::parse(PAIR).unwrap().into_global().is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let bad = GLOBAL.replace("\"delay\": 15", "\"delay\": 15, \"bogus\": 1");
        assert!(File::parse(&bad).is_err());
        let bad = PAIR.replace("\"delay\": 30", "\"delay\": 30, \"bogus\": 1");
        assert!(File::parse(&bad).is_err());
    }

    #[test]
    fn rejects_wrong_version() {
        let bad = GLOBAL.replace("\"version\": 1", "\"version\": 2");
        let err = format!("{:#}", File::parse(&bad).unwrap_err());
        assert!(err.contains("version 2"), "{err}");
    }

    #[test]
    fn rejects_path_traversal_in_names() {
        assert!(File::parse(&GLOBAL.replace("\"docs\"", "\"../docs\"")).is_err());
        assert!(File::parse(&PAIR.replace("\"ssh\"", "\"a/b\"")).is_err());
    }

    #[test]
    fn rejects_relative_paths() {
        assert!(File::parse(&GLOBAL.replace("/mnt/docs", "mnt/docs")).is_err());
        assert!(File::parse(&PAIR.replace("/home/alice/.ssh", "ssh")).is_err());
    }

    #[test]
    fn markdown_sync_must_be_a_separate_directory() {
        let with = |md: &str| {
            PAIR.replace(
                "\"push\": {",
                &format!("\"markdownSync\": {{\"path\": \"{md}\", \"syncDeletions\": true, \"trackMoves\": true}}, \"push\": {{"),
            )
        };
        let p = File::parse(&with("/home/alice/vault"))
            .unwrap()
            .into_pair()
            .unwrap();
        assert_eq!(
            p.markdown_sync.unwrap().path,
            PathBuf::from("/home/alice/vault")
        );
        assert!(
            File::parse(&with("/home/alice/.ssh")).is_err(),
            "same as localPath"
        );
        assert!(
            File::parse(&with("/home/alice/.ssh/vault")).is_err(),
            "inside localPath"
        );
        assert!(
            File::parse(&with("/home/alice")).is_err(),
            "contains localPath"
        );
        assert!(File::parse(&with("vault")).is_err(), "relative");
    }

    #[test]
    fn rejects_bad_timespans_and_percentages() {
        assert!(File::parse(&PAIR.replace("15min", "soon")).is_err());
        assert!(File::parse(&PAIR.replace("\"maxDelete\": null", "\"maxDelete\": 101")).is_err());
        assert!(
            File::parse(&PAIR.replace("\"debounce\": \"2s\"", "\"debounce\": \"10ms\"")).is_err()
        );
    }
}
