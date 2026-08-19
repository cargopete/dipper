//! The search request, and the shape its answers arrive in.

use serde::{Deserialize, Serialize};

use crate::category;
use crate::client::TpbClient;
use crate::error::{Error, Result};
use crate::magnet;

pub const API_URL: &str = "https://apibay.org/q.php";

/// The sentinel row the API returns instead of an empty array.
const NO_RESULTS_ID: &str = "0";

/// The wire format, exactly as it arrives.
///
/// Every field is a string, the numbers included, so deserialising straight
/// into `u64` fails on the first row. This type exists only to be converted
/// into something honest.
#[derive(Debug, Deserialize)]
struct RawTorrent {
    id: String,
    name: String,
    #[serde(default)]
    info_hash: String,
    #[serde(default)]
    seeders: String,
    #[serde(default)]
    leechers: String,
    #[serde(default)]
    num_files: String,
    #[serde(default)]
    size: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    added: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    category: String,
}

/// One result, in the shape the rest of balerion wants it.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: u64,
    pub name: String,
    pub info_hash: String,
    pub seeders: u64,
    pub leechers: u64,
    pub num_files: u64,
    pub size_bytes: u64,
    /// Who uploaded it, and whether the site vouches for them. Not a rights
    /// statement, and not a quality guarantee, but a `vip` upload from a known
    /// name is less often a disappointment than an anonymous one.
    pub username: String,
    pub status: String,
    /// Unix epoch seconds. Left as a number on purpose: the browser has `Date`
    /// and knows the viewer's timezone, and this crate would need a date
    /// library to do a worse job of it.
    pub added: i64,
    pub category: u32,
    pub category_label: &'static str,
    /// Assembled locally from the infohash. A hit balerion cannot open is not a
    /// hit, so this is not optional: rows without a usable hash are dropped
    /// during parsing rather than shown and then failing on click.
    pub magnet: String,
}

impl TryFrom<RawTorrent> for Hit {
    type Error = Error;

    fn try_from(raw: RawTorrent) -> Result<Self> {
        let unusable = |why: &str| Error::Unusable {
            id: raw.id.clone(),
            why: why.to_string(),
        };

        let info_hash = raw.info_hash.to_uppercase();
        // apibay serves names HTML-escaped, so "español" arrives as
        // "espa&ntilde;ol". Decoded here, once, before anything else sees it:
        // the name goes on the page as text and into the magnet's display
        // name, and both would otherwise carry the entity through verbatim.
        let name = html_escape::decode_html_entities(&raw.name).into_owned();
        let magnet = magnet::uri(&info_hash, &name)
            .ok_or_else(|| unusable("no usable infohash, so no magnet"))?;
        let category: u32 = raw.category.parse().unwrap_or(0);

        Ok(Hit {
            id: raw.id.parse().map_err(|_| unusable("id is not a number"))?,
            size_bytes: raw
                .size
                .parse()
                .map_err(|_| unusable("size is not a number"))?,
            magnet,
            info_hash,
            name,
            // These are decoration. A missing seeder count is worth showing as
            // zero; it is not worth throwing away an otherwise good result.
            seeders: raw.seeders.parse().unwrap_or(0),
            leechers: raw.leechers.parse().unwrap_or(0),
            num_files: raw.num_files.parse().unwrap_or(0),
            username: raw.username,
            status: raw.status,
            added: raw.added.parse().unwrap_or(0),
            category,
            category_label: category::label(category),
        })
    }
}

