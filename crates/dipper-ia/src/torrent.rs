//! Metainfo parsing (BEP 3), infohash computation and magnet links (BEP 9's
//! entry point), with the webseed list (BEP 19) that archive.org relies on.
//!
//! The infohash is the SHA-1 of the `info` dictionary *exactly as it appears
//! on the wire*, so we locate its byte span with a small bencode scanner
//! rather than re-encoding a parsed structure. Re-encoding would silently drop
//! unknown keys and change the hash.

use std::collections::HashMap;
use std::ops::Range;

use serde_bencode::value::Value as Bencode;
use sha1::{Digest, Sha1};

use crate::client::IaClient;
use crate::error::{Error, Result};
use crate::metadata::ItemMetadata;

/// archive.org's own trackers. They will not let third parties seed, but they
/// do answer announces for leeching.
pub const IA_TRACKERS: &[&str] = &[
    "http://bt1.archive.org:6969/announce",
    "http://bt2.archive.org:6969/announce",
];

/// Guard against hostile nesting in untrusted bencode.
const MAX_DEPTH: usize = 32;

/// One file inside a torrent, with its offset in the concatenated piece space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFile {
    pub path: String,
    pub length: u64,
    /// Byte offset of this file within the torrent's flat piece space.
    pub offset: u64,
}

/// A parsed `.torrent`.
#[derive(Debug, Clone)]
pub struct Metainfo {
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: u64,
    /// Concatenated 20-byte SHA-1 piece hashes.
    pub pieces: Vec<u8>,
    pub files: Vec<TorrentFile>,
    pub total_length: u64,
    pub announce: Vec<String>,
    /// BEP 19 `url-list`. For archive.org items this is where the bytes
    /// actually come from.
    pub webseeds: Vec<String>,
    pub private: bool,
    pub comment: Option<String>,
    pub created_by: Option<String>,
}

impl Metainfo {
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let info_span = info_span(raw)?;
        let info_hash: [u8; 20] = Sha1::digest(&raw[info_span]).into();

        let root = match serde_bencode::from_bytes::<Bencode>(raw) {
            Ok(Bencode::Dict(dict)) => dict,
            Ok(_) => return Err(Error::Torrent("metainfo is not a dictionary".into())),
            Err(err) => return Err(Error::Torrent(format!("bencode: {err}"))),
        };

        let info = match root.get(b"info".as_slice()) {
            Some(Bencode::Dict(dict)) => dict,
            _ => return Err(Error::Torrent("missing info dictionary".into())),
        };

        let name = dict_string(info, b"name")
            .ok_or_else(|| Error::Torrent("info dict has no name".into()))?;
        let piece_length = dict_int(info, b"piece length")
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::Torrent("info dict has no piece length".into()))?
            as u64;
        let pieces = match info.get(b"pieces".as_slice()) {
            Some(Bencode::Bytes(bytes)) => bytes.clone(),
            _ => return Err(Error::Torrent("info dict has no pieces".into())),
        };
        if pieces.is_empty() || pieces.len() % 20 != 0 {
            return Err(Error::Torrent(format!(
                "piece hash blob is {} bytes, not a multiple of 20",
                pieces.len()
            )));
        }

        let files = parse_files(info, &name)?;
        let total_length: u64 = files.iter().map(|f| f.length).sum();

        let expected_pieces = total_length.div_ceil(piece_length);
        if expected_pieces != (pieces.len() / 20) as u64 {
            return Err(Error::Torrent(format!(
                "{} piece hashes for {total_length} bytes at {piece_length} per piece (expected {expected_pieces})",
                pieces.len() / 20
            )));
        }

        Ok(Self {
            info_hash,
            name,
            piece_length,
            pieces,
            files,
            total_length,
            announce: parse_announce(&root),
            webseeds: parse_url_list(&root),
            private: dict_int(info, b"private") == Some(1),
            comment: dict_string(&root, b"comment"),
            created_by: dict_string(&root, b"created by"),
        })
    }

    pub fn info_hash_hex(&self) -> String {
        hex::encode(self.info_hash)
    }

    pub fn piece_count(&self) -> usize {
        self.pieces.len() / 20
    }

    pub fn piece_hash(&self, index: usize) -> Option<&[u8]> {
        self.pieces.get(index * 20..index * 20 + 20)
    }

    /// Length of a specific piece; the last one is usually short.
    pub fn piece_size(&self, index: usize) -> Option<u64> {
        let count = self.piece_count() as u64;
        let index = index as u64;
        if index >= count {
            return None;
        }
        if index + 1 < count {
            Some(self.piece_length)
        } else {
            let remainder = self.total_length % self.piece_length;
            Some(if remainder == 0 {
                self.piece_length
            } else {
                remainder
            })
        }
    }

    pub fn is_single_file(&self) -> bool {
        self.files.len() == 1 && self.files[0].path == self.name
    }

    /// A magnet link carrying everything a client needs to find this swarm.
    pub fn magnet(&self) -> String {
        let mut magnet = format!("magnet:?xt=urn:btih:{}", self.info_hash_hex());
        magnet.push_str(&format!("&dn={}", urlencoding::encode(&self.name)));
        for tracker in &self.announce {
            magnet.push_str(&format!("&tr={}", urlencoding::encode(tracker)));
        }
        for webseed in &self.webseeds {
            magnet.push_str(&format!("&ws={}", urlencoding::encode(webseed)));
        }
        magnet
    }
}

