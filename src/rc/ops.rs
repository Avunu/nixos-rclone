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

/// Of those, the ones only a server can have answered. Named separately because
/// "gateway timeout" also reads as a network timeout.
const SERVER_ANSWERED: &[&str] = &[
    "rate limit",
    "too many requests",
    "service unavailable",
    "bad gateway",
    "gateway timeout",
];
/// Status codes, matched whole: a refused connection to port 5029 is no 502.
const SERVER_ANSWERED_CODES: &[&str] = &["429", "502", "503", "504"];

fn server_answered(text: &str) -> bool {
    SERVER_ANSWERED.iter().any(|h| text.contains(h))
        || text
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|word| SERVER_ANSWERED_CODES.contains(&word))
}

pub(crate) fn classify(message: String) -> OpError {
    let lower = message.to_lowercase();
    // rclone quotes a server's error page ahead of the status, and its words say
    // nothing about this request: the page Synology's proxy serves for a 502
    // says "not found" in its script. Judge only what follows it.
    let text = lower
        .rfind("</html>")
        .map_or(lower.as_str(), |i| &lower[i..]);
    let not_found = text.contains("not found") || text.contains("no such file");
    let transient = !not_found && TRANSIENT.iter().any(|h| text.contains(h));
    OpError {
        not_found,
        transient,
        server_answered: !transient || server_answered(text),
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
            server_answered: false,
            message: e.to_string(),
        },
        other => OpError {
            not_found: false,
            transient: false,
            server_answered: false,
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

    #[test]
    fn tells_a_server_that_answered_from_one_that_could_not_be_reached() {
        assert!(classify("copy call failed: 502 Bad Gateway".into()).server_answered);
        assert!(classify("504 Gateway Timeout".into()).server_answered);
        assert!(classify("googleapi: Error 503: backend error".into()).server_answered);
        for unreachable in [
            "dial tcp 10.0.0.1:5029: connection refused",
            "dial tcp: lookup webdav.example: no such host",
            "read: i/o timeout",
            "connect: network is unreachable",
        ] {
            let e = classify(unreachable.into());
            assert!(e.transient && !e.server_answered, "{unreachable}");
        }
    }

    #[test]
    fn a_servers_error_page_is_not_read_as_the_error() {
        // The shape Synology's proxy really produced, for a file that exists.
        let page = "<!DOCTYPE html>\n<html><script>var a={en:\"Sorry, the page you are looking for is not found.\"}</script>\n</html>";
        let e = classify(format!("copy call failed: {page}: 502 Bad Gateway"));
        assert!(e.transient && !e.not_found, "{e:?}");
        // Digits in an embedded image are not a status either.
        let e = classify("<html>data:image/jpg;base64,AB502C</html>: 403 Forbidden".into());
        assert!(!e.transient && !e.not_found, "{e:?}");
        // A real 404 still counts, page or no page.
        let e = classify(format!("{page}: 404 Not Found"));
        assert!(e.not_found, "{e:?}");
    }
}
