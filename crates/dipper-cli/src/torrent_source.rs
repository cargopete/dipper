//! Turning whatever the user typed into something the engine can download.
//!
//! Three shapes are accepted, because all three are things people have to
//! hand: a magnet URI, a path to a `.torrent`, and a bare archive.org
//! identifier (which we resolve through the metadata API).

use std::path::Path;

use anyhow::{Context, Result, bail};
use dipper_bt::{Magnet, Metainfo};
use dipper_ia::{IaClient, metadata, torrent};

/// What the user gave us, resolved as far as it can be without the network.
#[derive(Debug)]
pub enum Source {
    /// A full torrent: we know the piece hashes and file list already.
    Torrent(Box<Metainfo>),
    /// Just an infohash and some hints. The file list has to come from peers.
    Magnet(Box<Magnet>),
}

impl Source {
    pub fn magnet(&self) -> Magnet {
        match self {
            Source::Torrent(meta) => meta.magnet(),
            Source::Magnet(magnet) => (**magnet).clone(),
        }
    }

    pub fn metainfo(&self) -> Option<&Metainfo> {
        match self {
            Source::Torrent(meta) => Some(meta),
            Source::Magnet(_) => None,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Source::Torrent(meta) => format!("{} ({})", meta.name, meta.info_hash),
            Source::Magnet(magnet) => match &magnet.display_name {
                Some(name) => format!("{name} ({})", magnet.info_hash),
                None => magnet.info_hash.to_string(),
            },
        }
    }
}

/// Classify and load an argument.
///
/// A `.torrent` on disk beats everything; then a magnet URI; then, if it looks
/// like an archive.org identifier, we ask archive.org.
pub async fn resolve(input: &str, client: &IaClient) -> Result<Source> {
    if input.starts_with("magnet:") {
        let magnet = Magnet::parse(input).context("parsing magnet link")?;
        return Ok(Source::Magnet(Box::new(magnet)));
    }

    let path = Path::new(input);
    if path.exists() {
        let raw = std::fs::read(path).with_context(|| format!("reading {input}"))?;
        let meta = Metainfo::parse(&raw).with_context(|| format!("parsing {input}"))?;
        return Ok(Source::Torrent(Box::new(meta)));
    }

    if input.contains('/') || input.contains(' ') {
        bail!("{input} is not a magnet link, a .torrent file, or an archive.org identifier");
    }

    // Last resort: treat it as an archive.org identifier.
    let item = metadata::fetch(client, input).await.with_context(|| {
        format!("{input} is not a file or magnet, and archive.org has no such item")
    })?;
    let meta = torrent::fetch(client, &item)
        .await
        .with_context(|| format!("archive.org has no derived torrent for {input}"))?;
    Ok(Source::Torrent(Box::new(meta)))
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

    fn torrent_bytes() -> Vec<u8> {
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(b"i2000e");
        info.extend(bstr(b"name"));
        info.extend(bstr(b"a-file.bin"));
        info.extend(bstr(b"piece length"));
        info.extend(b"i1024e");
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&[0xAAu8; 40]));
        info.extend(b"e");

        let mut raw = Vec::new();
        raw.extend(b"d");
        raw.extend(bstr(b"announce"));
        raw.extend(bstr(b"udp://tracker.example:80"));
        raw.extend(bstr(b"info"));
        raw.extend(&info);
        raw.extend(b"e");
        raw
    }

    #[tokio::test]
    async fn reads_a_torrent_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thing.torrent");
        std::fs::write(&path, torrent_bytes()).unwrap();

        let client = IaClient::new().unwrap();
        let source = resolve(path.to_str().unwrap(), &client).await.unwrap();

        let meta = source.metainfo().expect("a torrent file has metadata");
        assert_eq!(meta.name, "a-file.bin");
        assert_eq!(meta.announce, vec!["udp://tracker.example:80"]);
        // And it can be handed straight back out as a magnet.
        assert_eq!(source.magnet().info_hash, meta.info_hash);
        assert!(source.describe().starts_with("a-file.bin ("));
    }

    #[tokio::test]
    async fn parses_a_magnet_without_touching_the_network() {
        let client = IaClient::new().unwrap();
        let source = resolve(
            "magnet:?xt=urn:btih:15c74d4165fc2ffff997d576bf44b4b25cbeb04e&dn=nasa",
            &client,
        )
        .await
        .unwrap();

        assert!(source.metainfo().is_none(), "a magnet has no file list yet");
        assert_eq!(
            source.describe(),
            "nasa (15c74d4165fc2ffff997d576bf44b4b25cbeb04e)"
        );
    }

    #[tokio::test]
    async fn rejects_things_that_are_neither() {
        let client = IaClient::new().unwrap();
        let err = resolve("./no/such/path.torrent", &client)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not a magnet link"), "{err}");
    }

    #[test]
    fn the_torrent_fixture_is_self_consistent() {
        // Guards the fixture itself: if this drifts, the tests above lie.
        let raw = torrent_bytes();
        let meta = Metainfo::parse(&raw).unwrap();
        let span = dipper_bt::bencode::info_span(&raw).unwrap();
        assert_eq!(
            meta.info_hash.as_bytes(),
            &<[u8; 20]>::from(Sha1::digest(&raw[span]))
        );
    }
}