fn parse_files(info: &HashMap<Vec<u8>, Bencode>, name: &str) -> Result<Vec<TorrentFile>> {
    match info.get(b"files".as_slice()) {
        // Multi-file: paths are relative to the info `name` directory.
        Some(Bencode::List(entries)) => {
            let mut files = Vec::with_capacity(entries.len());
            let mut offset = 0u64;
            for entry in entries {
                let Bencode::Dict(entry) = entry else {
                    return Err(Error::Torrent("files entry is not a dictionary".into()));
                };
                let length = dict_int(entry, b"length")
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| Error::Torrent("files entry has no length".into()))?
                    as u64;
                let Some(Bencode::List(segments)) = entry.get(b"path".as_slice()) else {
                    return Err(Error::Torrent("files entry has no path".into()));
                };
                let mut path = String::from(name);
                for segment in segments {
                    let Bencode::Bytes(segment) = segment else {
                        return Err(Error::Torrent("path segment is not a string".into()));
                    };
                    let segment = String::from_utf8_lossy(segment);
                    // Refuse to be talked out of the download directory.
                    if segment.is_empty() || segment == ".." || segment.contains('/') {
                        return Err(Error::Torrent(format!("unsafe path segment: {segment:?}")));
                    }
                    path.push('/');
                    path.push_str(&segment);
                }
                files.push(TorrentFile {
                    path,
                    length,
                    offset,
                });
                offset += length;
            }
            if files.is_empty() {
                return Err(Error::Torrent("multi-file torrent lists no files".into()));
            }
            Ok(files)
        }
        // Single-file: the info `name` is the file.
        _ => {
            let length = dict_int(info, b"length")
                .filter(|n| *n >= 0)
                .ok_or_else(|| Error::Torrent("info dict has neither files nor length".into()))?
                as u64;
            Ok(vec![TorrentFile {
                path: name.to_string(),
                length,
                offset: 0,
            }])
        }
    }
}

fn parse_announce(root: &HashMap<Vec<u8>, Bencode>) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |url: String| {
        if !url.is_empty() && !out.contains(&url) {
            out.push(url);
        }
    };
    if let Some(url) = dict_string(root, b"announce") {
        push(url);
    }
    if let Some(Bencode::List(tiers)) = root.get(b"announce-list".as_slice()) {
        for tier in tiers {
            if let Bencode::List(urls) = tier {
                for url in urls {
                    if let Bencode::Bytes(url) = url {
                        push(String::from_utf8_lossy(url).into_owned());
                    }
                }
            }
        }
    }
    out
}

