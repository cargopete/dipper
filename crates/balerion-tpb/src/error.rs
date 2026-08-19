/// Everything that can go wrong while talking to apibay.
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

    #[error("apibay returned {status} for {url}")]
    Status {
        status: reqwest::StatusCode,
        url: String,
    },

    /// A row arrived that cannot be turned into something balerion can open.
    /// Not fatal on its own: [`crate::search`] drops the row and carries on.
    #[error("unusable row {id}: {why}")]
    Unusable { id: String, why: String },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
