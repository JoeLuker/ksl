use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// 403/429/5xx from the site. KSL fronts with PerimeterX; bursts of
    /// requests earn a window of these. Callers should back off.
    #[error("throttled or blocked by server (HTTP {status})")]
    Throttled { status: u16 },

    #[error("unexpected HTTP status {status} for {url}")]
    Status { status: u16, url: String },

    #[error("server-action discovery failed: {0}")]
    Discovery(String),

    #[error("failed to parse server response: {0}")]
    Parse(String),

    #[error("listing {0}: no schema.org Product JSON-LD found")]
    NoStructuredData(u64),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
