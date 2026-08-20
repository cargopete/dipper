/// Everything the OpenSubtitles client can complain about.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No API key was configured. Not a failure so much as a feature that was
    /// never switched on, and reported separately so the player can say which.
    #[error("no OpenSubtitles API key is configured (set OPENSUBTITLES_API_KEY)")]
    NoKey,

    /// The daily download allowance is spent. Distinct from every other error
    /// because it is the one that comes back tomorrow on its own.
    #[error("the OpenSubtitles download quota is spent; it resets every 24 hours")]
    QuotaSpent,

    #[error("OpenSubtitles refused the API key")]
    BadKey,

    #[error("OpenSubtitles said {status}: {body}")]
    Http { status: u16, body: String },

    #[error("OpenSubtitles returned something unreadable: {0}")]
    Malformed(String),

    #[error(transparent)]
    Transport(#[from] reqwest::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    /// Is waiting the right response to this?
    ///
    /// Only the quota, and only for a day. Everything else here is either a
    /// configuration problem or a genuine failure, and retrying either of those
    /// spends requests against a limit that is already tight.
    pub fn is_temporary(&self) -> bool {
        matches!(self, Error::QuotaSpent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_quota_is_worth_waiting_out() {
        assert!(Error::QuotaSpent.is_temporary());
        assert!(!Error::NoKey.is_temporary());
        assert!(!Error::BadKey.is_temporary());
        assert!(
            !Error::Malformed("x".into()).is_temporary(),
            "retrying a parse failure spends a request to fail identically"
        );
    }

    #[test]
    fn a_missing_key_says_which_variable_to_set() {
        // The error a first-time user is most likely to see, so it had better
        // say what to do rather than only what went wrong.
        assert!(Error::NoKey.to_string().contains("OPENSUBTITLES_API_KEY"));
    }
}
