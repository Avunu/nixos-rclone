//! Shared harness: the real daemon driving a real `rclone rcd` over local
//! directories. Needs `rclone` on PATH (the Nix check provides it).
#![allow(dead_code)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rclone_remotes::config::{File, PairConfig};
use rclone_remotes::ctl::{self, Request, Response};
use rclone_remotes::pair::{self, Runtime};
use serde_json::json;
use tempfile::TempDir;
use tokio::task::JoinHandle;

pub fn rclone_bin() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("rclone"))
                .find(|c| c.is_file())
        })
        .expect("rclone must be on PATH to run the integration tests")
}

#[derive(Clone, Copy, Default)]
pub struct Options {
    pub settle: bool,
    pub push: bool,
    /// Enable markdownSync, with the vault at [`Harness::vault`].
    pub markdown: bool,
}

pub struct Harness {
    _tmp: TempDir,
    pub local: PathBuf,
    pub remote: PathBuf,
    pub vault: PathBuf,
    pub workdir: PathBuf,
    pub cfg: PairConfig,
    socket: PathBuf,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Harness {
    pub async fn start(opts: Options) -> Self {
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let (local, remote) = (root.join("local dir"), root.join("remote"));
        let vault = root.join("Obsidian Vault");
        let (workdir, run, state) = (root.join("workdir"), root.join("run"), root.join("state"));
        for d in [&local, &remote, &workdir, &run, &state, &vault] {
            fs::create_dir_all(d).unwrap();
        }
        let cfg = json!({
            "kind": "pair", "version": 1, "name": "test",
            "rclone": rclone_bin(),
            "remote": remote, "localPath": local, "workdir": workdir,
            "configCredential": false,
            "excludes": ["#recycle/**"],
            "googleDrive": null,
            "sftp": {"pathOverride": null, "disableHashcheck": false},
            // The timer must stay out of the test's way.
            "pull": {"interval": "1h", "onBoot": "1h", "jitter": "0"},
            "conflict": {"resolve": "newer", "loser": "delete"},
            "bisync": {"compare": "size,modtime,checksum", "resilient": true, "recover": true,
                       "createEmptySrcDirs": true, "maxLock": "5m", "maxDelete": null,
                       "extraParams": {}},
            "settle": {"enable": opts.settle, "delay": 1},
            "push": {"enable": opts.push, "debounce": "300ms"},
            "markdownSync": if opts.markdown {
                json!({"path": vault, "syncDeletions": true, "trackMoves": true})
            } else { serde_json::Value::Null },
        });
        let cfg = File::parse(&cfg.to_string()).unwrap().into_pair().unwrap();
        let socket = run.join("ctl.sock");
        let task = tokio::spawn(pair::run(
            cfg.clone(),
            Runtime {
                dir: run,
                state: Some(state),
                credentials: None,
            },
        ));
        let h = Self {
            _tmp: tmp,
            local,
            remote,
            vault,
            workdir,
            cfg,
            socket,
            task,
        };
        h.wait_ready().await;
        h
    }

    async fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !self.socket.exists() {
            assert!(!self.task.is_finished(), "daemon exited during startup");
            assert!(Instant::now() < deadline, "daemon never became ready");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn ctl(&self, req: Request) -> Response {
        let socket = self.socket.clone();
        tokio::task::spawn_blocking(move || ctl::call(&socket, req).unwrap())
            .await
            .unwrap()
    }

    pub async fn sync_ok(&self) -> Response {
        let r = self.ctl(Request::Sync).await;
        assert!(r.ok, "sync failed: {:?}", r.message);
        r
    }

    /// Poll until `cond` holds, failing the test with `what` after 15s.
    pub async fn eventually(&self, what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !cond() {
            assert!(
                !self.task.is_finished(),
                "daemon exited while waiting for: {what}"
            );
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Dropping the task drops the supervisor, which kills rcd.
        self.task.abort();
    }
}

pub fn put(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

pub fn read(dir: &Path, rel: &str) -> Option<String> {
    fs::read_to_string(dir.join(rel)).ok()
}

pub fn inode(dir: &Path, rel: &str) -> Option<u64> {
    fs::metadata(dir.join(rel)).ok().map(|m| m.ino())
}

/// Every file under `dir`, relative, sorted.
pub fn tree(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push(p.strip_prefix(base).unwrap().to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}
