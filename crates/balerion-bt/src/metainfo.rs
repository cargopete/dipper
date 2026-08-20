//! Metainfo: the `info` dictionary and the things wrapped around it (BEP 3).
//!
//! Can be built either from a whole `.torrent` file or from a bare info
//! dictionary recovered from peers over BEP 9, which is what makes magnets
//! work. Both paths compute the infohash over the original wire bytes.

use serde_bencode::value::Value as Bencode;
use sha1::{Digest, Sha1};

use crate::bencode::{self, Dict};
use crate::error::{Error, Result};
use crate::infohash::InfoHash;
use crate::magnet::Magnet;

/// archive.org's own trackers. They answer announces but reject third-party
/// seeding, so you can leech from them and never give back.
pub const IA_TRACKERS: &[&str] = &[
    "http://bt1.archive.org:6969/announce",
    "http://bt2.archive.org:6969/announce",
];

/// One file inside a torrent, with its offset in the flat piece space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TorrentFile {
    /// Relative path, always beginning with the torrent name for multi-file
    /// torrents. Validated to contain no traversal.
    pub path: String,
    pub length: u64,
    /// Byte offset of this file within the torrent's concatenated piece space.
    pub offset: u64,
}

impl TorrentFile {
    pub fn end(&self) -> u64 {
        self.offset + self.length
    }
}

/// A slice of some byte span that lives in one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSlice {
    pub file_index: usize,
    /// Offset within that file.
    pub file_offset: u64,
    /// Offset within the span that was asked for, which is a piece when the
    /// slices came from [`Metainfo::piece_slices`] and an arbitrary byte range
    /// when they came from [`Metainfo::byte_slices`].
    pub offset_in_span: u64,
    pub length: u64,
}

/// A parsed torrent.
#[derive(Debug, Clone)]
pub struct Metainfo {
    pub info_hash: InfoHash,
    pub name: String,
    pub piece_length: u64,
    /// Concatenated 20-byte SHA-1 piece hashes.
    pub pieces: Vec<u8>,
    pub files: Vec<TorrentFile>,
    pub total_length: u64,
    pub announce: Vec<String>,
    /// BEP 19 `url-list`.
    pub webseeds: Vec<String>,
    pub private: bool,
    pub comment: Option<String>,
    pub created_by: Option<String>,
    /// The info dictionary exactly as it arrived, so we can re-serve it to
    /// other peers and persist it for resume without re-encoding.
    pub raw_info: Vec<u8>,
}

impl Metainfo {
    /// Parse a complete `.torrent` file.
    pub fn parse(raw: &[u8]) -> Result<Self> {
        let span = bencode::info_span(raw)?;
        let raw_info = raw[span].to_vec();

        let root = match serde_bencode::from_bytes::<Bencode>(raw) {
            Ok(Bencode::Dict(dict)) => dict,
            Ok(_) => return Err(Error::Metainfo("metainfo is not a dictionary".into())),
            Err(err) => return Err(Error::Metainfo(format!("bencode: {err}"))),
        };

        let mut meta = Self::from_info_dict(&raw_info)?;
        meta.announce = parse_announce(&root);
        meta.webseeds = parse_url_list(&root);
        meta.comment = bencode::dict_string(&root, b"comment");
        meta.created_by = bencode::dict_string(&root, b"created by");
        Ok(meta)
    }

    /// Build from a bare info dictionary, as recovered over BEP 9. Trackers
    /// and webseeds are not in there; take those from the magnet.
    pub fn from_info_dict(raw_info: &[u8]) -> Result<Self> {
        let info_hash = InfoHash::new(Sha1::digest(raw_info).into());

        let info = match serde_bencode::from_bytes::<Bencode>(raw_info) {
            Ok(Bencode::Dict(dict)) => dict,
            Ok(_) => return Err(Error::Metainfo("info is not a dictionary".into())),
            Err(err) => return Err(Error::Metainfo(format!("bencode: {err}"))),
        };

        let name = bencode::dict_string(&info, b"name")
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Error::Metainfo("info dict has no name".into()))?;
        if name.contains('/') || name == ".." {
            return Err(Error::Metainfo(format!("unsafe torrent name: {name:?}")));
        }

