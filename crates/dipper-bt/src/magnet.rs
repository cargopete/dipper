//! Magnet URI parsing (BEP 9 / BEP 53 style `magnet:?xt=urn:btih:...`).
//!
//! A magnet is not a torrent. It carries an infohash and some discovery hints,
//! and nothing at all about piece length, piece hashes or file layout. Those
//! have to be recovered from the swarm; see [`crate::peer`].

use std::net::SocketAddr;

use crate::error::{Error, Result};
use crate::infohash::InfoHash;

/// A parsed magnet link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Magnet {
    pub info_hash: InfoHash,
    /// `dn`. A display hint only; it is attacker-controlled and must never be
    /// used as a filesystem path. The real name comes from the info dict.
    pub display_name: Option<String>,
    /// `tr`, in the order given.
    pub trackers: Vec<String>,
    /// `ws`, BEP 19 webseeds.
    pub webseeds: Vec<String>,
    /// `x.pe`, peers to try directly.
    pub peers: Vec<SocketAddr>,
    /// A v2 `urn:btmh:` topic, if one was present. We record it so we can say
    /// so, but the engine downloads v1 swarms.
    pub v2_multihash: Option<String>,
}

impl Magnet {
    /// Parse a `magnet:?...` URI.
    ///
    /// Splits on `&` first and percent-decodes each value afterwards: decoding
    /// the whole query string first would corrupt tracker URLs, which contain
    /// encoded `:` and `/`.
    pub fn parse(uri: &str) -> Result<Self> {
        let query = uri
            .strip_prefix("magnet:?")
            .ok_or_else(|| Error::Magnet("does not start with `magnet:?`".into()))?;

        let mut info_hash = None;
        let mut v2_multihash = None;
        let mut display_name = None;
        let mut trackers = Vec::new();
        let mut webseeds = Vec::new();
        let mut peers = Vec::new();

        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = match pair.split_once('=') {
                Some((key, value)) => (key, decode(value)),
                None => continue,
            };
            match key {
                "xt" | "xt.1" | "xt.2" => {
                    if let Some(v1) = value.strip_prefix("urn:btih:") {
                        // Keep the first v1 topic; some magnets list several.
                        if info_hash.is_none() {
                            info_hash = Some(InfoHash::parse(v1)?);
                        }
                    } else if let Some(v2) = value.strip_prefix("urn:btmh:") {
                        v2_multihash.get_or_insert_with(|| v2.to_string());
                    }
                }
                "dn" => {
                    display_name.get_or_insert(value);
                }
                "tr" => push_unique(&mut trackers, value),
                "ws" => push_unique(&mut webseeds, value),
                "x.pe" => {
                    if let Ok(addr) = value.parse::<SocketAddr>()
                        && !peers.contains(&addr)
                    {
                        peers.push(addr);
                    }
                }
                _ => {}
            }
        }

        let info_hash = info_hash.ok_or_else(|| {
            if v2_multihash.is_some() {
                Error::Magnet("v2-only magnet (urn:btmh); dipper needs a v1 urn:btih topic".into())
            } else {
                Error::Magnet("no `xt=urn:btih:` topic".into())
            }
        })?;

        Ok(Self {
            info_hash,
            display_name,
            trackers,
            webseeds,
            peers,
            v2_multihash,
        })
    }

    /// Render back to a magnet URI. Useful for logging and for round-tripping
    /// a `.torrent` into a link.
    pub fn to_uri(&self) -> String {
        let mut uri = format!("magnet:?xt=urn:btih:{}", self.info_hash);
        if let Some(name) = &self.display_name {
            uri.push_str(&format!("&dn={}", urlencoding::encode(name)));
        }
        for tracker in &self.trackers {
            uri.push_str(&format!("&tr={}", urlencoding::encode(tracker)));
        }
        for webseed in &self.webseeds {
            uri.push_str(&format!("&ws={}", urlencoding::encode(webseed)));
        }
        for peer in &self.peers {
            uri.push_str(&format!("&x.pe={}", urlencoding::encode(&peer.to_string())));
        }
        uri
    }
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !value.is_empty() && !list.contains(&value) {
        list.push(value);
    }
}

