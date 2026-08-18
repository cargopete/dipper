//! Building a magnet from an infohash.
//!
//! apibay hands back a hash and a name, not a link, so the link is assembled
//! here. No request is involved: given the JSON, this works offline.

/// The trackers thepiratebay's own magnet links carry.
///
/// A magnet with no tracker list still resolves through the DHT, and spends a
/// minute or two looking thoroughly broken first. Given dipper starts playback
/// as soon as pieces arrive, that opening minute is the whole experience.
pub const TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.bittor.pw:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://tracker.dler.org:6969/announce",
    "udp://exodus.desync.com:6969/announce",
    // tracker.moeking.me used to be here and its domain no longer resolves, so
    // it cost a DNS timeout per magnet and returned nothing. Checked by
    // announcing to each of these directly rather than by trusting the list
    // thepiratebay ships.
    "udp://explodie.org:6969/announce",
];

/// Assemble a magnet URI, or `None` if the hash is not one.
///
/// The check is the point. A magnet built from a blank or truncated hash is
/// syntactically perfect, resolves to nothing, and is indistinguishable from a
/// swarm with no peers, so it would sit in the interface saying "looking for
/// peers" until the viewer gave up and reported a bug.
pub fn uri(info_hash: &str, name: &str) -> Option<String> {
    if info_hash.len() != 40 || !info_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut uri = format!(
        "magnet:?xt=urn:btih:{info_hash}&dn={}",
        urlencoding::encode(name)
    );
    for tracker in TRACKERS {
        uri.push_str("&tr=");
        uri.push_str(&urlencoding::encode(tracker));
    }
    Some(uri)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "31A5EA99284B3603E94EF861311B6BB29345C6D2";

    #[test]
    fn a_magnet_carries_the_hash_the_name_and_every_tracker() {
        let uri = uri(HASH, "Some Film 1080p").unwrap();
        assert!(
            uri.starts_with(&format!("magnet:?xt=urn:btih:{HASH}")),
            "{uri}"
        );
        assert!(uri.contains("&dn=Some%20Film%201080p"), "{uri}");
        assert_eq!(uri.matches("&tr=").count(), TRACKERS.len(), "{uri}");
    }

    #[test]
    fn a_name_cannot_break_out_of_its_parameter() {
        // An ampersand left raw would append a parameter of the uploader's
        // choosing, and `&tr=` in a filename is not a hypothetical.
        let uri = uri(HASH, "film&tr=udp://evil:6969/announce").unwrap();
        assert_eq!(uri.matches("&tr=").count(), TRACKERS.len(), "{uri}");
        assert!(uri.contains("%26tr%3D"), "{uri}");
    }

    #[test]
    fn a_hash_that_is_not_a_hash_yields_no_magnet() {
        assert!(uri("", "x").is_none());
        assert!(uri("31A5EA99", "x").is_none(), "truncated");
        assert!(uri(&"z".repeat(40), "x").is_none(), "not hex");
    }
}
