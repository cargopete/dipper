//! `advancedsearch.php`: page-based search.
//!
//! This is the paginator balerin actually uses. The scrape API in
//! [`crate::search`] is documented as the cursor-based, unlimited-depth
//! option, but as of 2026-08 archive.org echoes the cursor back unchanged and
//! serves page one forever (and rejects its own cursor outright when `sorts`
//! is set). Page-based search works, at the cost of a 10,000 result ceiling.
//!
//! Paging is only stable if the sort is, so we sort by identifier unless told
//! otherwise. With relevance ordering, adjacent pages overlap.

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::client::IaClient;
use crate::error::{Error, Result};
use crate::metadata::{Meta, TORRENT_FORMAT};
use crate::search::SearchHit;

pub const ADVANCED_ENDPOINT: &str = "https://archive.org/advancedsearch.php";

/// archive.org refuses to page past this many results and points you at the
/// scrape API, which does not currently work. Partition the query by date if
/// you need to go deeper.
pub const DEEP_PAGING_LIMIT: usize = 10_000;

pub const MAX_ROWS: u32 = 1_000;

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
    // Tells us whether archive.org has derived a .torrent.
    "format",
];

/// A page-based search of archive.org.
#[derive(Debug, Clone)]
pub struct AdvancedQuery {
    pub q: String,
    pub fields: Vec<String>,
    pub sort: Vec<String>,
    pub rows: u32,
}

impl AdvancedQuery {
    pub fn new(q: impl Into<String>) -> Self {
        Self {
            q: q.into(),
            fields: DEFAULT_FIELDS.iter().map(|s| (*s).to_string()).collect(),
            // A stable sort, so page N+1 does not repeat page N.
            sort: vec!["identifier asc".to_string()],
            rows: 500,
        }
    }

    pub fn fields<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.fields = fields.into_iter().map(Into::into).collect();
        self
    }

    pub fn sort<I, S>(mut self, sort: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.sort = sort.into_iter().map(Into::into).collect();
        self
    }

    pub fn rows(mut self, rows: u32) -> Self {
        self.rows = rows.clamp(1, MAX_ROWS);
        self
    }

    /// Build the request URL. Pages are 1-based.
    pub fn url(&self, page: u32) -> String {
        let mut url = format!(
            "{ADVANCED_ENDPOINT}?q={}&output=json&rows={}&page={}",
            urlencoding::encode(&self.q),
            self.rows.clamp(1, MAX_ROWS),
            page.max(1),
        );
        let mut fields: Vec<&str> = self.fields.iter().map(String::as_str).collect();
        if !fields.contains(&"identifier") {
            fields.insert(0, "identifier");
        }
        for field in fields {
            url.push_str(&format!("&fl[]={}", urlencoding::encode(field)));
        }
        for sort in &self.sort {
            url.push_str(&format!("&sort[]={}", urlencoding::encode(sort)));
        }
        url
    }
}

/// One page of results.
#[derive(Debug, Clone)]
pub struct AdvancedPage {
    pub hits: Vec<SearchHit>,
    pub num_found: u64,
    pub start: u64,
}

