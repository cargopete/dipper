//! Building a Torznab query, and reading what comes back.
//!
//! The wire format is RSS with an extra namespace bolted on, which is a 2007
//! decision everybody has been living with since. The parts that matter are not
//! in the RSS at all: seeders, the infohash and the magnet arrive as
//! `<torznab:attr name="..." value="..."/>` elements inside each item.
//!
//! Parsing is kept away from the network for the usual reason, and rather more
//! than usual here: this is the seam where a change at an indexer's end turns
//! into results for the wrong thing, and it should be testable against a
//! recorded body rather than against somebody's server.

use quick_xml::Reader;
use quick_xml::events::Event;

use crate::error::{Error, Result};

/// Torznab category numbers, which are the newznab ones with torrents added.
///
/// Only the video ones, and deliberately never a bare `2000` on its own when a
/// search could be answered from every category on the indexer: the same
/// reasoning as apibay's `cat=0`, which returns the adult categories for an
/// innocent query.
pub mod category {
    /// Films, all of them.
    pub const MOVIES: u32 = 2000;
    /// Television, all of it.
    pub const TV: u32 = 5000;

    /// What balerion asks for when nobody said otherwise.
    pub const VIDEO: &[u32] = &[MOVIES, TV];
}

/// What to ask an indexer for.
#[derive(Debug, Clone)]
pub struct Query {
    pub terms: String,
    pub categories: Vec<u32>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub limit: u32,
}

impl Query {
    pub fn new(terms: impl Into<String>) -> Self {
        Self {
            terms: terms.into(),
            categories: category::VIDEO.to_vec(),
            season: None,
            episode: None,
            limit: 50,
        }
    }

    pub fn episode(mut self, season: u32, episode: u32) -> Self {
        self.season = Some(season);
        self.episode = Some(episode);
        self
    }

    pub fn categories(mut self, categories: impl IntoIterator<Item = u32>) -> Self {
        self.categories = categories.into_iter().collect();
        self
    }

    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = limit.clamp(1, 200);
        self
    }

    /// The URL to ask, given an indexer's base and key.
    ///
    /// `t=search` rather than `t=tvsearch` even when a season and episode are
    /// given, because not every indexer implements the specialised endpoints
    /// and the general one accepts the same parameters.
    pub fn url(&self, base: &str, api_key: &str) -> String {
        let base = base.trim_end_matches('/');
        let mut url = format!(
            "{base}/api?t=search&apikey={}&limit={}",
            urlencoding::encode(api_key),
            self.limit
        );
        if !self.terms.trim().is_empty() {
            url.push_str(&format!("&q={}", urlencoding::encode(self.terms.trim())));
        }
        if !self.categories.is_empty() {
            let categories: Vec<String> = self.categories.iter().map(|id| id.to_string()).collect();
            url.push_str(&format!("&cat={}", categories.join(",")));
        }
        if let Some(season) = self.season {
            url.push_str(&format!("&season={season}"));
        }
        if let Some(episode) = self.episode {
            url.push_str(&format!("&ep={episode}"));
        }
        url
    }
}

/// One result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    /// A magnet, either as the indexer gave it or built from the infohash.
    ///
    /// Not optional: a result balerion cannot open is not a result, so anything
    /// offering only a `.torrent` URL is dropped during parsing rather than
    /// shown and then failing on click. The count of those is reported.
    pub magnet: String,
    pub info_hash: String,
    pub size_bytes: u64,
    pub seeders: u64,
    pub leechers: u64,
    /// Which indexer answered, for the label. With several configured, knowing
    /// where a result came from is most of what makes the list legible.
    pub indexer: String,
    pub category: Option<u32>,
}

/// What a search produced, including what it had to throw away.
#[derive(Debug, Clone, Default)]
pub struct Answer {
    pub hits: Vec<Hit>,
    /// Items that named only a `.torrent` URL, which balerion cannot take.
    ///
    /// Reported rather than hidden, for the same reason the seeder floor is:
    /// "twelve results, nine of them not usable" is a useful thing to know
    /// about an indexer, and quietly showing three reads as though three was
    /// all there was.
    pub without_magnet: usize,
}

