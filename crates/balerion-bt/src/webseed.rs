//! HTTP webseeds, GetRight style (BEP 19).
//!
//! A webseed is a plain HTTP server holding the torrent's files. Pieces map
//! onto byte ranges, so a piece is one range request per file it touches, and
//! the bytes are verified against the same piece hashes as anything from a
//! peer. For institutional torrents (archive.org above all) this is not a
//! fallback, it is where essentially every byte comes from.

use std::time::Duration;

use crate::error::{Error, Result};
use crate::metainfo::Metainfo;

/// A webseed root from the torrent's `url-list`.
#[derive(Debug, Clone)]
pub struct Webseed {
    base: String,
    client: reqwest::Client,
}

impl Webseed {
    pub fn new(base: impl Into<String>, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("balerion/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            base: base.into(),
            client,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// URL for one file of the torrent.
    ///
    /// BEP 19: a `url-list` entry ending in `/` is a directory to append the
    /// torrent's path to; anything else is the file itself, which only makes
    /// sense for single-file torrents. Our file paths already carry the
    /// torrent name as their first segment, which is exactly what the spec
    /// wants appended.
    pub fn file_url(&self, meta: &Metainfo, file_index: usize) -> Result<String> {
        let file = meta
            .files
            .get(file_index)
            .ok_or_else(|| Error::Metainfo(format!("file {file_index} is out of range")))?;

        if self.base.ends_with('/') {
            Ok(format!("{}{}", self.base, encode_path(&file.path)))
        } else if meta.files.len() == 1 {
            Ok(self.base.clone())
        } else {
            Err(Error::Peer(format!(
                "webseed {} is a single-file URL but this torrent has {} files",
                self.base,
                meta.files.len()
            )))
        }
    }

    /// Fetch one piece, stitched together from however many files it spans.
    ///
    /// The caller still has to verify it: a webseed is no more trustworthy
    /// than a peer, it is just usually faster.
    pub async fn fetch_piece(&self, meta: &Metainfo, index: usize) -> Result<Vec<u8>> {
        let size = meta
            .piece_size(index)
            .ok_or_else(|| Error::Metainfo(format!("piece {index} is out of range")))?;
        let mut out = Vec::with_capacity(size as usize);

        for slice in meta.piece_slices(index) {
            let url = self.file_url(meta, slice.file_index)?;
            let file_length = meta.files[slice.file_index].length;
            let last = slice.file_offset + slice.length - 1;
            let response = self
                .client
                .get(&url)
                .header("Range", format!("bytes={}-{last}", slice.file_offset))
                .send()
                .await?;

            let status = response.status();
            if !status.is_success() {
                return Err(Error::Peer(format!(
                    "webseed {url} answered {status} for a range request"
                )));
            }
            let body = response.bytes().await?;

            // 206 means the server honoured the range. 200 means it sent the
            // whole file instead, which plenty of servers do (archive.org
            // does it whenever the range happens to cover everything), so we
            // slice it ourselves rather than splicing the lot into the piece.
            let wanted = match status.as_u16() {
                206 => body,
                200 if body.len() as u64 == file_length => {
                    let from = slice.file_offset as usize;
                    let to = from + slice.length as usize;
                    body.slice(from..to)
                }
                _ => {
                    return Err(Error::Peer(format!(
                        "webseed {url} answered {status} with {} bytes for a {}-byte range of a {file_length}-byte file",
                        body.len(),
                        slice.length
                    )));
                }
            };

            if wanted.len() as u64 != slice.length {
                return Err(Error::Peer(format!(
                    "webseed {url} returned {} bytes, expected {}",
                    wanted.len(),
                    slice.length
                )));
            }
            out.extend_from_slice(&wanted);
        }

        if out.len() as u64 != size {
            return Err(Error::Peer(format!(
                "webseed assembled {} bytes for piece {index}, expected {size}",
                out.len()
            )));
        }
        Ok(out)
    }
}

/// Percent-encode a path, leaving the separators alone.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| urlencoding::encode(segment).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::{Digest, Sha1};

    fn bstr(s: &[u8]) -> Vec<u8> {
        let mut out = format!("{}:", s.len()).into_bytes();
        out.extend_from_slice(s);
        out
    }

    fn multi_file_meta() -> Metainfo {
        let content: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let mut hashes = Vec::new();
        for chunk in content.chunks(512) {
            hashes.extend_from_slice(&Sha1::digest(chunk));
        }

        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"files"));
        info.extend(b"l");
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i600e");
        info.extend(bstr(b"path"));
        info.extend(b"l");
        info.extend(bstr(b"one two.bin"));
        info.extend(b"e");
        info.extend(b"e");
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i400e");
        info.extend(bstr(b"path"));
        info.extend(b"l");
        info.extend(bstr(b"two.bin"));
        info.extend(b"e");
        info.extend(b"e");
        info.extend(b"e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"an-item"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i512e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&hashes));
        info.extend(b"e");
        Metainfo::from_info_dict(&info).unwrap()
    }

    fn single_file_meta() -> Metainfo {
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i1000e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"solo.bin"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i512e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&[0u8; 40]));
        info.extend(b"e");
        Metainfo::from_info_dict(&info).unwrap()
    }

    #[test]
    fn directory_webseeds_get_the_torrent_path_appended() {
        let seed = Webseed::new("https://archive.org/download/", Duration::from_secs(5)).unwrap();
        let meta = multi_file_meta();
        assert_eq!(
            seed.file_url(&meta, 0).unwrap(),
            "https://archive.org/download/an-item/one%20two.bin",
            "paths are percent-encoded, separators are not"
        );
        assert_eq!(
            seed.file_url(&meta, 1).unwrap(),
            "https://archive.org/download/an-item/two.bin"
        );
    }

    #[test]
    fn a_single_file_url_is_used_as_is() {
        let seed = Webseed::new("https://example.com/solo.bin", Duration::from_secs(5)).unwrap();
        assert_eq!(
            seed.file_url(&single_file_meta(), 0).unwrap(),
            "https://example.com/solo.bin"
        );
    }

    #[test]
    fn a_single_file_url_cannot_serve_a_multi_file_torrent() {
        let seed = Webseed::new("https://example.com/solo.bin", Duration::from_secs(5)).unwrap();
        assert!(seed.file_url(&multi_file_meta(), 0).is_err());
    }

    #[test]
    fn out_of_range_files_are_refused() {
        let seed = Webseed::new("https://example.com/", Duration::from_secs(5)).unwrap();
        assert!(seed.file_url(&multi_file_meta(), 99).is_err());
    }
}