/// `url-list` is a string when there is one webseed and a list when there are
/// several. archive.org writes `http://` URLs for hosts that speak HTTPS, so
/// we upgrade them on the way past.
fn parse_url_list(root: &HashMap<Vec<u8>, Bencode>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |url: &[u8]| {
        let url = upgrade_scheme(&String::from_utf8_lossy(url));
        if !url.is_empty() && !out.contains(&url) {
            out.push(url);
        }
    };
    match root.get(b"url-list".as_slice()) {
        Some(Bencode::Bytes(url)) => push(url),
        Some(Bencode::List(urls)) => {
            for url in urls {
                if let Bencode::Bytes(url) = url {
                    push(url);
                }
            }
        }
        _ => {}
    }
    out
}

/// Prefer HTTPS for archive.org hosts, which all support it.
fn upgrade_scheme(url: &str) -> String {
    match url.strip_prefix("http://") {
        Some(rest) if rest.contains("archive.org") => format!("https://{rest}"),
        _ => url.to_string(),
    }
}

fn dict_string(dict: &HashMap<Vec<u8>, Bencode>, key: &[u8]) -> Option<String> {
    match dict.get(key)? {
        Bencode::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

fn dict_int(dict: &HashMap<Vec<u8>, Bencode>, key: &[u8]) -> Option<i64> {
    match dict.get(key)? {
        Bencode::Int(n) => Some(*n),
        _ => None,
    }
}

/// Byte range of the `info` dictionary's value within a raw metainfo file.
pub fn info_span(raw: &[u8]) -> Result<Range<usize>> {
    if raw.first() != Some(&b'd') {
        return Err(Error::Torrent("metainfo does not start with a dict".into()));
    }
    let mut pos = 1;
    while pos < raw.len() && raw[pos] != b'e' {
        let key_end = scan(raw, pos, 0)?;
        let key = &raw[pos..key_end];
        let value_end = scan(raw, key_end, 0)?;
        // Keys are bencoded strings: `<len>:<bytes>`.
        if key.strip_prefix(b"4:") == Some(b"info") {
            return Ok(key_end..value_end);
        }
        pos = value_end;
    }
    Err(Error::Torrent("metainfo has no info dictionary".into()))
}

/// Scan one bencode value starting at `pos`, returning the index just past it.
fn scan(raw: &[u8], pos: usize, depth: usize) -> Result<usize> {
    if depth > MAX_DEPTH {
        return Err(Error::Torrent("bencode nested too deeply".into()));
    }
    match raw.get(pos) {
        Some(b'i') => {
            let end = find(raw, pos + 1, b'e')?;
            Ok(end + 1)
        }
        Some(b'l') | Some(b'd') => {
            let mut cursor = pos + 1;
            loop {
                match raw.get(cursor) {
                    Some(b'e') => return Ok(cursor + 1),
                    Some(_) => cursor = scan(raw, cursor, depth + 1)?,
                    None => return Err(Error::Torrent("truncated bencode container".into())),
                }
            }
        }
        Some(c) if c.is_ascii_digit() => {
            let colon = find(raw, pos, b':')?;
            let len: usize = std::str::from_utf8(&raw[pos..colon])
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| Error::Torrent("bad bencode string length".into()))?;
            let end = colon
                .checked_add(1 + len)
                .filter(|end| *end <= raw.len())
                .ok_or_else(|| Error::Torrent("bencode string runs past end".into()))?;
            Ok(end)
        }
        Some(c) => Err(Error::Torrent(format!(
            "unexpected bencode byte {:?} at offset {pos}",
            *c as char
        ))),
        None => Err(Error::Torrent("truncated bencode".into())),
    }
}

fn find(raw: &[u8], from: usize, needle: u8) -> Result<usize> {
    raw[from.min(raw.len())..]
        .iter()
        .position(|b| *b == needle)
        .map(|offset| from + offset)
        .ok_or_else(|| Error::Torrent("truncated bencode".into()))
}

/// Download and parse an item's derived `.torrent`.
pub async fn fetch(client: &IaClient, item: &ItemMetadata) -> Result<Metainfo> {
    let url = item.torrent_url().ok_or_else(|| Error::Missing {
        identifier: item.identifier.clone(),
        what: "derived .torrent".into(),
    })?;
    let raw = client.get(&url).await?;
    Metainfo::parse(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a bencoded string.
    fn bstr(s: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", s.len()).into_bytes();
        out.extend_from_slice(s);
        out
    }

    /// A minimal but realistic single-file torrent with two webseeds.
    fn single_file_torrent() -> (Vec<u8>, Vec<u8>) {
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i2000e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"xfetch.pdf"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i1024e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&[0xAAu8; 40])); // two pieces
        info.extend(b"e");

        let mut raw = Vec::new();
        raw.extend(b"d");
        raw.extend(bstr(b"announce"));
        raw.extend(bstr(b"http://bt1.archive.org:6969/announce"));
        raw.extend(bstr(b"announce-list"));
        raw.extend(b"ll");
        raw.extend(bstr(b"http://bt1.archive.org:6969/announce"));
        raw.extend(bstr(b"http://bt2.archive.org:6969/announce"));
        raw.extend(b"ee");
        raw.extend(bstr(b"comment"));
        raw.extend(bstr(b"a test item"));
        raw.extend(bstr(b"info"));
        raw.extend(&info);
        raw.extend(bstr(b"url-list"));
        raw.extend(b"l");
        raw.extend(bstr(b"http://ia800308.us.archive.org/21/items/"));
        raw.extend(bstr(b"https://archive.org/download/"));
        raw.extend(b"e");
        raw.extend(b"e");

        (raw, info)
    }

    #[test]
    fn info_span_covers_exactly_the_info_dict() {
        let (raw, info) = single_file_torrent();
        let span = info_span(&raw).unwrap();
        assert_eq!(&raw[span], info.as_slice());
    }

    #[test]
    fn info_hash_is_sha1_of_the_raw_info_dict() {
        let (raw, info) = single_file_torrent();
        let meta = Metainfo::parse(&raw).unwrap();
        let expected: [u8; 20] = Sha1::digest(&info).into();
        assert_eq!(meta.info_hash, expected);
        assert_eq!(meta.info_hash_hex().len(), 40);
    }

    #[test]
    fn parses_a_single_file_torrent() {
        let (raw, _) = single_file_torrent();
        let meta = Metainfo::parse(&raw).unwrap();
        assert_eq!(meta.name, "xfetch.pdf");
        assert_eq!(meta.piece_length, 1024);
        assert_eq!(meta.piece_count(), 2);
        assert_eq!(meta.total_length, 2000);
        assert!(meta.is_single_file());
        assert_eq!(meta.files[0].offset, 0);
        assert_eq!(meta.comment.as_deref(), Some("a test item"));
        assert!(!meta.private);
    }

    #[test]
    fn last_piece_is_short() {
        let (raw, _) = single_file_torrent();
        let meta = Metainfo::parse(&raw).unwrap();
        assert_eq!(meta.piece_size(0), Some(1024));
        assert_eq!(meta.piece_size(1), Some(976));
        assert_eq!(meta.piece_size(2), None);
    }

    #[test]
    fn webseeds_are_upgraded_to_https_and_deduplicated() {
        let (raw, _) = single_file_torrent();
        let meta = Metainfo::parse(&raw).unwrap();
        assert_eq!(
            meta.webseeds,
            vec![
                "https://ia800308.us.archive.org/21/items/",
                "https://archive.org/download/"
            ]
        );
    }

    #[test]
    fn announce_list_is_flattened_without_duplicates() {
        let (raw, _) = single_file_torrent();
        let meta = Metainfo::parse(&raw).unwrap();
        assert_eq!(meta.announce, IA_TRACKERS);
    }

    #[test]
    fn magnet_carries_hash_name_trackers_and_webseeds() {
        let (raw, _) = single_file_torrent();
        let meta = Metainfo::parse(&raw).unwrap();
        let magnet = meta.magnet();
        assert!(magnet.starts_with(&format!("magnet:?xt=urn:btih:{}", meta.info_hash_hex())));
        assert!(magnet.contains("&dn=xfetch.pdf"));
        assert!(magnet.contains("&tr=http%3A%2F%2Fbt1.archive.org%3A6969%2Fannounce"));
        assert!(magnet.contains("&ws=https%3A%2F%2Farchive.org%2Fdownload%2F"));
    }

    #[test]
    fn parses_a_multi_file_torrent_with_offsets() {
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"files"));
        info.extend(b"l");
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i600e");
        info.extend(bstr(b"path"));
        info.extend(b"l");
        info.extend(bstr(b"disc1"));
        info.extend(bstr(b"track01.mp3"));
        info.extend(b"e");
        info.extend(b"e");
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i400e");
        info.extend(bstr(b"path"));
        info.extend(b"l");
        info.extend(bstr(b"track02.mp3"));
        info.extend(b"e");
        info.extend(b"e");
        info.extend(b"e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"an-album"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i512e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&[0xBBu8; 40]));
        info.extend(b"e");

        let mut raw = Vec::new();
        raw.extend(b"d");
        raw.extend(bstr(b"info"));
        raw.extend(&info);
        raw.extend(b"e");

        let meta = Metainfo::parse(&raw).unwrap();
        assert!(!meta.is_single_file());
        assert_eq!(meta.total_length, 1000);
        assert_eq!(
            meta.files,
            vec![
                TorrentFile {
                    path: "an-album/disc1/track01.mp3".into(),
                    length: 600,
                    offset: 0
                },
                TorrentFile {
                    path: "an-album/track02.mp3".into(),
                    length: 400,
                    offset: 600
                },
            ]
        );
        assert!(meta.announce.is_empty());
        assert!(meta.webseeds.is_empty());
    }

    #[test]
    fn rejects_path_traversal() {
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"files"));
        info.extend(b"l");
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i10e");
        info.extend(bstr(b"path"));
        info.extend(b"l");
        info.extend(bstr(b".."));
        info.extend(bstr(b"passwd"));
        info.extend(b"e");
        info.extend(b"e");
        info.extend(b"e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"evil"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i512e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&[0u8; 20]));
        info.extend(b"e");

        let mut raw = Vec::new();
        raw.extend(b"d");
        raw.extend(bstr(b"info"));
        raw.extend(&info);
        raw.extend(b"e");

        let err = Metainfo::parse(&raw).unwrap_err();
        assert!(matches!(err, Error::Torrent(msg) if msg.contains("unsafe path segment")));
    }

    #[test]
    fn rejects_piece_hash_count_mismatch() {
        let (raw, _) = single_file_torrent();
        // Chop one piece hash out of the blob: 2000 bytes at 1024 needs two.
        let mangled = String::from_utf8_lossy(&raw)
            .replace("6:pieces40:", "6:pieces20:")
            .into_bytes();
        let mangled: Vec<u8> = {
            let mut v = Vec::new();
            let mut skipped = false;
            let mut iter = mangled.into_iter().peekable();
            while let Some(b) = iter.next() {
                v.push(b);
                if !skipped && v.ends_with(b"6:pieces20:") {
                    // drop the surplus 20 bytes of hash data
                    for _ in 0..20 {
                        iter.next();
                    }
                    skipped = true;
                }
            }
            v
        };
        assert!(Metainfo::parse(&mangled).is_err());
    }

    #[test]
    fn rejects_truncated_and_hostile_bencode() {
        assert!(Metainfo::parse(b"").is_err());
        assert!(Metainfo::parse(b"d4:infod").is_err());
        assert!(Metainfo::parse(b"d4:info99999:short").is_err());
        let bomb: Vec<u8> = std::iter::repeat_n(b'l', 200).collect();
        assert!(info_span(&bomb).is_err() || Metainfo::parse(&bomb).is_err());
        let deep: Vec<u8> = [b"d4:info".as_slice(), &[b'l'; 100]].concat();
        assert!(Metainfo::parse(&deep).is_err());
    }
}