#[derive(Debug, Deserialize)]
struct RawResponse {
    #[serde(default)]
    response: Option<RawInner>,
    #[serde(default)]
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct RawInner {
    #[serde(default)]
    #[serde(rename = "numFound")]
    num_found: u64,
    #[serde(default)]
    start: u64,
    #[serde(default)]
    docs: Vec<Value>,
}

impl AdvancedPage {
    /// Parse an advancedsearch response. Errors arrive as a 200 with an
    /// `error` key, so status codes are not enough.
    pub fn parse(url: &str, body: &[u8]) -> Result<Self> {
        let raw: RawResponse = serde_json::from_slice(body).map_err(|source| Error::Json {
            context: url.to_string(),
            source,
        })?;
        if let Some(error) = raw.error {
            let message = match error {
                Value::String(s) => s,
                other => other.to_string(),
            };
            return Err(Error::Search(message));
        }
        let inner = raw
            .response
            .ok_or_else(|| Error::Search("response had no `response` object".into()))?;
        let hits = inner.docs.into_iter().filter_map(hit_from_doc).collect();
        Ok(Self {
            hits,
            num_found: inner.num_found,
            start: inner.start,
        })
    }
}

fn hit_from_doc(doc: Value) -> Option<SearchHit> {
    let mut map: Map<String, Value> = match doc {
        Value::Object(map) => map,
        _ => return None,
    };
    let identifier = map.remove("identifier")?.as_str()?.to_string();
    Some(SearchHit {
        identifier,
        fields: Meta(map),
    })
}

/// Fetch one page (1-based).
pub async fn page(client: &IaClient, query: &AdvancedQuery, page: u32) -> Result<AdvancedPage> {
    let url = query.url(page);
    let body = client.get(&url).await?;
    AdvancedPage::parse(&url, &body)
}

/// How many items match, without fetching them.
pub async fn total(client: &IaClient, query: &AdvancedQuery) -> Result<u64> {
    let probe = query.clone().rows(1).fields(["identifier"]);
    Ok(page(client, &probe, 1).await?.num_found)
}

/// Page through results until `limit` hits are collected, the results run out,
/// or archive.org's deep-paging ceiling is reached.
///
/// `on_page` is called after each page with the hits so far and the total
/// match count, so callers can drive a progress bar.
pub async fn collect<F>(
    client: &IaClient,
    query: &AdvancedQuery,
    limit: usize,
    mut on_page: F,
) -> Result<Vec<SearchHit>>
where
    F: FnMut(&[SearchHit], u64),
{
    let ceiling = limit.min(DEEP_PAGING_LIMIT);
    let mut out: Vec<SearchHit> = Vec::new();
    let mut page_number = 1u32;

    loop {
        let page = page(client, query, page_number).await?;
        if page.hits.is_empty() {
            break;
        }
        out.extend(page.hits);
        on_page(&out, page.num_found);

        if out.len() >= ceiling || out.len() as u64 >= page.num_found {
            break;
        }
        // Never ask for a page that would straddle the deep-paging limit.
        if out.len() + query.rows as usize > DEEP_PAGING_LIMIT {
            tracing::warn!(
                collected = out.len(),
                total = page.num_found,
                "stopping at archive.org's {DEEP_PAGING_LIMIT} result deep-paging limit"
            );
            break;
        }
        page_number += 1;
    }

    out.truncate(ceiling);
    Ok(out)
}

/// Did archive.org derive a `.torrent` for this item?
pub fn has_torrent(hit: &SearchHit) -> bool {
    hit.fields.get_all("format").contains(&TORRENT_FORMAT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_carries_repeated_field_and_sort_params() {
        let url = AdvancedQuery::new("birdsong")
            .fields(["title", "item_size"])
            .sort(["identifier asc"])
            .rows(50)
            .url(3);
        assert!(url.contains("q=birdsong"), "{url}");
        assert!(url.contains("&rows=50&page=3"), "{url}");
        assert!(
            url.contains("&fl%5B%5D=identifier") || url.contains("&fl[]=identifier"),
            "{url}"
        );
        assert!(url.contains("&fl[]=title"), "{url}");
        assert!(url.contains("&sort[]=identifier%20asc"), "{url}");
    }

    #[test]
    fn identifier_is_always_requested() {
        let url = AdvancedQuery::new("x").fields(["title"]).url(1);
        let first = url.find("fl[]=identifier").expect("identifier requested");
        let title = url.find("fl[]=title").unwrap();
        assert!(first < title, "identifier should lead: {url}");
    }

    #[test]
    fn rows_and_pages_are_clamped_to_sane_values() {
        assert_eq!(AdvancedQuery::new("x").rows(0).rows, 1);
        assert_eq!(AdvancedQuery::new("x").rows(99_999).rows, MAX_ROWS);
        assert!(AdvancedQuery::new("x").url(0).contains("&page=1"));
    }

    #[test]
    fn parses_a_response() {
        let body = br#"{
            "responseHeader": {"status": 0},
            "response": {
                "numFound": 1547,
                "start": 0,
                "docs": [
                    {"identifier": "a", "title": "A", "item_size": 10,
                     "format": ["VBR MP3", "Archive BitTorrent"]},
                    {"identifier": "b", "title": "B", "format": "Text PDF"},
                    {"title": "no identifier"}
                ]
            }
        }"#;
        let page = AdvancedPage::parse("test", body).unwrap();
        assert_eq!(page.num_found, 1547);
        assert_eq!(page.hits.len(), 2);
        assert!(has_torrent(&page.hits[0]));
        assert!(!has_torrent(&page.hits[1]));
        assert_eq!(page.hits[0].item_size(), Some(10));
    }

    #[test]
    fn surfaces_the_deep_paging_error() {
        let body = br#"{"error": "[DEEP_PAGING] Requested results would exceed the deep paging limit for this service, 10000 results"}"#;
        let err = AdvancedPage::parse("test", body).unwrap_err();
        assert!(matches!(err, Error::Search(msg) if msg.contains("DEEP_PAGING")));
    }
}
