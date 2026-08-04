//! The archive.org scrape API: cursor-paginated search that, on paper, has no
//! deep-paging ceiling.
//!
//! In practice, as of 2026-08, anonymous cursor pagination does not work: the
//! server echoes the same cursor back and re-serves page one indefinitely, and
//! returns `400 Bad cursor` for its own cursor when `sorts` is set. So this
//! module is good for a first page and for cheap `total_only` counts, while
//! [`crate::advanced`] does the actual paging. [`scrape_all`] therefore stops
//! rather than looping when it sees a cursor that has not moved.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::IaClient;
use crate::error::{Error, Result};
use crate::metadata::{IA_BASE, Meta, TORRENT_FORMAT};

pub const SCRAPE_ENDPOINT: &str = "https://archive.org/services/search/v1/scrape";

/// The scrape API rejects anything below this.
pub const MIN_COUNT: u32 = 100;
pub const MAX_COUNT: u32 = 10_000;

const DEFAULT_FIELDS: &[&str] = &[
    "identifier",
    "title",
    "creator",
    "description",
    "subject",
    "collection",
    "mediatype",
    "downloads",
    "item_size",
    "publicdate",
];

/// A scrape query. `q` uses Lucene-ish syntax, same as the website's search box.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub q: String,
    pub fields: Vec<String>,
    pub sorts: Vec<String>,
    pub count: u32,
}

impl SearchQuery {
    pub fn new(q: impl Into<String>) -> Self {
        Self {
            q: q.into(),
            fields: DEFAULT_FIELDS.iter().map(|s| (*s).to_string()).collect(),
            sorts: Vec::new(),
            count: MIN_COUNT,
        }
    }

    /// Restrict to items archive.org has derived a `.torrent` for.
    pub fn torrents_only(mut self) -> Self {
        self.q = format!("({}) AND format:\"{TORRENT_FORMAT}\"", self.q);
        self
    }

    /// Restrict to one mediatype (`texts`, `audio`, `movies`, `software`, ...).
    pub fn mediatype(mut self, mediatype: &str) -> Self {
        self.q = format!("({}) AND mediatype:{mediatype}", self.q);
        self
    }

    pub fn collection(mut self, collection: &str) -> Self {
        self.q = format!("({}) AND collection:{collection}", self.q);
        self
    }

    pub fn fields<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.fields = fields.into_iter().map(Into::into).collect();
        self
    }

    pub fn sorts<I, S>(mut self, sorts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.sorts = sorts.into_iter().map(Into::into).collect();
        self
    }

    pub fn count(mut self, count: u32) -> Self {
        self.count = count.clamp(MIN_COUNT, MAX_COUNT);
        self
    }

    /// Build the request URL. `identifier` is always requested (we key on it)
    /// and, if used as a sort, is moved last as the API demands.
    pub fn url(&self, cursor: Option<&str>) -> String {
        let mut fields: Vec<&str> = self.fields.iter().map(String::as_str).collect();
        if !fields.contains(&"identifier") {
            fields.insert(0, "identifier");
        }

        let mut sorts: Vec<&str> = self.sorts.iter().map(String::as_str).collect();
        if let Some(pos) = sorts.iter().position(|s| s.starts_with("identifier")) {
            let id_sort = sorts.remove(pos);
            sorts.push(id_sort);
        }

        let mut url = format!(
            "{SCRAPE_ENDPOINT}?q={}&fields={}&count={}",
            urlencoding::encode(&self.q),
            urlencoding::encode(&fields.join(",")),
            self.count.clamp(MIN_COUNT, MAX_COUNT),
        );
        if !sorts.is_empty() {
            url.push_str(&format!("&sorts={}", urlencoding::encode(&sorts.join(","))));
        }
        if let Some(cursor) = cursor {
            url.push_str(&format!("&cursor={}", urlencoding::encode(cursor)));
        }
        url
    }

    /// URL for a count-only query, which is cheap and does not consume a cursor.
    pub fn total_only_url(&self) -> String {
        format!(
            "{SCRAPE_ENDPOINT}?q={}&total_only=true&count={MIN_COUNT}",
            urlencoding::encode(&self.q)
        )
    }
}

/// One search result: the identifier plus whatever fields were requested.
#[derive(Debug, Clone, Serialize)]
pub struct SearchHit {
    pub identifier: String,
    pub fields: Meta,
}

impl SearchHit {
    pub fn title(&self) -> Option<&str> {
        self.fields.title()
    }

    pub fn mediatype(&self) -> Option<&str> {
        self.fields.mediatype()
    }

    pub fn item_size(&self) -> Option<u64> {
        num_field(&self.fields, "item_size")
    }

    pub fn downloads(&self) -> Option<u64> {
        num_field(&self.fields, "downloads")
    }

    pub fn details_url(&self) -> String {
        format!(
            "{IA_BASE}/details/{}",
            urlencoding::encode(&self.identifier)
        )
    }

    /// The derived torrent lives at a predictable URL, so we can offer one
    /// without a second metadata round trip. Fetch the metadata if you need
    /// to be sure it exists.
    pub fn torrent_url(&self) -> String {
        let id = urlencoding::encode(&self.identifier);
        format!("{IA_BASE}/download/{id}/{id}_archive.torrent")
    }
}

