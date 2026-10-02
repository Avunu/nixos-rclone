//! Supervision of the private `rclone rcd` child.
//!
//! One rcd per pair, not one shared: rclone captures a bisync's log output
//! through a process-global handler, so two concurrent bisyncs in one rcd would
//! interleave and lose each other's `output`. It also keeps each pair's config,
//! restarts and failures independent.

use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};

use super::Rc;

pub struct RcdSpec {
    pub rclone: PathBuf,
    pub socket: PathBuf,
    /// rclone config file; `None` leaves rclone to find the user's own.
    pub config: Option<PathBuf>,
    /// Extra environment, for backend options that apply to every remote this
    /// rcd talks to (`RCLONE_DRIVE_EXPORT_FORMATS`, …).
    pub env: Vec<(String, String)>,
}

pub struct Rcd {
    child: Child,
}

impl Rcd {
    /// Start rcd and wait until it answers on its socket.
    pub async fn spawn(spec: &RcdSpec) -> Result<(Self, Rc)> {
        // A stale socket from a previous run would make rcd fail to bind.
        match std::fs::remove_file(&spec.socket) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("removing stale rc socket"),
        }

        let mut cmd = Command::new(&spec.rclone);
        cmd.arg("rcd")
            .arg("--rc-addr")
            .arg(format!("unix://{}", spec.socket.display()))
            // The socket lives in a directory only this pair's user can enter.
            .arg("--rc-no-auth")
            // Long enough that a finished job outlives any polling gap.
            .arg("--rc-job-expire-duration=1h")
            .arg("--log-level=INFO")
            .stdin(Stdio::null())
            .kill_on_drop(true);
        if let Some(c) = &spec.config {
            cmd.env("RCLONE_CONFIG", c);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let child = cmd
            .spawn()
            .with_context(|| format!("starting {}", spec.rclone.display()))?;
        let mut rcd = Self { child };
        let rc = Rc::unix(&spec.socket)?;

        let mut waited = Duration::ZERO;
        loop {
            if let Some(status) = rcd.child.try_wait()? {
                bail!("rclone rcd exited during startup: {status}");
            }
            if rc.version().await.is_ok() {
                return Ok((rcd, rc));
            }
            if waited > Duration::from_secs(30) {
                bail!(
                    "rclone rcd did not answer on {} within 30s",
                    spec.socket.display()
                );
            }
            let step = Duration::from_millis(100);
            tokio::time::sleep(step).await;
            waited += step;
        }
    }

    /// Resolves when the child exits.
    pub async fn exited(&mut self) -> Result<ExitStatus> {
        Ok(self.child.wait().await?)
    }

    /// Ask rcd to stop (SIGTERM cancels its running jobs cleanly), escalating
    /// to SIGKILL if it does not.
    pub async fn shutdown(&mut self, grace: Duration) {
        if let Some(pid) = self
            .child
            .id()
            .and_then(|p| rustix::process::Pid::from_raw(p as i32))
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        if tokio::time::timeout(grace, self.child.wait())
            .await
            .is_err()
        {
            let _ = self.child.kill().await;
        }
    }
}

/// Environment for the backend options a pair sets, which rclone reads on top
/// of the config file. They apply to every remote of that backend type that
/// this pair's rcd opens, as the equivalent command-line flags used to.
pub fn backend_env(cfg: &crate::config::PairConfig) -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Some(g) = &cfg.google_drive {
        env.push((
            "RCLONE_DRIVE_EXPORT_FORMATS".into(),
            g.export_formats.clone(),
        ));
        env.push((
            "RCLONE_DRIVE_IMPORT_FORMATS".into(),
            g.import_formats.clone(),
        ));
        if let Some(id) = &g.root_folder_id {
            env.push(("RCLONE_DRIVE_ROOT_FOLDER_ID".into(), id.clone()));
        }
    }
    if let Some(p) = &cfg.sftp.path_override {
        env.push(("RCLONE_SFTP_PATH_OVERRIDE".into(), p.clone()));
    }
    if cfg.sftp.disable_hashcheck {
        env.push(("RCLONE_SFTP_DISABLE_HASHCHECK".into(), "true".into()));
    }
    env
}
