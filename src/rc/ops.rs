//! The push engine's view of the remote, over rc's `operations/*` calls.
//!
//! These are short calls and are made synchronously. Submitting each as an
//! async job would leave one finished job per file in rcd's memory until it
//! expires.

use std::sync::Arc;

use rclone_sdk::{Error, types};
use serde_json::json;

use super::Rc;
use crate::push::{OpError, Remote};

pub struct RcRemote {
    pub rc: Arc<Rc>,
    /// The local sync root, as an rclone `fs` string.
    pub local: String,
    /// The remote root (`webdav:ssh`), as an rclone `fs` string.
    pub remote: String,
}

/// Messages that mean "try again later", not "this request is wrong".
const TRANSIENT: &[&str] = &[
    "connection refused",
    "connection reset",
    "broken pipe",
    "timeout",
    "timed out",
    "deadline exceeded",
    "no such host",
    "network is unreachable",
    "temporary failure",
    "try again",
    "rate limit",
    "too many requests",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
    "unexpected eof",
    "429",
    "502",
    "503",
    "504",
];

pub(crate) fn classify(message: String) -> OpError {
    let lower = message.to_lowercase();
    let not_found = lower.contains("not found") || lower.contains("no such file");
    let transient = !not_found && TRANSIENT.iter().any(|h| lower.contains(h));
    OpError {
        not_found,
        transient,
        message,
    }
}

fn from_sdk(e: Error<types::RcError>) -> OpError {
    match e {
        Error::ErrorResponse(r) => classify(r.into_inner().error),
        // rcd unreachable or the connection dropped: it will be back, or the
        // service restarts.
        Error::CommunicationError(e) => OpError {
            not_found: false,
            transient: true,
            message: e.to_string(),
        },
        other => OpError {
            not_found: false,
            transient: false,
            message: other.to_string(),
        },
    }
}

impl Remote for RcRemote {
    async fn copy_file(&self, rel: &str) -> Result<(), OpError> {
        let body: types::OperationsCopyfileRequest = serde_json::from_value(json!({
            "srcFs": self.local, "srcRemote": rel,
            "dstFs": self.remote, "dstRemote": rel,
        }))
        .map_err(|e| classify(e.to_string()))?;
        self.rc
            .client
            .operations_copyfile(None, None, None, None, None, None, None, &body)
            .await
            .map(|_| ())
            .map_err(from_sdk)
    }

    async fn move_file(&self, from: &str, to: &str) -> Result<(), OpError> {
        // Within the remote: a server-side move where the backend has one.
        let body: types::OperationsMovefileRequest = serde_json::from_value(json!({
            "srcFs": self.remote, "srcRemote": from,
            "dstFs": self.remote, "dstRemote": to,
        }))
        .map_err(|e| classify(e.to_string()))?;
        self.rc
            .client
            .operations_movefile(None, None, None, None, None, None, None, &body)
            .await
            .map(|_| ())
            .map_err(from_sdk)
    }

    async fn delete_file(&self, rel: &str) -> Result<(), OpError> {
        let body: types::OperationsDeletefileRequest =
            serde_json::from_value(json!({ "fs": self.remote, "remote": rel }))
                .map_err(|e| classify(e.to_string()))?;
        self.rc
            .client
            .operations_deletefile(None, None, None, None, None, &body)
            .await
            .map(|_| ())
            .map_err(from_sdk)
    }

    async fn remove_dir(&self, rel: &str) -> Result<(), OpError> {
        let body: types::OperationsRmdirRequest =
            serde_json::from_value(json!({ "fs": self.remote, "remote": rel }))
                .map_err(|e| classify(e.to_string()))?;
        self.rc
            .client
            .operations_rmdir(None, None, None, None, None, &body)
            .await
            .map(|_| ())
            .map_err(from_sdk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_rclone_errors() {
        assert!(classify("object not found".into()).not_found);
        assert!(classify("directory not found".into()).not_found);
        let t = classify("Get \"https://x\": dial tcp: connection refused".into());
        assert!(t.transient && !t.not_found);
        assert!(classify("googleapi: Error 503: backend error".into()).transient);
        assert!(classify("read: i/o timeout".into()).transient);
        let p = classify("permission denied".into());
        assert!(!p.transient && !p.not_found);
        // "not found" wins: a missing file is permanent however the error is dressed.
        assert!(!classify("503 object not found".into()).transient);
    }
}