/// Trackers put on a magnet built from a bare infohash.
///
/// Only used when the indexer gave us a hash and no magnet of its own. Open
/// trackers, so nothing here assumes a private swarm.
const DEFAULT_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://tracker.torrent.eu.org:451/announce",
];

/// Build a magnet from an infohash and a name.
pub fn magnet_for(info_hash: &str, name: &str) -> String {
    let trackers: String = DEFAULT_TRACKERS
        .iter()
        .map(|url| format!("&tr={}", urlencoding::encode(url)))
        .collect();
    format!(
        "magnet:?xt=urn:btih:{}&dn={}{trackers}",
        info_hash.to_lowercase(),
        urlencoding::encode(name)
    )
}

/// Is this a 40-character hex infohash?
fn looks_like_a_hash(value: &str) -> bool {
    value.len() == 40 && value.chars().all(|c| c.is_ascii_hexdigit())
}

/// One item, as it is being assembled out of the XML.
#[derive(Debug, Default)]
struct Partial {
    title: Option<String>,
    link: Option<String>,
    size: Option<u64>,
    seeders: Option<u64>,
    leechers: Option<u64>,
    info_hash: Option<String>,
    magnet: Option<String>,
    category: Option<u32>,
}

impl Partial {
    /// Tidy the accumulated text once, now that all of it has arrived.
    fn trim(&mut self) {
        for field in [&mut self.title, &mut self.link] {
            if let Some(value) = field {
                let tidied = value.trim().to_string();
                if tidied.is_empty() {
                    *field = None;
                } else {
                    *value = tidied;
                }
            }
        }
    }

    fn finish(self, indexer: &str) -> Option<Hit> {
        let title = self.title?;

        // Three ways an indexer might hand over something we can open, in
        // descending order of how much it was actually told us.
        let magnet = self
            .magnet
            .filter(|magnet| magnet.starts_with("magnet:"))
            .or_else(|| self.link.clone().filter(|link| link.starts_with("magnet:")))
            .or_else(|| {
                self.info_hash
                    .as_deref()
                    .filter(|hash| looks_like_a_hash(hash))
                    .map(|hash| magnet_for(hash, &title))
            })?;

        // Recovered from the magnet when the indexer did not say, since the
        // deduplication downstream keys on it.
        let info_hash = self
            .info_hash
            .filter(|hash| looks_like_a_hash(hash))
            .or_else(|| {
                magnet
                    .split("xt=urn:btih:")
                    .nth(1)?
                    .split('&')
                    .next()
                    .map(|hash| hash.to_ascii_lowercase())
                    .filter(|hash| looks_like_a_hash(hash))
            })
            .unwrap_or_default();

        Some(Hit {
            title,
            magnet,
            info_hash,
            size_bytes: self.size.unwrap_or(0),
            seeders: self.seeders.unwrap_or(0),
            leechers: self.leechers.unwrap_or(0),
            indexer: indexer.to_string(),
            category: self.category,
        })
    }
}

