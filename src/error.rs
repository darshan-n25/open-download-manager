use std::error::Error as _;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RiptideError {
    /// An HTTP-level failure.
    ///
    /// The message carries the whole cause chain, not `reqwest`'s summary: "builder
    /// error" on its own says nothing about *which* builder failed and why, and this
    /// error is what a user sees when a download does not start.
    #[error("http error: {0}")]
    Http(String),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid url: {0}")]
    Url(#[from] url::ParseError),

    #[error("malformed state file: {0}")]
    Json(#[from] serde_json::Error),

    #[error("malformed manifest: {0}")]
    Manifest(String),

    #[error("server does not support byte ranges")]
    NoRangeSupport,

    #[error("authentication required ({scheme})")]
    AuthRequired { scheme: String },

    #[error("authentication failed: {0}")]
    AuthFailed(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("download cancelled")]
    Cancelled,

    /// The bytes-per-window budget ran out. Distinct from `Cancelled` because the job is
    /// not suspended by the user and it will become runnable again on its own; the engine
    /// records it as `Paused` so the queue can retry it after the window rolls.
    #[error("{0}")]
    QuotaExhausted(#[from] crate::quota::QuotaExhausted),

    #[error("not supported: {0}")]
    Unsupported(String),

    /// The downloaded bytes did not match the expected digest. The working file has already
    /// been removed by the time this is returned, so nothing is left that could be mistaken
    /// for a good download.
    #[error("{algorithm} checksum mismatch for {path}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        algorithm: String,
        expected: String,
        actual: String,
        path: String,
    },

    #[error("no such job: {0}")]
    NoSuchJob(String),

    #[error("no such project: {0}")]
    NoSuchProject(String),

    #[error("{0}")]
    Other(String),
}

impl RiptideError {
    pub fn other<S: Into<String>>(s: S) -> Self {
        RiptideError::Other(s.into())
    }
}

impl From<reqwest::Error> for RiptideError {
    fn from(e: reqwest::Error) -> Self {
        RiptideError::Http(http_cause_chain(&e))
    }
}

/// The causes behind a `reqwest` error, as one readable string.
///
/// `reqwest`'s own message is a summary ("builder error", "error sending request"); the
/// reason is in the source chain, and a download manager that only says "builder error"
/// has told the user nothing.
fn http_cause_chain(e: &reqwest::Error) -> String {
    let mut out = e.to_string();
    let mut src = e.source();
    while let Some(cause) = src {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        src = cause.source();
    }
    out
}

pub type Result<T> = std::result::Result<T, RiptideError>;
