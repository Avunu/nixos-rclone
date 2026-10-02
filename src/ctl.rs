//! Control socket of a running pair: newline-delimited JSON over a unix socket,
//! used by `rclone-remotes ctl` (and by the VM tests) to ask for a sync or read
//! the status without waiting for the timer.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as AsyncBufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    /// Report the current state, without doing anything.
    Status,
    /// Run a pull now and answer when it has finished.
    Sync,
    /// Rebuild the listings (`--resync`), the way out of a critical lockout.
    Resync,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Starting,
    Idle,
    Syncing,
    /// The last pass failed; the next scheduled one will retry.
    Failed,
    /// rclone locked the pair out after a critical error. Not retried until
    /// `ctl resync`: resyncing automatically could hide what went wrong.
    NeedsResync,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: State,
    /// Unix seconds.
    pub last_success: Option<u64>,
    pub last_error: Option<String>,
    /// Completed bisync passes (a settle pass counts).
    pub passes: u64,
    /// How often this process started from scratch with `--resync`.
    pub resyncs: u64,
    /// What local changes pushed to the remote so far, without waiting for a pull.
    pub pushed: Pushed,
    /// Pushes that failed for a transient reason and will be retried.
    pub pending: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Pushed {
    pub uploaded: u64,
    pub moved: u64,
    pub deleted: u64,
}

impl Status {
    pub fn new() -> Self {
        Self {
            state: State::Starting,
            last_success: None,
            last_error: None,
            passes: 0,
            resyncs: 0,
            pushed: Pushed::default(),
            pending: 0,
        }
    }
}

impl Default for Status {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: Option<String>,
    pub status: Status,
}

pub type Call = (Request, oneshot::Sender<Response>);

/// `/run/rclone-remotes/<name>/ctl.sock`: the unit's `RuntimeDirectory`.
pub fn default_socket(name: &str) -> PathBuf {
    Path::new("/run/rclone-remotes").join(name).join("ctl.sock")
}

/// Serve requests from `listener`, forwarding each to the pair's actor.
pub async fn serve(listener: UnixListener, calls: mpsc::Sender<Call>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let calls = calls.clone();
        tokio::spawn(async move {
            let (rd, mut wr) = stream.into_split();
            let mut lines = AsyncBufReader::new(rd).lines();
            let Ok(Some(line)) = lines.next_line().await else {
                return;
            };
            let reply = match serde_json::from_str::<Request>(&line) {
                Err(e) => format!("{{\"error\":\"bad request: {e}\"}}"),
                Ok(req) => {
                    let (tx, rx) = oneshot::channel();
                    if calls.send((req, tx)).await.is_err() {
                        return;
                    }
                    match rx.await {
                        Ok(resp) => serde_json::to_string(&resp).unwrap_or_default(),
                        Err(_) => return,
                    }
                }
            };
            let _ = wr.write_all(reply.as_bytes()).await;
            let _ = wr.write_all(b"\n").await;
        });
    }
}

/// Blocking client for the CLI.
pub fn call(socket: &Path, req: Request) -> Result<Response> {
    let mut stream = UnixStream::connect(socket).with_context(|| {
        format!(
            "connecting to {} (is the service running?)",
            socket.display()
        )
    })?;
    writeln!(stream, "{}", serde_json::to_string(&req)?)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    serde_json::from_str(&line).with_context(|| format!("unexpected reply: {line:?}"))
}
