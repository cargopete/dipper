use std::time::Duration;

/// Everything that can go wrong while talking to the Internet Archive.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("invalid json from {context}: {source}")]
    Json {
        context: String,
        #[source]
        source: serde_json::Error,
    },

    /// The metadata API answers with an empty JSON array for items that do not
    /// exist (or are dark), rather than an error object.
    #[error("no such item: {0}")]
    ItemNotFound(String),

    #[error("item {identifier} has no {what}")]
    Missing { identifier: String, what: String },

    /// Archive.org rate limits hard; we surface this once the retry budget is
    /// spent so callers can slow down rather than hammer on.
    #[error("rate limited by archive.org after {attempts} attempts (last wait {wait:?})")]
    RateLimited { attempts: u32, wait: Duration },

    #[error("archive.org returned {status} for {url}")]
    Status {
        status: reqwest::StatusCode,
        url: String,
    },

    #[error("search error: {0}")]
    Search(String),

    #[error("malformed torrent: {0}")]
    Torrent(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
