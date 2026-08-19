use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use crate::error::{Error, Result};

/// apibay publishes no rate limit, which is not the same as having none. A
/// quarter of a second between requests costs a human typist nothing and stops
/// a leant-on Enter key from hammering someone else's server.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(250);

const DEFAULT_UA: &str = concat!(
    "balerion/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/cargopete/balerion)"
);

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub user_agent: String,
    /// Minimum spacing between requests issued by this client.
    pub min_interval: Duration,
    /// A search nobody is waiting on is worthless, so this is short. The
    /// endpoint is either up and quick or it is unreachable.
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

/// An HTTP client for apibay, the JSON endpoint behind thepiratebay's
/// frontend. The site's page is a shell; this is the request its own
/// JavaScript makes.
#[derive(Debug)]
pub struct TpbClient {
    http: reqwest::Client,
    cfg: ClientConfig,
    last_request: Mutex<Option<Instant>>,
}

impl TpbClient {
    pub fn new() -> Result<Self> {
        Self::with_config(ClientConfig::default())
    }

    pub fn with_config(cfg: ClientConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            // A client with no timeout waits forever and calls it success.
            .timeout(cfg.timeout)
            .build()?;
        Ok(Self {
            http,
            cfg,
            last_request: Mutex::new(None),
        })
    }

    pub fn config(&self) -> &ClientConfig {
        &self.cfg
    }

    /// Block until enough time has passed since the previous request.
    async fn throttle(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < self.cfg.min_interval {
                tokio::time::sleep(self.cfg.min_interval - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    /// GET a URL, paced, and parse the body as JSON.
    ///
    /// No retries, unlike [`balerion_ia`]: a failed search is a line of text in
    /// the page and a human who will press the button again, not a download
    /// that has to survive a rough patch unattended.
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        self.throttle().await;
        let response = self.http.get(url).send().await?;

        let status = response.status();
        if !status.is_success() {
            return Err(Error::Status {
                status,
                url: url.to_string(),
            });
        }

        let body = response.text().await?;
        serde_json::from_str(&body).map_err(|source| Error::Json {
            context: url.to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn throttle_paces_requests() {
        let client = TpbClient::with_config(ClientConfig {
            min_interval: Duration::from_millis(120),
            ..Default::default()
        })
        .unwrap();
        let start = Instant::now();
        client.throttle().await;
        client.throttle().await;
        assert!(start.elapsed() >= Duration::from_millis(110));
    }

    #[test]
    fn the_client_identifies_itself_and_has_a_timeout() {
        let cfg = ClientConfig::default();
        assert!(cfg.user_agent.starts_with("balerion/"));
        assert!(cfg.timeout > Duration::ZERO);
    }
}