/// Read a Torznab RSS response.
pub fn parse(body: &[u8], indexer: &str) -> Result<Answer> {
    let mut reader = Reader::from_reader(body);
    /* Deliberately *not* trimming each text event. Because the reader splits
     * text around every entity reference, trimming per fragment eats the spaces
     * either side of one, and `Tom &amp; Jerry` comes out as `Tom&Jerry`. The
     * accumulated value is trimmed once, at the closing tag, which is the only
     * place the whole string exists. */
    reader.config_mut().trim_text(false);

    let mut answer = Answer::default();
    let mut item: Option<Partial> = None;
    let mut field: Option<String> = None;
    let mut saw_channel = false;

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Err(err) => {
                return Err(Error::Malformed(format!(
                    "at byte {}: {err}",
                    reader.buffer_position()
                )));
            }

            Ok(Event::Start(tag)) | Ok(Event::Empty(tag)) => {
                let name = local_name(tag.name().as_ref());
                match name.as_str() {
                    "channel" => saw_channel = true,
                    "item" => item = Some(Partial::default()),
                    // The interesting fields are all attributes on these.
                    "attr" if item.is_some() => {
                        let (key, value) = attr_pair(&tag);
                        if let (Some(item), Some(key), Some(value)) = (item.as_mut(), key, value) {
                            apply_attr(item, &key, &value);
                        }
                    }
                    "enclosure" if item.is_some() => {
                        // Some indexers put the magnet here and nowhere else.
                        if let Some(item) = item.as_mut() {
                            if let Some(url) = attribute(&tag, "url") {
                                if url.starts_with("magnet:") {
                                    item.magnet = Some(url);
                                } else {
                                    item.link.get_or_insert(url);
                                }
                            }
                            if let Some(length) = attribute(&tag, "length")
                                && let Ok(length) = length.parse::<u64>()
                            {
                                item.size.get_or_insert(length);
                            }
                        }
                    }
                    other => field = Some(other.to_string()),
                }
            }

            /* Appended rather than assigned, which is the whole of a bug worth
             * recording. quick-xml splits text around entity references, so
             * `magnet:?xt=urn:btih:...&amp;dn=x` arrives as three events: the
             * text before the `&`, the reference, and the text after. Assigning
             * left `dn=x` as the entire link, and since release names are full
             * of ampersands and apostrophes it would have quietly mangled
             * titles too. */
            Ok(Event::Text(text)) => {
                let (Some(item), Some(field)) = (item.as_mut(), field.as_deref()) else {
                    continue;
                };
                let value = text.decode().unwrap_or_default().into_owned();
                match field {
                    "title" => item.title.get_or_insert_default().push_str(&value),
                    "link" => item.link.get_or_insert_default().push_str(&value),
                    "size" => item.size = value.parse().ok(),
                    // `<comments>` and the rest are of no interest.
                    _ => {}
                }
            }

            // The other half of the same thing: put the character back.
            Ok(Event::GeneralRef(reference)) => {
                let (Some(item), Some(field)) = (item.as_mut(), field.as_deref()) else {
                    continue;
                };
                let Some(resolved) = reference.resolve_char_ref().ok().flatten().or_else(|| {
                    match reference.as_ref() {
                        b"amp" => Some('&'),
                        b"lt" => Some('<'),
                        b"gt" => Some('>'),
                        b"quot" => Some('"'),
                        b"apos" => Some('\''),
                        _ => None,
                    }
                }) else {
                    continue;
                };
                match field {
                    "title" => item.title.get_or_insert_default().push(resolved),
                    "link" => item.link.get_or_insert_default().push(resolved),
                    _ => {}
                }
            }

            Ok(Event::End(tag)) => {
                if local_name(tag.name().as_ref()) == "item"
                    && let Some(mut partial) = item.take()
                {
                    partial.trim();
                    match partial.finish(indexer) {
                        Some(hit) => answer.hits.push(hit),
                        None => answer.without_magnet += 1,
                    }
                }
                field = None;
            }
            _ => {}
        }
    }

    if !saw_channel {
        // An indexer answering an error usually does it as a bare `<error>`
        // document with a 200, which would otherwise read as "no results".
        return Err(Error::Malformed(
            "the response was not a Torznab feed; the indexer may have refused the key".into(),
        ));
    }
    Ok(answer)
}

fn apply_attr(item: &mut Partial, key: &str, value: &str) {
    match key {
        "seeders" => item.seeders = value.parse().ok(),
        "peers" => {
            // `peers` is seeders plus leechers, so the leechers are what is
            // left after taking the seeders off. Indexers that report
            // `leechers` directly overwrite this below.
            if let Ok(peers) = value.parse::<u64>() {
                item.leechers = Some(peers.saturating_sub(item.seeders.unwrap_or(0)));
            }
        }
        "leechers" => item.leechers = value.parse().ok(),
        "size" => item.size = value.parse().ok(),
        "infohash" => item.info_hash = Some(value.to_ascii_lowercase()),
        "magneturl" => item.magnet = Some(value.to_string()),
        "category" => item.category = item.category.or_else(|| value.parse().ok()),
        _ => {}
    }
}

/// `torznab:attr` and `attr` are the same element; drop the namespace.
fn local_name(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw);
    name.rsplit(':')
        .next()
        .unwrap_or(&name)
        .to_ascii_lowercase()
}

