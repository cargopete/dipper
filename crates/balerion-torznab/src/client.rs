//! Talking to one or more indexers.

use std::time::Duration;

use crate::error::{Error, Result};
use crate::search::{Answer, Query, parse};

const DEFAULT_UA: &str = concat!("balerion/", env!("CARGO_PKG_VERSION"));

/// One indexer, with a name to label its results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Indexer {
    /// What to call it in the interface.
    pub name: String,
    /// Base URL, up to but not including `/api`.
    pub base: String,
}

impl Indexer {
    /// Parse one entry of the configuration.
    ///
    /// Either `name=url` or a bare url, in which case the host stands in for a
    /// name. Naming it matters once there is more than one: with several
    /// configured, knowing which answered is most of what makes a list of
    /// results legible.
    pub fn parse(entry: &str) -> Option<Self> {
        let entry = entry.trim();
        if entry.is_empty() {
            return None;
        }
        // Split on the first `=` that is not part of a URL's query string,
        // which in practice means one before the scheme.
        let (name, base) = match entry.split_once('=') {
            Some((name, base)) if !name.contains("://") && base.contains("://") => (name, base),
            _ => ("", entry),
        };
        let base = base.trim().trim_end_matches('/').to_string();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return None;
        }

        let name = if name.trim().is_empty() {
            host_of(&base)
        } else {
            name.trim().to_string()
        };
        Some(Self { name, base })
    }
}

/// The host part of a URL, for naming an indexer nobody named.
fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}

/// A client for however many indexers are configured.
#[derive(Debug)]
pub struct TorznabClient {
    http: reqwest::Client,
    api_key: String,
    indexers: Vec<Indexer>,
}

impl TorznabClient {
    /// Build from the environment.
    ///
    /// `BALERION_TORZNAB` is a comma-separated list of `name=url` entries or
    /// bare urls; `BALERION_TORZNAB_KEY` is the API key the indexer issued.
    /// Returns `None` when either is missing, because having no indexer is an
    /// ordinary state to be in and not a misconfiguration to complain about.
    pub fn from_env() -> Option<Self> {
        let list = std::env::var("BALERION_TORZNAB").ok()?;
        let key = std::env::var("BALERION_TORZNAB_KEY").unwrap_or_default();
        let indexers: Vec<Indexer> = list.split(',').filter_map(Indexer::parse).collect();
        if indexers.is_empty() {
            tracing::warn!(
                value = list,
                "BALERION_TORZNAB is set but names no usable indexer URL"
            );
            return None;
        }
        Self::new(indexers, &key).ok()
    }

    pub fn new(indexers: Vec<Indexer>, api_key: &str) -> Result<Self> {
        if indexers.is_empty() {
            return Err(Error::NotConfigured);
        }
        let http = reqwest::Client::builder()
            .user_agent(DEFAULT_UA)
            // Short: an indexer is on the local network or the local machine,
            // and a search nobody is waiting on is worthless.
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            http,
            api_key: api_key.to_string(),
            indexers,
        })
    }

    pub fn indexers(&self) -> &[Indexer] {
        &self.indexers
    }

    /// Ask every configured indexer at once and take the union.
    ///
    /// Concurrent because they are independent, and one that is slow or down
    /// must not hold up the ones that are neither. An indexer that fails is
    /// logged and skipped rather than failing the search: with three
    /// configured, two good answers beat one error.
    pub async fn search(&self, query: &Query) -> Answer {
        let asks = self.indexers.iter().map(|indexer| async move {
            match self.ask(indexer, query).await {
                Ok(answer) => Some(answer),
                Err(err) => {
                    tracing::warn!(indexer = indexer.name, %err, "indexer did not answer");
                    None
                }
            }
        });

        let mut combined = Answer::default();
        for answer in futures_util::future::join_all(asks)
            .await
            .into_iter()
            .flatten()
        {
            combined.hits.extend(answer.hits);
            combined.without_magnet += answer.without_magnet;
        }
        combined
    }

    /// Ask one indexer.
    pub async fn ask(&self, indexer: &Indexer, query: &Query) -> Result<Answer> {
        let url = query.url(&indexer.base, &self.api_key);
        let response = self.http.get(&url).send().await?;
        let status = response.status();
        let body = response.bytes().await?;

        if !status.is_success() {
            return Err(match status.as_u16() {
                401 | 403 => Error::BadKey,
                other => Error::Http { status: other },
            });
        }
        parse(&body, &indexer.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_indexer_keeps_its_name() {
        let indexer = Indexer::parse("prowlarr=http://box.local:9696").unwrap();
        assert_eq!(indexer.name, "prowlarr");
        assert_eq!(indexer.base, "http://box.local:9696");
    }

    #[test]
    fn an_unnamed_indexer_is_named_after_its_host() {
        // With several configured, knowing which one answered is most of what
        // makes the list legible.
        let indexer = Indexer::parse("https://indexer.example:9117/torznab/all/").unwrap();
        assert_eq!(indexer.name, "indexer.example:9117");
        assert_eq!(
            indexer.base, "https://indexer.example:9117/torznab/all",
            "the trailing slash is dropped so the URL builder does not double it"
        );
    }

    #[test]
    fn something_that_is_not_a_url_is_not_an_indexer() {
        assert!(Indexer::parse("").is_none());
        assert!(Indexer::parse("   ").is_none());
        assert!(Indexer::parse("box.local:9696").is_none(), "no scheme");
        assert!(Indexer::parse("name=box.local").is_none());
    }

    #[test]
    fn a_url_with_an_equals_in_its_query_is_not_split_on_it() {
        // The classic way to mangle a configuration line.
        let indexer = Indexer::parse("http://box.local/torznab?apikey=abc").unwrap();
        assert_eq!(indexer.base, "http://box.local/torznab?apikey=abc");
        assert_eq!(indexer.name, "box.local");
    }

    #[test]
    fn a_client_with_no_indexers_refuses_to_exist() {
        // Better than one that exists and fails every search, which reads as
        // "the feature is broken" rather than "set this up".
        assert!(matches!(
            TorznabClient::new(Vec::new(), "K"),
            Err(Error::NotConfigured)
        ));
    }

    #[test]
    fn several_indexers_are_parsed_from_one_line() {
        let indexers: Vec<Indexer> = "a=http://one.local,b=http://two.local, http://three.local"
            .split(',')
            .filter_map(Indexer::parse)
            .collect();
        assert_eq!(indexers.len(), 3);
        assert_eq!(indexers[0].name, "a");
        assert_eq!(indexers[2].name, "three.local");
    }
}
