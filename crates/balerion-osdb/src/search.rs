//! Asking for subtitles, and fetching one.
//!
//! Two ways to ask, and they are worth a great deal more or less than each
//! other. A **hash match** means a file somebody timed against this exact
//! release, so it needs no correction at all. A **title match** means a file
//! timed against *some* release of the same programme, which is where every
//! offset and framerate complaint comes from.
//!
//! So the hash is tried first, and a title match is reported as such rather
//! than quietly presented as equivalent. The player treats the two differently:
//! one is trusted, the other is checked against the audio before it is shown.

use serde::Deserialize;

use crate::client::{API_BASE, OsdbClient};
use crate::error::{Error, Result};

/// What to look for.
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// The OpenSubtitles file hash, when we can compute one.
    pub moviehash: Option<String>,
    /// Title to fall back on when the hash finds nothing.
    pub query: Option<String>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// Two-letter language codes, most wanted first.
    pub languages: Vec<String>,
}

impl Query {
    /// Subtitles for one exact file.
    pub fn for_hash(moviehash: impl Into<String>) -> Self {
        Self {
            moviehash: Some(moviehash.into()),
            languages: vec!["en".to_string()],
            ..Default::default()
        }
    }

    /// Subtitles for a programme by name.
    pub fn for_title(title: impl Into<String>) -> Self {
        Self {
            query: Some(title.into()),
            languages: vec!["en".to_string()],
            ..Default::default()
        }
    }

    pub fn episode(mut self, season: u32, episode: u32) -> Self {
        self.season = Some(season);
        self.episode = Some(episode);
        self
    }

    pub fn languages(mut self, languages: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.languages = languages.into_iter().map(Into::into).collect();
        self
    }

    /// Build the search URL.
    ///
    /// Kept separate from the request so it can be tested without an account,
    /// which matters here rather more than usual: a wrong parameter returns a
    /// perfectly valid list of subtitles for the wrong film.
    pub fn url(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(hash) = &self.moviehash {
            parts.push(format!("moviehash={}", urlencoding::encode(hash)));
        }
        if let Some(query) = &self.query {
            parts.push(format!("query={}", urlencoding::encode(query)));
        }
        if let Some(season) = self.season {
            parts.push(format!("season_number={season}"));
        }
        if let Some(episode) = self.episode {
            parts.push(format!("episode_number={episode}"));
        }
        if !self.languages.is_empty() {
            parts.push(format!(
                "languages={}",
                urlencoding::encode(&self.languages.join(","))
            ));
        }
        // Sorted so a URL is stable for a given query, which makes it cacheable
        // and makes a test of it mean something.
        parts.sort();
        format!("{API_BASE}/subtitles?{}", parts.join("&"))
    }
}

/// One subtitle file on offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// What a download request needs.
    pub file_id: i64,
    pub language: Option<String>,
    /// Their name for the release these were timed against, which is the most
    /// useful thing to show a viewer choosing between two.
    pub release: Option<String>,
    /// How many people have downloaded it. A crude proxy for "is this the good
    /// one", and better than the order the API happens to return.
    pub downloads: u64,
    /// True when this came back from a hash match, and so was timed against
    /// exactly the file being played.
    pub exact: bool,
    /// Whether the uploader claims these were machine translated. Worth
    /// knowing: they are frequently much worse, and never worth preferring.
    pub machine_translated: bool,
}

#[derive(Debug, Deserialize)]
struct RawResponse {
    #[serde(default)]
    data: Vec<RawItem>,
}

#[derive(Debug, Deserialize)]
struct RawItem {
    #[serde(default)]
    attributes: RawAttributes,
}

#[derive(Debug, Default, Deserialize)]
struct RawAttributes {
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    release: Option<String>,
    #[serde(default)]
    download_count: Option<i64>,
    #[serde(default)]
    moviehash_match: Option<bool>,
    #[serde(default)]
    machine_translated: Option<serde_json::Value>,
    #[serde(default)]
    files: Vec<RawFile>,
}

#[derive(Debug, Deserialize)]
struct RawFile {
    #[serde(default)]
    file_id: Option<i64>,
}

/// `machine_translated` comes back as a bool, as 0/1, or as a string,
/// depending on the record. Treated as false unless it plainly says otherwise.
fn truthy(value: &Option<serde_json::Value>) -> bool {
    match value {
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(serde_json::Value::Number(number)) => number.as_i64().unwrap_or(0) != 0,
        Some(serde_json::Value::String(text)) => {
            matches!(text.as_str(), "1" | "true" | "yes")
        }
        _ => false,
    }
}

/// Parse a search response.
///
/// Separated from the request for the same reason as [`Query::url`]: this is
/// where a change at their end turns into subtitles for the wrong thing, and it
/// should be testable against a recorded body rather than against the internet.
pub fn parse(body: &[u8]) -> Result<Vec<Match>> {
    let raw: RawResponse = serde_json::from_slice(body)
        .map_err(|err| Error::Malformed(format!("search response: {err}")))?;

    let mut found: Vec<Match> = raw
        .data
        .into_iter()
        .filter_map(|item| {
            let attributes = item.attributes;
            // A record with no file has nothing to download and is not a
            // result, whatever else it says.
            let file_id = attributes.files.first().and_then(|file| file.file_id)?;
            Some(Match {
                file_id,
                language: attributes.language,
                release: attributes.release,
                downloads: attributes.download_count.unwrap_or(0).max(0) as u64,
                exact: attributes.moviehash_match.unwrap_or(false),
                machine_translated: truthy(&attributes.machine_translated),
            })
        })
        .collect();

    // Best first: timed against this exact file, then written by a person, then
    // whatever most people chose.
    found.sort_by(|a, b| {
        b.exact
            .cmp(&a.exact)
            .then(a.machine_translated.cmp(&b.machine_translated))
            .then(b.downloads.cmp(&a.downloads))
    });
    Ok(found)
}

