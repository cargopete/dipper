use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use crate::error::{Error, Result};

/// archive.org is generous with data and stingy with request rates. Community
/// experience is that anything much faster than ~3 requests/second starts
/// collecting 429s, so we pace ourselves by default.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_millis(350);

const DEFAULT_UA: &str = concat!(
    "balerion/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/cargopete/balerion)"
);

#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Identify yourself; archive.org's bot policy asks for it.
    pub user_agent: String,
    /// Minimum spacing between requests issued by this client.
    pub min_interval: Duration,
    /// How many times to retry a 429/5xx before giving up.
    pub max_retries: u32,
    /// Ask archive.org to serve us at reduced priority. Costs us latency,
    /// buys us goodwill and fewer 429s.
    pub reduced_priority: bool,
    /// Optional IA-S3 keypair (access, secret). Reads do not need it, but
    /// sending it can raise our priority.
    pub s3_keys: Option<(String, String)>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            user_agent: DEFAULT_UA.to_string(),
            min_interval: DEFAULT_MIN_INTERVAL,
            max_retries: 5,
            reduced_priority: false,
            s3_keys: None,
        }
    }
}

/// A polite HTTP client for archive.org.
///
/// All requests funnel through [`IaClient::get`], which enforces the
/// configured minimum spacing and retries 429/5xx with jittered exponential
/// backoff, honouring `Retry-After` when the server bothers to send one.
#[derive(Debug)]
pub struct IaClient {
    http: reqwest::Client,
    cfg: ClientConfig,
    last_request: Mutex<Option<Instant>>,
}

impl IaClient {
    pub fn new() -> Result<Self> {
        Self::with_config(ClientConfig::default())
    }

    pub fn with_config(cfg: ClientConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(cfg.user_agent.clone())
            .timeout(Duration::from_secs(60))
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

    pub fn http(&self) -> &reqwest::Client {
        &self.http
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

    fn decorate(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut req = req.header("Accept-Encoding", "gzip, deflate");
        if self.cfg.reduced_priority {
            req = req.header("X-Accept-Reduced-Priority", "1");
        }
        if let Some((access, secret)) = &self.cfg.s3_keys {
            req = req.header("Authorization", format!("LOW {access}:{secret}"));
        }
        req
    }

    /// GET a URL, paced and retried. Returns the response body as bytes.
    pub async fn get(&self, url: &str) -> Result<Vec<u8>> {
        let mut attempt = 0u32;
        loop {
            self.throttle().await;
            let resp = self.decorate(self.http.get(url)).send().await;

            let wait = match resp {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        return Ok(resp.bytes().await?.to_vec());
                    }
                    if !retryable(status) {
                        return Err(Error::Status {
                            status,
                            url: url.to_string(),
                        });
                    }
                    retry_after(&resp).unwrap_or_else(|| backoff(attempt))
                }
                Err(err) if err.is_timeout() || err.is_connect() => backoff(attempt),
                Err(err) => return Err(err.into()),
            };

            if attempt >= self.cfg.max_retries {
                return Err(Error::RateLimited {
                    attempts: attempt + 1,
                    wait,
                });
            }
            tracing::debug!(url, attempt, ?wait, "archive.org pushed back, retrying");
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }

    /// GET a URL and parse the body as JSON.
    pub async fn get_json<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let body = self.get(url).await?;
        serde_json::from_slice(&body).map_err(|source| Error::Json {
            context: url.to_string(),
            source,
        })
    }
}

fn retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let secs: u64 = resp
        .headers()
        .get("Retry-After")?
        .to_str()
        .ok()?
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs.min(120)))
}

/// Exponential backoff, capped at 30s, with up to 250ms of jitter so a fleet
/// of balerions does not stampede in lockstep.
fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_millis(500 * 2u64.saturating_pow(attempt.min(6)));
    base.min(Duration::from_secs(30)) + jitter()
}

fn jitter() -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    Duration::from_millis(u64::from(nanos % 250))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_is_capped() {
        let first = backoff(0);
        let later = backoff(4);
        assert!(first < Duration::from_secs(1));
        assert!(later >= Duration::from_secs(8));
        assert!(backoff(20) <= Duration::from_secs(31));
    }

    #[test]
    fn only_429_and_5xx_are_retried() {
        assert!(retryable(StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!retryable(StatusCode::NOT_FOUND));
        assert!(!retryable(StatusCode::FORBIDDEN));
    }

    #[tokio::test]
    async fn throttle_paces_requests() {
        let client = IaClient::with_config(ClientConfig {
            min_interval: Duration::from_millis(120),
            ..Default::default()
        })
        .unwrap();
        let start = Instant::now();
        client.throttle().await;
        client.throttle().await;
        assert!(start.elapsed() >= Duration::from_millis(110));
    }
}
