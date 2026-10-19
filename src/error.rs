use thiserror::Error;

#[derive(Debug, Error)]
pub enum RiptideError {
    /// Boxed: `reqwest::Error` is over 100 bytes, and it appears in `Result` in the
    /// innermost loops of the engine, so boxing it keeps every `Result` small.
    #[error("http error: {0}")]
    Http(Box<reqwest::Error>),

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
        RiptideError::Http(Box::new(e))
    }
}

pub type Result<T> = std::result::Result<T, RiptideError>;