fn num_field(meta: &Meta, key: &str) -> Option<u64> {
    match meta.0.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ScrapeResponse {
    #[serde(default)]
    items: Vec<Value>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    error: Option<String>,
}

/// Result of one scrape page.
#[derive(Debug, Clone)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub cursor: Option<String>,
    pub total: Option<u64>,
}

impl SearchPage {
    /// Parse a scrape API response body. `url` is only used for error context.
    pub fn parse(url: &str, body: &[u8]) -> Result<Self> {
        let resp: ScrapeResponse = serde_json::from_slice(body).map_err(|source| Error::Json {
            context: url.to_string(),
            source,
        })?;
        if let Some(error) = resp.error {
            return Err(Error::Search(format!("scrape API error: {error}")));
        }
        let hits = resp
            .items
            .into_iter()
            .filter_map(|item| {
                let mut map = match item {
                    Value::Object(map) => map,
                    _ => return None,
                };
                let identifier = map.remove("identifier")?.as_str()?.to_string();
                Some(SearchHit {
                    identifier,
                    fields: Meta(map),
                })
            })
            .collect();
        Ok(Self {
            hits,
            cursor: resp.cursor,
            total: resp.total,
        })
    }
}

/// Fetch a single page of results, continuing from `cursor` if given.
pub async fn scrape_page(
    client: &IaClient,
    query: &SearchQuery,
    cursor: Option<&str>,
) -> Result<SearchPage> {
    let url = query.url(cursor);
    let body = client.get(&url).await?;
    SearchPage::parse(&url, &body)
}

/// Follow cursors until `limit` hits are collected or the results run out.
///
/// Stops if archive.org hands back a cursor identical to the one we sent,
/// which is its current way of saying "no more pages for you" while looking
/// like it means the opposite.
pub async fn scrape_all(
    client: &IaClient,
    query: &SearchQuery,
    limit: usize,
) -> Result<Vec<SearchHit>> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = scrape_page(client, query, cursor.as_deref()).await?;
        let empty = page.hits.is_empty();
        out.extend(page.hits);
        if out.len() >= limit {
            out.truncate(limit);
            return Ok(out);
        }
        match page.cursor {
            Some(next) if !empty && Some(&next) != cursor.as_ref() => cursor = Some(next),
            Some(_) => {
                tracing::warn!(
                    collected = out.len(),
                    "archive.org returned an unchanged scrape cursor; stopping"
                );
                return Ok(out);
            }
            None => return Ok(out),
        }
    }
}

/// How many items match, without paging through them.
pub async fn total(client: &IaClient, query: &SearchQuery) -> Result<u64> {
    let url = query.total_only_url();
    let body = client.get(&url).await?;
    let page = SearchPage::parse(&url, &body)?;
    Ok(page.total.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_always_requests_identifier() {
        let url = SearchQuery::new("kittens").fields(["title"]).url(None);
        assert!(url.contains("fields=identifier%2Ctitle"), "{url}");
    }

    #[test]
    fn identifier_sort_is_moved_last() {
        let url = SearchQuery::new("kittens")
            .sorts(["identifier asc", "downloads desc"])
            .url(None);
        assert!(
            url.contains("sorts=downloads%20desc%2Cidentifier%20asc"),
            "{url}"
        );
    }

    #[test]
    fn count_is_clamped_to_the_api_minimum() {
        let q = SearchQuery::new("kittens").count(5);
        assert_eq!(q.count, MIN_COUNT);
        assert_eq!(SearchQuery::new("kittens").count(50_000).count, MAX_COUNT);
    }

    #[test]
    fn filters_compose_into_the_query() {
        let q = SearchQuery::new("scifi").mediatype("audio").torrents_only();
        assert_eq!(
            q.q,
            "((scifi) AND mediatype:audio) AND format:\"Archive BitTorrent\""
        );
    }

    #[test]
    fn cursor_is_carried_into_the_url() {
        let url = SearchQuery::new("kittens").url(Some("abc/def=="));
        assert!(url.contains("cursor=abc%2Fdef%3D%3D"), "{url}");
    }

    #[test]
    fn parses_a_page_and_keeps_unknown_fields() {
        let body = br#"{
            "items": [
                {"identifier": "xfetch", "title": "X-Fetch", "downloads": 42,
                 "item_size": "10682344", "weird_new_field": "hello"},
                {"no_identifier": true}
            ],
            "count": 1,
            "cursor": "next-please",
            "total": 1234
        }"#;
        let page = SearchPage::parse("test", body).unwrap();
        assert_eq!(page.hits.len(), 1, "hits without an identifier are dropped");
        assert_eq!(page.total, Some(1234));
        assert_eq!(page.cursor.as_deref(), Some("next-please"));

        let hit = &page.hits[0];
        assert_eq!(hit.identifier, "xfetch");
        assert_eq!(hit.title(), Some("X-Fetch"));
        assert_eq!(hit.downloads(), Some(42));
        assert_eq!(hit.item_size(), Some(10_682_344));
        assert_eq!(hit.fields.get_str("weird_new_field"), Some("hello"));
        assert_eq!(
            hit.torrent_url(),
            "https://archive.org/download/xfetch/xfetch_archive.torrent"
        );
    }

    #[test]
    fn surfaces_scrape_api_errors() {
        let body = br#"{"error": "invalid cursor"}"#;
        assert!(SearchPage::parse("test", body).is_err());
    }
}