fn decode(value: &str) -> String {
    urlencoding::decode(value)
        .map(|decoded| decoded.into_owned())
        .unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "15c74d4165fc2ffff997d576bf44b4b25cbeb04e";

    #[test]
    fn parses_a_full_archive_org_magnet() {
        let uri = format!(
            "magnet:?xt=urn:btih:{HEX}&dn=nasa\
             &tr=http%3A%2F%2Fbt1.archive.org%3A6969%2Fannounce\
             &tr=http%3A%2F%2Fbt2.archive.org%3A6969%2Fannounce\
             &ws=https%3A%2F%2Farchive.org%2Fdownload%2F"
        );
        let magnet = Magnet::parse(&uri).unwrap();

        assert_eq!(magnet.info_hash.to_hex(), HEX);
        assert_eq!(magnet.display_name.as_deref(), Some("nasa"));
        assert_eq!(
            magnet.trackers,
            vec![
                "http://bt1.archive.org:6969/announce",
                "http://bt2.archive.org:6969/announce"
            ],
            "tracker URLs must survive percent-decoding intact"
        );
        assert_eq!(magnet.webseeds, vec!["https://archive.org/download/"]);
    }

    #[test]
    fn accepts_the_minimal_form() {
        let magnet = Magnet::parse(&format!("magnet:?xt=urn:btih:{HEX}")).unwrap();
        assert_eq!(magnet.info_hash.to_hex(), HEX);
        assert!(magnet.trackers.is_empty());
        assert!(magnet.display_name.is_none());
    }

    #[test]
    fn accepts_a_base32_topic() {
        let base32 = data_encoding::BASE32
            .encode(InfoHash::parse(HEX).unwrap().as_bytes())
            .trim_end_matches('=')
            .to_string();
        let magnet = Magnet::parse(&format!("magnet:?xt=urn:btih:{base32}")).unwrap();
        assert_eq!(magnet.info_hash.to_hex(), HEX);
    }

    #[test]
    fn parses_direct_peers() {
        let magnet =
            Magnet::parse(&format!("magnet:?xt=urn:btih:{HEX}&x.pe=1.2.3.4%3A6881")).unwrap();
        assert_eq!(
            magnet.peers,
            vec!["1.2.3.4:6881".parse::<SocketAddr>().unwrap()]
        );
    }

    #[test]
    fn prefers_the_v1_topic_of_a_hybrid_magnet() {
        let uri = format!(
            "magnet:?xt=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e&xt=urn:btih:{HEX}"
        );
        let magnet = Magnet::parse(&uri).unwrap();
        assert_eq!(magnet.info_hash.to_hex(), HEX);
        assert!(magnet.v2_multihash.is_some());
    }

    #[test]
    fn v2_only_magnets_are_refused_with_an_explanation() {
        let uri = "magnet:?xt=urn:btmh:1220caf1e1c30e81cb361b9ee167c4aa64228a7fa4fa9f6105232b28ad099f3a302e";
        let err = Magnet::parse(uri).unwrap_err();
        assert!(format!("{err}").contains("v2-only"), "{err}");
    }

    #[test]
    fn rejects_rubbish() {
        assert!(Magnet::parse("").is_err());
        assert!(Magnet::parse("https://example.com").is_err());
        assert!(Magnet::parse("magnet:?dn=no+topic+here").is_err());
        assert!(Magnet::parse("magnet:?xt=urn:btih:nothex").is_err());
    }

    #[test]
    fn ignores_unknown_and_malformed_parameters() {
        let magnet = Magnet::parse(&format!(
            "magnet:?xt=urn:btih:{HEX}&so=0-2&kt=keywords&novalue&x.pe=not-an-address"
        ))
        .unwrap();
        assert_eq!(magnet.info_hash.to_hex(), HEX);
        assert!(magnet.peers.is_empty());
    }

    #[test]
    fn deduplicates_repeated_trackers() {
        let magnet = Magnet::parse(&format!(
            "magnet:?xt=urn:btih:{HEX}&tr=udp%3A%2F%2Ft.example%3A80&tr=udp%3A%2F%2Ft.example%3A80"
        ))
        .unwrap();
        assert_eq!(magnet.trackers.len(), 1);
    }

    #[test]
    fn round_trips_through_a_uri() {
        let uri = format!("magnet:?xt=urn:btih:{HEX}&dn=some+thing&tr=udp%3A%2F%2Ft.example%3A80");
        let parsed = Magnet::parse(&uri).unwrap();
        let reparsed = Magnet::parse(&parsed.to_uri()).unwrap();
        assert_eq!(parsed, reparsed);
    }
}