        let piece_length = bencode::dict_int(&info, b"piece length")
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::Metainfo("info dict has no piece length".into()))?
            as u64;

        let pieces = bencode::dict_bytes(&info, b"pieces")
            .ok_or_else(|| Error::Metainfo("info dict has no pieces".into()))?
            .to_vec();
        if pieces.is_empty() || pieces.len() % 20 != 0 {
            return Err(Error::Metainfo(format!(
                "piece hash blob is {} bytes, not a multiple of 20",
                pieces.len()
            )));
        }

        let files = parse_files(&info, &name)?;
        let total_length: u64 = files.iter().map(|f| f.length).sum();

        let expected = total_length.div_ceil(piece_length);
        if expected != (pieces.len() / 20) as u64 {
            return Err(Error::Metainfo(format!(
                "{} piece hashes for {total_length} bytes at {piece_length} per piece (expected {expected})",
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
            announce: Vec::new(),
            webseeds: Vec::new(),
            private: bencode::dict_int(&info, b"private") == Some(1),
            comment: None,
            created_by: None,
            raw_info: raw_info.to_vec(),
        })
    }

    /// Build from an info dict recovered over BEP 9, checking it against the
    /// infohash we asked for. **This is the security boundary for magnets**: a
    /// peer can return any bytes it likes, and only this comparison stops it
    /// dictating our file layout and piece hashes.
    pub fn from_verified_info_dict(raw_info: &[u8], expected: InfoHash) -> Result<Self> {
        let actual = InfoHash::new(Sha1::digest(raw_info).into());
        if actual != expected {
            return Err(Error::MetadataMismatch {
                peer: format!("expected {expected}, got {actual}"),
            });
        }
        Self::from_info_dict(raw_info)
    }

    /// Fill in discovery hints from the magnet the info dict was fetched for.
    pub fn apply_magnet(&mut self, magnet: &Magnet) {
        for tracker in &magnet.trackers {
            if !self.announce.contains(tracker) {
                self.announce.push(tracker.clone());
            }
        }
        for webseed in &magnet.webseeds {
            let webseed = upgrade_scheme(webseed);
            if !self.webseeds.contains(&webseed) {
                self.webseeds.push(webseed);
            }
        }
    }

    pub fn info_hash_hex(&self) -> String {
        self.info_hash.to_hex()
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

    /// Which files a piece lands in, and where. A piece can straddle any
    /// number of file boundaries, which is the classic off-by-one in this
    /// protocol.
    pub fn piece_slices(&self, index: usize) -> Vec<FileSlice> {
        match self.piece_size(index) {
            Some(size) => self.byte_slices(index as u64 * self.piece_length, size),
            None => Vec::new(),
        }
    }

    /// The same mapping for an arbitrary byte span of the concatenated piece
    /// space, which is what serving an HTTP range request needs.
    ///
    /// The span is clamped to the torrent, so a reader asking for more than
    /// there is gets what exists rather than an error.
    pub fn byte_slices(&self, start: u64, len: u64) -> Vec<FileSlice> {
        let start = start.min(self.total_length);
        let end = start.saturating_add(len).min(self.total_length);

        let mut slices = Vec::new();
        for (file_index, file) in self.files.iter().enumerate() {
            if file.end() <= start || file.offset >= end {
                continue;
            }
            let from = start.max(file.offset);
            let to = end.min(file.end());
            slices.push(FileSlice {
                file_index,
                file_offset: from - file.offset,
                offset_in_span: from - start,
                length: to - from,
            });
        }
        slices
    }

    /// The pieces covering a byte span, as a half-open range of indices.
    ///
    /// Empty when the span is empty. This is the bridge between "the browser
    /// asked for bytes 40 MB to 44 MB" and "fetch pieces 312 through 344".
    pub fn pieces_for_span(&self, start: u64, len: u64) -> std::ops::Range<usize> {
        if len == 0 || start >= self.total_length {
            return 0..0;
        }
        let end = start.saturating_add(len).min(self.total_length);
        let first = (start / self.piece_length) as usize;
        // `end` is exclusive, so the last byte we want is `end - 1`.
        let last = ((end - 1) / self.piece_length) as usize;
        first..(last + 1).min(self.piece_count())
    }

    /// A magnet link carrying everything a client needs to find this swarm.
    pub fn magnet(&self) -> Magnet {
        Magnet {
            info_hash: self.info_hash,
            display_name: Some(self.name.clone()),
            trackers: self.announce.clone(),
            webseeds: self.webseeds.clone(),
            peers: Vec::new(),
            v2_multihash: None,
        }
    }

    pub fn magnet_uri(&self) -> String {
        self.magnet().to_uri()
    }

    /// Serialise back to a `.torrent` file.
    ///
    /// The info dictionary is spliced in exactly as it arrived rather than
    /// re-encoded, because re-encoding drops keys we did not understand and
    /// changes the infohash, which would make the file describe a different
    /// torrent from the one it sits next to.
    ///
    /// The point of writing one at all is that a directory of downloaded bytes
    /// is otherwise anonymous: without the file list and the piece hashes there
    /// is no way to say what is in it, and balerion used to have to go back to
    /// the swarm to find out something it already knew. Any other client can
    /// read the result, which is the test of whether it is really a torrent
    /// file or merely our own notes.
    pub fn to_torrent_bytes(&self) -> Vec<u8> {
        fn bstr(out: &mut Vec<u8>, value: &[u8]) {
            out.extend_from_slice(format!("{}:", value.len()).as_bytes());
            out.extend_from_slice(value);
        }

        let mut out = vec![b'd'];
        // Keys in sorted byte order, as bencode requires:
        // announce, announce-list, info, url-list.
        if let Some(first) = self.announce.first() {
            bstr(&mut out, b"announce");
            bstr(&mut out, first.as_bytes());
        }
        if !self.announce.is_empty() {
            bstr(&mut out, b"announce-list");
            out.push(b'l');
            for tracker in &self.announce {
                // Each tier is a list. One tracker per tier keeps the order we
                // were given, which is the order they were tried in.
                out.push(b'l');
                bstr(&mut out, tracker.as_bytes());
                out.push(b'e');
            }
            out.push(b'e');
        }
        bstr(&mut out, b"info");
        out.extend_from_slice(&self.raw_info);
        if !self.webseeds.is_empty() {
            bstr(&mut out, b"url-list");
            out.push(b'l');
            for url in &self.webseeds {
                bstr(&mut out, url.as_bytes());
            }
            out.push(b'e');
        }
        out.push(b'e');
        out
    }
}

fn parse_files(info: &Dict, name: &str) -> Result<Vec<TorrentFile>> {
    match info.get(b"files".as_slice()) {
        // Multi-file: paths are relative to the info `name` directory.
        Some(Bencode::List(entries)) => {
            let mut files = Vec::with_capacity(entries.len());
            let mut offset = 0u64;
            for entry in entries {
                let Bencode::Dict(entry) = entry else {
                    return Err(Error::Metainfo("files entry is not a dictionary".into()));
                };
                let length = bencode::dict_int(entry, b"length")
                    .filter(|n| *n >= 0)
                    .ok_or_else(|| Error::Metainfo("files entry has no length".into()))?
                    as u64;
                let Some(Bencode::List(segments)) = entry.get(b"path".as_slice()) else {
                    return Err(Error::Metainfo("files entry has no path".into()));
                };
                let mut path = String::from(name);
                for segment in segments {
                    let Bencode::Bytes(segment) = segment else {
                        return Err(Error::Metainfo("path segment is not a string".into()));
                    };
                    let segment = String::from_utf8_lossy(segment);
                    // Refuse to be talked out of the download directory.
                    if segment.is_empty()
                        || segment == "."
                        || segment == ".."
                        || segment.contains('/')
                        || segment.contains('\\')
                    {
                        return Err(Error::Metainfo(format!("unsafe path segment: {segment:?}")));
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
                return Err(Error::Metainfo("multi-file torrent lists no files".into()));
            }
            Ok(files)
        }
        // Single-file: the info `name` is the file.
        _ => {
            let length = bencode::dict_int(info, b"length")
                .filter(|n| *n >= 0)
                .ok_or_else(|| Error::Metainfo("info dict has neither files nor length".into()))?
                as u64;
            Ok(vec![TorrentFile {
                path: name.to_string(),
                length,
                offset: 0,
            }])
        }
    }
}

fn parse_announce(root: &Dict) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |url: String| {
        if !url.is_empty() && !out.contains(&url) {
            out.push(url);
        }
    };
    if let Some(url) = bencode::dict_string(root, b"announce") {
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
/// several.
fn parse_url_list(root: &Dict) -> Vec<String> {
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

/// archive.org writes `http://` webseed URLs for hosts that speak HTTPS.
fn upgrade_scheme(url: &str) -> String {
    match url.strip_prefix("http://") {
        Some(rest) if rest.contains("archive.org") => format!("https://{rest}"),
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn multi_file_torrent() -> Vec<u8> {
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
        raw
    }

    #[test]
    fn info_hash_is_sha1_of_the_raw_info_dict() {
        let (raw, info) = single_file_torrent();
        let meta = Metainfo::parse(&raw).unwrap();
        let expected: [u8; 20] = Sha1::digest(&info).into();
        assert_eq!(meta.info_hash.as_bytes(), &expected);
        assert_eq!(meta.raw_info, info);
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
        assert_eq!(meta.comment.as_deref(), Some("a test item"));
        assert_eq!(meta.announce, IA_TRACKERS);
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
    fn webseeds_are_upgraded_to_https() {
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
    fn a_bare_info_dict_gives_the_same_torrent() {
        let (raw, info) = single_file_torrent();
        let from_file = Metainfo::parse(&raw).unwrap();
        let from_dict = Metainfo::from_info_dict(&info).unwrap();

        assert_eq!(from_file.info_hash, from_dict.info_hash);
        assert_eq!(from_file.files, from_dict.files);
        assert_eq!(from_file.pieces, from_dict.pieces);
        // Discovery hints only exist in the outer dict.
        assert!(from_dict.announce.is_empty());
        assert!(from_dict.webseeds.is_empty());
    }

    #[test]
    fn verified_construction_rejects_a_lying_peer() {
        let (_, info) = single_file_torrent();
        let real = Metainfo::from_info_dict(&info).unwrap().info_hash;
        assert!(Metainfo::from_verified_info_dict(&info, real).is_ok());

        let wrong = InfoHash::new([0u8; 20]);
        let err = Metainfo::from_verified_info_dict(&info, wrong).unwrap_err();
        assert!(matches!(err, Error::MetadataMismatch { .. }));

        // Tampering with the dict must also be caught.
        let mut tampered = info.clone();
        let pos = tampered
            .windows(10)
            .position(|w| w == b"xfetch.pdf")
            .unwrap();
        tampered[pos] = b'y';
        assert!(Metainfo::from_verified_info_dict(&tampered, real).is_err());
    }

    #[test]
    fn magnet_hints_are_folded_in() {
        let (_, info) = single_file_torrent();
        let mut meta = Metainfo::from_info_dict(&info).unwrap();
        let magnet = Magnet::parse(&format!(
            "magnet:?xt=urn:btih:{}&tr=udp%3A%2F%2Ft.example%3A80&ws=http%3A%2F%2Farchive.org%2Fdownload%2F",
            meta.info_hash
        ))
        .unwrap();
        meta.apply_magnet(&magnet);

        assert_eq!(meta.announce, vec!["udp://t.example:80"]);
        assert_eq!(meta.webseeds, vec!["https://archive.org/download/"]);
    }

    #[test]
    fn maps_pieces_across_file_boundaries() {
        let meta = Metainfo::parse(&multi_file_torrent()).unwrap();
        assert_eq!(meta.total_length, 1000);
        assert_eq!(meta.piece_length, 512);

        // Piece 0 is wholly inside the first file.
        assert_eq!(
            meta.piece_slices(0),
            vec![FileSlice {
                file_index: 0,
                file_offset: 0,
                offset_in_span: 0,
                length: 512
            }]
        );
        // Piece 1 straddles the boundary at 600 and is short (488 bytes).
        assert_eq!(
            meta.piece_slices(1),
            vec![
                FileSlice {
                    file_index: 0,
                    file_offset: 512,
                    offset_in_span: 0,
                    length: 88
                },
                FileSlice {
                    file_index: 1,
                    file_offset: 0,
                    offset_in_span: 88,
                    length: 400
                },
            ]
        );
        assert!(meta.piece_slices(2).is_empty());
    }

    #[test]
    fn maps_an_arbitrary_byte_span_across_file_boundaries() {
        let meta = Metainfo::parse(&multi_file_torrent()).unwrap();

        // A span sitting entirely inside the second file, which starts at 600.
        assert_eq!(
            meta.byte_slices(700, 50),
            vec![FileSlice {
                file_index: 1,
                file_offset: 100,
                offset_in_span: 0,
                length: 50
            }]
        );
        // And one crossing the boundary, which is where this goes wrong.
        assert_eq!(
            meta.byte_slices(580, 40),
            vec![
                FileSlice {
                    file_index: 0,
                    file_offset: 580,
                    offset_in_span: 0,
                    length: 20
                },
                FileSlice {
                    file_index: 1,
                    file_offset: 0,
                    offset_in_span: 20,
                    length: 20
                },
            ]
        );
    }

    #[test]
    fn byte_spans_are_clamped_rather_than_refused() {
        let meta = Metainfo::parse(&multi_file_torrent()).unwrap();

        // Asking past the end yields what exists. A player probing with
        // `Range: bytes=0-` asks for far more than the file holds.
        let slices = meta.byte_slices(900, 10_000);
        assert_eq!(slices.iter().map(|s| s.length).sum::<u64>(), 100);
        assert!(meta.byte_slices(1000, 10).is_empty());
        assert!(meta.byte_slices(0, 0).is_empty());
    }

    #[test]
    fn spans_map_to_the_pieces_that_cover_them() {
        let meta = Metainfo::parse(&multi_file_torrent()).unwrap();
        assert_eq!(meta.piece_length, 512);

        assert_eq!(meta.pieces_for_span(0, 1), 0..1);
        // A span ending exactly on the boundary must not claim the next piece.
        assert_eq!(meta.pieces_for_span(0, 512), 0..1);
        assert_eq!(meta.pieces_for_span(0, 513), 0..2);
        assert_eq!(meta.pieces_for_span(511, 2), 0..2);
        assert_eq!(meta.pieces_for_span(512, 10), 1..2);
        // Past the end, and empty spans, produce nothing to fetch.
        assert!(meta.pieces_for_span(0, 0).is_empty());
        assert!(meta.pieces_for_span(1000, 10).is_empty());
        // Overlong spans stop at the last real piece.
        assert_eq!(meta.pieces_for_span(0, u64::MAX), 0..2);
    }

    #[test]
    fn slices_cover_every_byte_exactly_once() {
        let meta = Metainfo::parse(&multi_file_torrent()).unwrap();
        let mut covered = 0u64;
        for index in 0..meta.piece_count() {
            let slices = meta.piece_slices(index);
            let piece_bytes: u64 = slices.iter().map(|s| s.length).sum();
            assert_eq!(piece_bytes, meta.piece_size(index).unwrap());
            covered += piece_bytes;
        }
        assert_eq!(covered, meta.total_length);
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

        let err = Metainfo::from_info_dict(&info).unwrap_err();
        assert!(matches!(err, Error::Metainfo(msg) if msg.contains("unsafe path segment")));
    }

    #[test]
    fn rejects_an_absolute_torrent_name() {
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i10e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"/etc/passwd"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i512e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&[0u8; 20]));
        info.extend(b"e");
        assert!(Metainfo::from_info_dict(&info).is_err());
    }

    #[test]
    fn rejects_piece_count_mismatch_and_truncation() {
        assert!(Metainfo::parse(b"").is_err());
        assert!(Metainfo::parse(b"d4:infod").is_err());
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i2000e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"x"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i1024e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&[0u8; 20])); // one hash for a two-piece torrent
        info.extend(b"e");
        assert!(Metainfo::from_info_dict(&info).is_err());
    }

    #[test]
    fn round_trips_to_a_magnet() {
        let (raw, _) = single_file_torrent();
        let meta = Metainfo::parse(&raw).unwrap();
        let magnet = Magnet::parse(&meta.magnet_uri()).unwrap();
        assert_eq!(magnet.info_hash, meta.info_hash);
        assert_eq!(magnet.display_name.as_deref(), Some("xfetch.pdf"));
        assert_eq!(magnet.trackers, IA_TRACKERS);
    }

    #[test]
    fn a_torrent_survives_being_written_out_and_read_back() {
        // This is what lets a directory of bytes on disk say what it is
        // without going back to the swarm to ask.
        let (raw, _) = single_file_torrent();
        let original = Metainfo::parse(&raw).unwrap();

        let written = original.to_torrent_bytes();
        let reread = Metainfo::parse(&written).expect("what we wrote is a torrent file");

        // The infohash is the thing that must not move. Everything else is
        // convenience; this is identity.
        assert_eq!(reread.info_hash, original.info_hash);
        assert_eq!(reread.raw_info, original.raw_info);
        assert_eq!(reread.name, original.name);
        assert_eq!(reread.total_length, original.total_length);
        assert_eq!(reread.piece_length, original.piece_length);
        assert_eq!(reread.pieces, original.pieces);
        assert_eq!(reread.files.len(), original.files.len());
        assert_eq!(reread.announce, original.announce);
        assert_eq!(reread.webseeds, original.webseeds);
    }

    #[test]
    fn a_torrent_with_no_trackers_or_webseeds_still_round_trips() {
        // Everything resolved from a bare magnet looks like this, and an empty
        // bencode list where a key should be absent is a malformed file.
        let (_, info) = single_file_torrent();
        let bare = Metainfo::from_info_dict(&info).unwrap();
        assert!(bare.announce.is_empty() && bare.webseeds.is_empty());

        let written = bare.to_torrent_bytes();
        assert!(
            !written.windows(8).any(|w| w == b"announce"),
            "no empty key"
        );
        let reread = Metainfo::parse(&written).expect("still a torrent file");
        assert_eq!(reread.info_hash, bare.info_hash);
        assert_eq!(reread.files.len(), bare.files.len());
    }
}