fn attribute(tag: &quick_xml::events::BytesStart, wanted: &str) -> Option<String> {
    tag.attributes().flatten().find_map(|attr| {
        (local_name(attr.key.as_ref()) == wanted)
            .then(|| String::from_utf8_lossy(&attr.value).into_owned())
    })
}

fn attr_pair(tag: &quick_xml::events::BytesStart) -> (Option<String>, Option<String>) {
    (
        attribute(tag, "name").map(|name| name.to_ascii_lowercase()),
        attribute(tag, "value"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_url_carries_the_key_the_terms_and_the_categories() {
        let url = Query::new("a film").url("https://box.local:9696/api/v1/indexer/3/newznab/", "K");
        assert!(url.contains("/newznab/api?t=search"), "{url}");
        assert!(url.contains("apikey=K"), "{url}");
        assert!(url.contains("q=a%20film"), "{url}");
        assert!(url.contains("cat=2000,5000"), "{url}");
        assert!(
            !url.contains("//api?"),
            "the trailing slash must not double: {url}"
        );
    }

    #[test]
    fn an_episode_query_carries_its_numbers() {
        let url = Query::new("a show").episode(2, 5).url("http://x/", "K");
        assert!(url.contains("&season=2"), "{url}");
        assert!(url.contains("&ep=5"), "{url}");
    }

    #[test]
    fn a_browse_with_no_terms_asks_for_no_terms_rather_than_an_empty_one() {
        // `q=` is not the same as omitting it: some indexers answer the former
        // with nothing at all.
        let url = Query::new("   ").url("http://x", "K");
        assert!(!url.contains("q="), "{url}");
    }

    #[test]
    fn the_limit_is_clamped_to_something_an_indexer_will_accept() {
        assert!(
            Query::new("x")
                .limit(9_000)
                .url("http://x", "K")
                .contains("limit=200")
        );
        assert!(
            Query::new("x")
                .limit(0)
                .url("http://x", "K")
                .contains("limit=1")
        );
    }

    fn feed(items: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <rss version="2.0" xmlns:torznab="http://torznab.com/schemas/2015/feed">
              <channel><title>Indexer</title>{items}</channel>
            </rss>"#
        )
        .into_bytes()
    }

    const HASH: &str = "30f15834bd5cb994bec71635455691acd64875e4";

    #[test]
    fn an_item_with_a_magnet_attribute_is_read() {
        let body = feed(&format!(
            r#"<item>
                 <title>Some.Film.1080p</title>
                 <size>2147483648</size>
                 <torznab:attr name="seeders" value="42"/>
                 <torznab:attr name="peers" value="50"/>
                 <torznab:attr name="infohash" value="{HASH}"/>
                 <torznab:attr name="magneturl" value="magnet:?xt=urn:btih:{HASH}"/>
               </item>"#
        ));
        let answer = parse(&body, "prowlarr").unwrap();
        assert_eq!(answer.hits.len(), 1, "{answer:?}");

        let hit = &answer.hits[0];
        assert_eq!(hit.title, "Some.Film.1080p");
        assert_eq!(hit.seeders, 42);
        assert_eq!(hit.leechers, 8, "peers is seeders plus leechers");
        assert_eq!(hit.size_bytes, 2_147_483_648);
        assert_eq!(hit.info_hash, HASH);
        assert_eq!(hit.indexer, "prowlarr");
    }

    #[test]
    fn an_item_with_only_an_infohash_gets_a_magnet_built_for_it() {
        // bitmagnet and Zilean both do this: they know the hash and have no
        // opinion about trackers.
        let body = feed(&format!(
            r#"<item><title>A Thing</title>
                 <torznab:attr name="infohash" value="{HASH}"/>
               </item>"#
        ));
        let hit = &parse(&body, "bitmagnet").unwrap().hits[0];
        assert!(
            hit.magnet
                .starts_with(&format!("magnet:?xt=urn:btih:{HASH}"))
        );
        assert!(
            hit.magnet.contains("&tr="),
            "with trackers to find it: {}",
            hit.magnet
        );
        assert!(hit.magnet.contains("dn=A%20Thing"));
    }

    #[test]
    fn a_magnet_in_the_link_element_is_used_when_there_is_nothing_better() {
        let body = feed(&format!(
            r#"<item><title>A Thing</title><link>magnet:?xt=urn:btih:{HASH}&amp;dn=x</link></item>"#
        ));
        let hit = &parse(&body, "jackett").unwrap().hits[0];
        assert!(hit.magnet.starts_with("magnet:"));
        // And the hash is recovered from it, because deduplication keys on it.
        assert_eq!(hit.info_hash, HASH);
    }

    #[test]
    fn an_item_offering_only_a_torrent_url_is_counted_rather_than_shown() {
        // balerion cannot take a `.torrent` URL, so offering it would produce a
        // result that fails when clicked. Counted, because "nine of twelve were
        // not usable" is worth knowing about an indexer.
        let body = feed(
            r#"<item><title>A Thing</title>
                 <link>https://indexer.example/download/abc.torrent</link>
               </item>"#,
        );
        let answer = parse(&body, "x").unwrap();
        assert!(answer.hits.is_empty());
        assert_eq!(answer.without_magnet, 1);
    }

    #[test]
    fn a_magnet_in_an_enclosure_is_found_too() {
        let body = feed(&format!(
            r#"<item><title>A Thing</title>
                 <enclosure url="magnet:?xt=urn:btih:{HASH}" length="1024" type="application/x-bittorrent"/>
               </item>"#
        ));
        let hit = &parse(&body, "x").unwrap().hits[0];
        assert_eq!(hit.size_bytes, 1024);
        assert_eq!(hit.info_hash, HASH);
    }

    #[test]
    fn leechers_reported_directly_beat_a_figure_derived_from_peers() {
        let body = feed(&format!(
            r#"<item><title>x</title>
                 <torznab:attr name="seeders" value="10"/>
                 <torznab:attr name="peers" value="30"/>
                 <torznab:attr name="leechers" value="7"/>
                 <torznab:attr name="infohash" value="{HASH}"/>
               </item>"#
        ));
        assert_eq!(parse(&body, "x").unwrap().hits[0].leechers, 7);
    }

    #[test]
    fn an_empty_feed_is_not_an_error() {
        let answer = parse(&feed(""), "x").unwrap();
        assert!(answer.hits.is_empty());
        assert_eq!(answer.without_magnet, 0);
    }

    #[test]
    fn an_error_document_is_refused_rather_than_read_as_no_results() {
        // Indexers answer a bad key with a 200 and an `<error>` element, which
        // would otherwise present for ever as "nothing matched that".
        let body =
            br#"<?xml version="1.0"?><error code="100" description="Incorrect user credentials"/>"#;
        assert!(parse(body, "x").is_err());
    }

    #[test]
    fn rubbish_is_refused() {
        assert!(parse(b"not xml at all <<<", "x").is_err());
        assert!(parse(b"", "x").is_err());
    }

    #[test]
    fn an_ampersand_in_a_title_survives_intact() {
        // Release names are full of these, and the XML reader hands the text
        // back in pieces around every one of them.
        let body = feed(&format!(
            r#"<item><title>Tom &amp; Jerry &apos;53</title>
                 <torznab:attr name="infohash" value="{HASH}"/>
               </item>"#
        ));
        assert_eq!(parse(&body, "x").unwrap().hits[0].title, "Tom & Jerry '53");
    }

    #[test]
    fn several_items_all_come_back() {
        let one = format!(
            r#"<item><title>One</title><torznab:attr name="infohash" value="{HASH}"/></item>"#
        );
        let two = r#"<item><title>Two</title><torznab:attr name="infohash" value="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"/></item>"#;
        let answer = parse(&feed(&format!("{one}{two}")), "x").unwrap();
        assert_eq!(answer.hits.len(), 2);
        assert_eq!(answer.hits[0].title, "One");
        assert_eq!(answer.hits[1].title, "Two");
    }

    #[test]
    fn a_nonsense_infohash_does_not_become_a_magnet() {
        // A magnet built from something that is not a hash resolves to nothing
        // and wastes a minute of somebody's evening finding that out.
        let body =
            feed(r#"<item><title>x</title><torznab:attr name="infohash" value="nope"/></item>"#);
        let answer = parse(&body, "x").unwrap();
        assert!(answer.hits.is_empty());
        assert_eq!(answer.without_magnet, 1);
    }
}
