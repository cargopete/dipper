//! Talking to the API, politely and within a very small allowance.

use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use crate::error::{Error, Result};

pub const API_BASE: &str = "https://api.opensubtitles.com/api/v1";

/// Their published limit is forty requests per ten seconds. This is a quarter
/// of that, because nothing here is in a hurry and the download quota runs out
/// long before the request rate does.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(1_000);

const DEFAULT_UA: &str = concat!("balerion v", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Their terms ask for a real application name and version here, and they
    /// reject a generic one, so this is not decoration.
    pub user_agent: String,
    pub min_interval: Duration,
    pub timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_UA.to_string(),
            min_interval: DEFAULT_MIN_INTERVAL,
            timeout: Duration::from_secs(20),
        }
    }
}

/// An HTTP client for OpenSubtitles.
#[derive(Debug)]
pub struct OsdbClient {
    http: reqwest::Client,
    api_key: String,
    cfg: ClientConfig,
    last_request: Mutex<Option<Instant>>,
}

impl OsdbClient {
    /// Build a client from the environment.
    ///
    /// Returns `None` rather than an error when no key is set, because having
    /// no OpenSubtitles account is an ordinary state to be in and not a
    /// misconfiguration to complain about.
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("OPENSUBTITLES_API_KEY").ok()?;
        Self::new(key.trim()).ok()
    }

    pub fn new(api_key: &str) -> Result<Self> {
        Self::with_config(api_key, ClientConfig::default())
    }

    pub fn with_config(api_key: &str, cfg: ClientConfig) -> Result<Self> {
        if api_key.is_empty() {
            return Err(Error::NoKey);
        }
        let http = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(cfg.timeout)
            .build()?;
        Ok(Self {
            http,
            api_key: api_key.to_string(),
            cfg,
            last_request: Mutex::new(None),
        })
    }

    /// Wait out the minimum spacing between requests.
    async fn pace(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(previous) = *last {
            let elapsed = previous.elapsed();
            if elapsed < self.cfg.min_interval {
                tokio::time::sleep(self.cfg.min_interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    pub(crate) async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.pace().await;
        let response = self
            .http
            .get(url)
            .header("Api-Key", &self.api_key)
            .header("Accept", "application/json")
            .send()
            .await?;
        self.read_json(response).await
    }

    pub(crate) async fn post_json<T: DeserializeOwned>(
        &self,
        url: &str,
        body: serde_json::Value,
    ) -> Result<T> {
        self.pace().await;
        let response = self
            .http
            .post(url)
            .header("Api-Key", &self.api_key)
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await?;
        self.read_json(response).await
    }

    /// Fetch a subtitle file from the temporary link a download returns.
    ///
    /// Deliberately not through `get_json`: the link is on their CDN rather
    /// than the API, wants no API key, and returns a subtitle file rather than
    /// JSON.
    pub async fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        let response = self.http.get(url).send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(Error::Http {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes).chars().take(200).collect(),
            });
        }
        Ok(bytes.to_vec())
    }

    async fn read_json<T: DeserializeOwned>(&self, response: reqwest::Response) -> Result<T> {
        let status = response.status();
        let bytes = response.bytes().await?;

        if !status.is_success() {
            let body: String = String::from_utf8_lossy(&bytes).chars().take(300).collect();
            return Err(classify(status.as_u16(), &body));
        }
        serde_json::from_slice(&bytes)
            .map_err(|err| Error::Malformed(format!("{err} in {}", preview(&bytes))))
    }
}

/// Turn a status and a body into the error that says what to do about it.
///
/// The quota is separated out because it is the only one worth waiting on, and
/// because "you have used your five downloads for today" is a completely
/// different message to a user from "something went wrong".
pub(crate) fn classify(status: u16, body: &str) -> Error {
    let lower = body.to_ascii_lowercase();
    match status {
        401 | 403 => Error::BadKey,
        // 406 is what they answer a spent allowance with, which is an
        // unusual choice and worth pinning down by the body as well.
        406 => Error::QuotaSpent,
        429 if lower.contains("quota") || lower.contains("download") => Error::QuotaSpent,
        _ if lower.contains("quota") => Error::QuotaSpent,
        _ => Error::Http {
            status,
            body: body.to_string(),
        },
    }
}

fn preview(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_without_a_key_refuses_to_exist() {
        // Better than a client that exists and fails every call with a 401,
        // which reads as "the service is broken" rather than "set this up".
        assert!(matches!(OsdbClient::new(""), Err(Error::NoKey)));
    }

    #[test]
    fn a_spent_quota_is_told_apart_from_a_bad_key() {
        assert!(matches!(
            classify(406, "no more downloads"),
            Error::QuotaSpent
        ));
        assert!(matches!(
            classify(429, "Download quota reached"),
            Error::QuotaSpent
        ));
        assert!(matches!(classify(401, "invalid api key"), Error::BadKey));
        assert!(matches!(classify(403, ""), Error::BadKey));
    }

    #[test]
    fn anything_else_keeps_its_status_and_body() {
        match classify(503, "maintenance") {
            Error::Http { status, body } => {
                assert_eq!(status, 503);
                assert_eq!(body, "maintenance");
            }
            other => panic!("expected a plain http error, got {other:?}"),
        }
    }

    #[test]
    fn the_user_agent_names_the_application_and_its_version() {
        // Their terms ask for this, and they refuse generic ones.
        let cfg = ClientConfig::default();
        assert!(
            cfg.user_agent.starts_with("balerion v"),
            "{}",
            cfg.user_agent
        );
        assert!(cfg.user_agent.len() > "balerion v".len());
    }
}
