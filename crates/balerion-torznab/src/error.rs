/// Everything the Torznab client can complain about.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No indexer is configured. A feature that was never switched on rather
    /// than a failure, and reported separately so the player can say which.
    #[error("no Torznab indexer is configured (set BALERION_TORZNAB and BALERION_TORZNAB_KEY)")]
    NotConfigured,

    #[error("the indexer refused the key")]
    BadKey,

    #[error("the indexer said {status}")]
    Http { status: u16 },

    #[error("the indexer returned something unreadable: {0}")]
    Malformed(String),

    #[error(transparent)]
    Transport(#[from] reqwest::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unconfigured_client_says_which_variables_to_set() {
        // The message a first-time user is most likely to see.
        let said = Error::NotConfigured.to_string();
        assert!(said.contains("BALERION_TORZNAB"));
        assert!(said.contains("BALERION_TORZNAB_KEY"));
    }
}
