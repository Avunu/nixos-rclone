//! Client for rclone's remote-control (rc) API, over the private unix socket of
//! this pair's own `rclone rcd` (see [`process`]).
//!
//! Short calls use the typed methods of `rclone-sdk`. Anything long-running is
//! submitted as an async job (`SDK::post_async`) and polled: the SDK's typed
//! methods model only the synchronous reply, and its default 15s timeout would
//! cut a bisync short. Params are plain JSON because the SDK's typed
//! `sync/bisync` lacks conflictResolve, conflictLoser, compare, recover,
//! maxLock and resyncMode.

pub mod ops;
pub mod process;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rclone_sdk::{Client, types};
use serde_json::{Value, json};

pub struct Rc {
    pub(crate) client: Client,
}

/// The finished state of an async rc job.
#[derive(Debug, Clone)]
pub struct JobOutcome {
    pub success: bool,
    /// rclone's error text, empty on success.
    pub error: String,
    /// The call's own result (for bisync: `output`, `listing1`, `listing2`, …).
    /// An async job keeps this even when the job failed.
    pub output: Value,
}

impl Rc {
    /// Connect to an rcd listening on the unix socket `sock`. No overall request
    /// timeout: jobs are polled, and a bisync of a large tree takes as long as
    /// it takes.
    pub fn unix(sock: &Path) -> Result<Self> {
        let http = reqwest::Client::builder()
            .unix_socket(sock)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .context("building rc http client")?;
        Ok(Self {
            client: Client::new_with_client("http://rclone", http),
        })
    }

    pub async fn version(&self) -> Result<String> {
        let body: types::CoreVersionRequest = serde_json::from_value(json!({}))?;
        let resp = self
            .client
            .core_version(None, None, None, &body)
            .await
            .map_err(|e| anyhow!("core/version: {e}"))?;
        Ok(resp.into_inner().version)
    }

    /// Submit `path` as an async job and return its id.
    pub async fn submit(&self, path: &str, params: &Value) -> Result<i64> {
        let resp = self
            .client
            .post_async(path, &[], params)
            .await
            .map_err(|e| anyhow!("submitting {path}: {e}"))?;
        Ok(resp.into_inner().jobid)
    }

    /// Poll until job `id` finishes.
    pub async fn wait(&self, id: i64) -> Result<JobOutcome> {
        // The id goes in the body, as a JSON number, and not in the query: the
        // SDK formats its `f64` query argument as "3.0", which rclone refuses
        // to parse as an int64. rclone lets the body override the query.
        let body: types::JobStatusRequest = serde_json::from_value(json!({ "jobid": id }))?;
        let mut delay = Duration::from_millis(50);
        loop {
            let st = self
                .client
                .job_status(None, None, None, &body)
                .await
                .map_err(|e| anyhow!("job/status {id}: {e}"))?
                .into_inner();
            if st.finished {
                return Ok(JobOutcome {
                    success: st.success,
                    error: st.error,
                    output: st.output.unwrap_or(Value::Null),
                });
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }

    /// `submit` + `wait`.
    pub async fn run_job(&self, path: &str, params: &Value) -> Result<JobOutcome> {
        let id = self.submit(path, params).await?;
        self.wait(id).await
    }
}
