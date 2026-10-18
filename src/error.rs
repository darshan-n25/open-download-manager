use thiserror::Error;

#[derive(Debug, Error)]
pub enum RiptideError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

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

    #[error("not supported: {0}")]
    Unsupported(String),

    #[error("no such job: {0}")]
    NoSuchJob(String),

    #[error("{0}")]
    Other(String),
}

impl RiptideError {
    pub fn other<S: Into<String>>(s: S) -> Self {
        RiptideError::Other(s.into())
    }
}

pub type Result<T> = std::result::Result<T, RiptideError>;