/// Search apibay for `terms` within a category code.
///
/// The code should come from [`category::CATEGORIES`]. Zero is legal to the
/// API and searches everything, adult categories included, which is why
/// nothing in balerion offers it.
pub async fn search(client: &TpbClient, terms: &str, code: u32) -> Result<Vec<Hit>> {
    let url = format!(
        "{API_URL}?q={}&cat={code}",
        urlencoding::encode(terms.trim())
    );
    let raw: Vec<RawTorrent> = client.get_json(&url).await?;

    // The API never returns `[]`. An empty search comes back as a single
    // placeholder row with id "0", which parses perfectly and renders as an
    // entirely convincing fake result if nobody checks for it.
    if raw.len() == 1 && raw[0].id == NO_RESULTS_ID {
        return Ok(Vec::new());
    }

    let mut hits = Vec::with_capacity(raw.len());
    for row in raw {
        match Hit::try_from(row) {
            Ok(hit) => hits.push(hit),
            // One bad row is not a failed search. Logged rather than swallowed
            // so a change in the wire format shows up as a pile of warnings
            // instead of a search that quietly returns less than it used to.
            Err(err) => tracing::warn!(%err, "skipping an apibay row"),
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(json: &str) -> Vec<RawTorrent> {
        serde_json::from_str(json).expect("the fixture should parse")
    }

    /// One real row, trimmed to the fields that matter.
    const ROW: &str = r#"[{
        "id": "18461548",
        "name": "Some.Film.1080p.BluRay.x265",
        "info_hash": "31a5ea99284b3603e94ef861311b6bb29345c6d2",
        "leechers": "1123",
        "seeders": "826",
        "num_files": "11",
        "size": "9645362426",
        "username": "Dr.XJ",
        "added": "1503608363",
        "status": "vip",
        "category": "208",
        "imdb": ""
    }]"#;

    #[test]
    fn a_row_becomes_a_hit_with_the_numbers_parsed() {
        let hit = Hit::try_from(raw(ROW).pop().unwrap()).unwrap();
        assert_eq!(hit.id, 18461548);
        assert_eq!(hit.seeders, 826);
        assert_eq!(hit.size_bytes, 9_645_362_426);
        assert_eq!(hit.added, 1_503_608_363);
        assert_eq!(hit.category_label, "HD TV shows");
        // Upper case, because that is how a magnet spells an infohash.
        assert_eq!(hit.info_hash, "31A5EA99284B3603E94EF861311B6BB29345C6D2");
        assert!(hit.magnet.contains(&hit.info_hash));
    }

    #[test]
    fn a_row_with_no_infohash_is_not_a_hit() {
        // It would render as a perfectly ordinary result and then fail to
        // resolve, thirty seconds later, looking like a bug in the engine.
        let json = ROW.replace("31a5ea99284b3603e94ef861311b6bb29345c6d2", "");
        assert!(Hit::try_from(raw(&json).pop().unwrap()).is_err());
    }

    #[test]
    fn html_entities_in_a_name_are_decoded() {
        // Observed live. Rendered as text an entity shows up literally, which
        // looks like our bug rather than theirs.
        let json = ROW.replace(
            "Some.Film.1080p.BluRay.x265",
            "game of thrones s01E01 espa&ntilde;ol latino &amp; more",
        );
        let hit = Hit::try_from(raw(&json).pop().unwrap()).unwrap();
        assert_eq!(hit.name, "game of thrones s01E01 español latino & more");
        // And the decoded ampersand must still be encoded into the magnet, or
        // the display name grows a parameter of the uploader's choosing.
        assert!(hit.magnet.contains("%26"), "{}", hit.magnet);
        assert!(!hit.magnet.contains("&ntilde"), "{}", hit.magnet);
    }

    #[test]
    fn a_missing_seeder_count_does_not_lose_the_result() {
        let json = ROW.replace(r#""seeders": "826""#, r#""seeders": """#);
        let hit = Hit::try_from(raw(&json).pop().unwrap()).unwrap();
        assert_eq!(hit.seeders, 0);
    }

    #[test]
    fn the_no_results_sentinel_is_recognised() {
        // Not a result. The whole trap is that it looks exactly like one.
        let sentinel = r#"[{
            "id": "0",
            "name": "No results returned",
            "info_hash": "0000000000000000000000000000000000000000",
            "seeders": "0", "leechers": "0", "num_files": "0",
            "size": "0", "username": "", "added": "0",
            "status": "member", "category": "0"
        }]"#;
        let rows = raw(sentinel);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, NO_RESULTS_ID);
        // And for good measure, an all-zero hash is a valid hex string, so the
        // magnet check alone would not have caught it.
        assert!(magnet::uri(&rows[0].info_hash.to_uppercase(), "x").is_some());
    }
}