/// Search for subtitles.
pub async fn search(client: &OsdbClient, query: &Query) -> Result<Vec<Match>> {
    let raw: serde_json::Value = client.get_json(&query.url()).await?;
    parse(&serde_json::to_vec(&raw).map_err(|err| Error::Malformed(err.to_string()))?)
}

/// Turn a match into the bytes of a subtitle file.
///
/// **This is the call that spends the daily allowance**, which is why it is a
/// separate step from searching rather than folded into it: searching is cheap
/// and can be done freely, and only the file actually wanted is fetched.
pub async fn download(client: &OsdbClient, file_id: i64) -> Result<Vec<u8>> {
    let response: serde_json::Value = client
        .post_json(
            &format!("{API_BASE}/download"),
            serde_json::json!({ "file_id": file_id }),
        )
        .await?;

    let link = response
        .get("link")
        .and_then(|link| link.as_str())
        .ok_or_else(|| Error::Malformed(format!("no download link in {response}")))?;

    client.fetch(link).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hash_query_asks_for_the_hash_and_a_language() {
        let url = Query::for_hash("8e245d9679d31e12").url();
        assert!(url.starts_with(API_BASE), "{url}");
        assert!(url.contains("moviehash=8e245d9679d31e12"), "{url}");
        assert!(url.contains("languages=en"), "{url}");
        assert!(
            !url.contains("query="),
            "a hash search names no title: {url}"
        );
    }

    #[test]
    fn an_episode_query_carries_its_numbers() {
        let url = Query::for_title("The Computer Chronicles")
            .episode(3, 7)
            .url();
        assert!(url.contains("season_number=3"), "{url}");
        assert!(url.contains("episode_number=7"), "{url}");
        assert!(url.contains("query=The%20Computer%20Chronicles"), "{url}");
    }

    #[test]
    fn several_languages_are_comma_separated() {
        let url = Query::for_hash("abc").languages(["en", "fr"]).url();
        assert!(url.contains("languages=en%2Cfr"), "{url}");
    }

    #[test]
    fn the_same_query_always_builds_the_same_url() {
        // Otherwise nothing downstream can cache by it.
        let query = Query::for_title("Nosferatu")
            .episode(1, 2)
            .languages(["en"]);
        assert_eq!(query.url(), query.url());
    }

    fn body(items: &str) -> Vec<u8> {
        format!(r#"{{"data": [{items}]}}"#).into_bytes()
    }

    fn item(file_id: i64, downloads: i64, exact: bool, machine: &str) -> String {
        format!(
            r#"{{"attributes": {{"language": "en", "release": "r{file_id}",
                "download_count": {downloads}, "moviehash_match": {exact},
                "machine_translated": {machine},
                "files": [{{"file_id": {file_id}}}]}}}}"#
        )
    }

    #[test]
    fn a_search_response_becomes_matches() {
        let found = parse(&body(&item(42, 900, true, "false"))).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].file_id, 42);
        assert_eq!(found[0].downloads, 900);
        assert!(found[0].exact);
        assert!(!found[0].machine_translated);
        assert_eq!(found[0].release.as_deref(), Some("r42"));
    }

    #[test]
    fn an_exact_match_beats_a_more_popular_inexact_one() {
        // The whole reason the hash is worth computing: a file timed against
        // this release needs no correcting, however obscure it is.
        let items = format!(
            "{}, {}",
            item(1, 100_000, false, "false"),
            item(2, 5, true, "false")
        );
        let found = parse(&body(&items)).unwrap();
        assert_eq!(found[0].file_id, 2, "the exact match must come first");
    }

    #[test]
    fn a_human_translation_beats_a_machine_one_of_equal_standing() {
        let items = format!(
            "{}, {}",
            item(1, 100, false, "true"),
            item(2, 100, false, "false")
        );
        let found = parse(&body(&items)).unwrap();
        assert_eq!(found[0].file_id, 2);
        assert!(found[1].machine_translated);
    }

    #[test]
    fn machine_translated_is_read_however_they_spell_it() {
        // Seen as a bool, as 0/1, and as a string, in the same API.
        for spelling in ["true", "1", "\"1\"", "\"true\""] {
            let found = parse(&body(&item(1, 1, false, spelling))).unwrap();
            assert!(found[0].machine_translated, "{spelling}");
        }
        for spelling in ["false", "0", "null", "\"0\""] {
            let found = parse(&body(&item(1, 1, false, spelling))).unwrap();
            assert!(!found[0].machine_translated, "{spelling}");
        }
    }

    #[test]
    fn a_record_with_no_file_is_not_a_result() {
        // There is nothing to download, so offering it would produce a choice
        // that fails when taken.
        let empty = r#"{"attributes": {"language": "en", "files": []}}"#;
        assert!(parse(&body(empty)).unwrap().is_empty());
    }

    #[test]
    fn an_empty_answer_is_not_an_error() {
        assert!(parse(br#"{"data": []}"#).unwrap().is_empty());
        assert!(parse(b"{}").unwrap().is_empty());
    }

    #[test]
    fn rubbish_is_refused_rather_than_read_as_nothing() {
        // "No subtitles exist" and "the API changed under us" must not look
        // the same, or a breakage presents for months as a quiet absence.
        assert!(parse(b"not json").is_err());
        assert!(parse(b"").is_err());
    }
}
