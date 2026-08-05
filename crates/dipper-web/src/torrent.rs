//! Turning a magnet into a running session.
//!
//! The orchestration differs from the CLI's (no spinners, and the answer goes
//! into a JSON response rather than onto a terminal) but the actual work is
//! the same `dipper_bt` primitives underneath: discovery, then BEP 9.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use dipper_bt::discovery::{Discovery, DiscoveryConfig};
use dipper_bt::infohash::generate_peer_id;
use dipper_bt::session::{DownloadConfig, VerifyPolicy};
use dipper_bt::{Dht, Magnet, Metainfo, Strategy, peer, session};
use dipper_ia::{IaClient, metadata, torrent as ia_torrent};

use crate::state::{ServeConfig, Torrent};

/// How many peers to try for the metadata before giving up.
const METADATA_PEERS: usize = 8;

/// What the user gave us, resolved as far as it can be without the swarm.
#[derive(Debug)]
pub enum Input {
    /// An infohash and some hints. The file list has to come from a peer.
    Magnet(Box<Magnet>),
    /// A whole torrent, so we already know the file list and piece hashes.
    Complete(Box<Metainfo>),
}

impl Input {
    pub fn magnet(&self) -> Magnet {
        match self {
            Input::Magnet(magnet) => (**magnet).clone(),
            Input::Complete(meta) => meta.magnet(),
        }
    }
}

/// Parse whatever the user pasted.
///
/// A bare 40-character hex infohash is accepted, because people paste those
/// and being told off for it helps nobody. So is an archive.org identifier,
/// which matters more than it looks: archive.org's trackers refuse third-party
/// seeding, so its swarms have no peers to ask for a file list. Fetching its
/// derived `.torrent` over HTTPS sidesteps the problem entirely, and without
/// this path every public domain film on the Archive fails to resolve.
pub async fn parse_input(input: &str, ia: &IaClient) -> Result<Input> {
    let input = input.trim();
    if input.starts_with("magnet:") {
        let magnet = Magnet::parse(input).context("that does not parse as a magnet link")?;
        return Ok(Input::Magnet(Box::new(magnet)));
    }

    let looks_like_a_hash = (input.len() == 40 && input.chars().all(|c| c.is_ascii_hexdigit()))
        || (input.len() == 32 && input.chars().all(|c| c.is_ascii_alphanumeric()));
    if looks_like_a_hash {
        let trackers: String = DEFAULT_TRACKERS
            .iter()
            .map(|url| format!("&tr={}", urlencode(url)))
            .collect();
        let magnet = Magnet::parse(&format!("magnet:?xt=urn:btih:{input}{trackers}"))
            .context("that is not a valid infohash")?;
        return Ok(Input::Magnet(Box::new(magnet)));
    }

    if input.is_empty() || input.contains(' ') || input.contains('/') {
        bail!("paste a magnet link, an infohash, or an archive.org identifier")
    }

    let item = metadata::fetch(ia, input).await.with_context(|| {
        format!("{input} is not a magnet or an infohash, and archive.org has no such item")
    })?;
    let meta = ia_torrent::fetch(ia, &item)
        .await
        .with_context(|| format!("archive.org has no derived torrent for {input}"))?;
    Ok(Input::Complete(Box::new(meta)))
}

/// Trackers used only when the user supplied a bare infohash with no hints of
/// its own. Open trackers, so nothing here assumes a private swarm.
const DEFAULT_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://open.demonii.com:1337/announce",
    "udp://tracker.torrent.eu.org:451/announce",
];

fn urlencode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Find peers, and the torrent metadata if we do not have it yet.
pub async fn resolve(input: &Input, config: &ServeConfig) -> Result<(Metainfo, Vec<SocketAddr>)> {
    let magnet = &input.magnet();
    let dht = if config.use_dht {
        match Dht::client() {
            Ok(dht) => Some(dht),
            Err(err) => {
                tracing::warn!(%err, "could not start the DHT; carrying on without it");
                None
            }
        }
    } else {
        None
    };

    let discovery = Discovery::new(magnet.info_hash);
    discovery
        .run(
            magnet,
            generate_peer_id(),
            0,
            dht.as_ref(),
            &DiscoveryConfig {
                tracker_timeout: config.tracker_timeout,
                dht_budget: config.dht_budget,
                use_dht: config.use_dht,
                port: config.peer_port,
            },
            |source, peers| tracing::debug!(?source, count = peers.len(), "peers found"),
        )
        .await;

    let peers = discovery.peers();

    let mut meta = match input {
        // We already have the file list, so no peer is needed. This is the
        // path every archive.org item takes, and it is why they work at all.
        Input::Complete(meta) => (**meta).clone(),
        Input::Magnet(_) => {
            if peers.is_empty() {
                bail!(
                    "no peers found. Trackers and the DHT both came up empty, so the swarm \
                     may be dead or your network may be blocking outbound BitTorrent"
                );
            }
            // What comes back is trustworthy solely because
            // `from_verified_info_dict` hashes it against the infohash we asked
            // for. A peer can otherwise choose our file layout for us.
            let (meta, from) = peer::fetch_metadata_from_peers(
                &peers,
                magnet.info_hash,
                generate_peer_id(),
                config.peer_port,
                METADATA_PEERS,
            )
            .await
            .context("no peer would serve the torrent metadata")?;
            tracing::debug!(from = %from, name = meta.name, "metadata recovered");
            meta
        }
    };

    meta.apply_magnet(magnet);
    if !config.use_webseeds {
        meta.webseeds.clear();
    }
    if peers.is_empty() && meta.webseeds.is_empty() {
        bail!("nothing to download from: no peers answered and the torrent has no webseeds");
    }
    Ok((meta, peers))
}

/// Start downloading, in streaming mode, and register the result.
pub async fn start(
    meta: Metainfo,
    peers: Vec<SocketAddr>,
    root: &Path,
    config: &ServeConfig,
) -> Result<Arc<Torrent>> {
    let download = DownloadConfig {
        max_peers: config.max_peers,
        pipeline_depth: config.pipeline_depth,
        port: config.peer_port,
        verify: VerifyPolicy::Auto,
        // The whole point: fetch what the player is waiting on, not what the
        // swarm would prefer we fetched.
        strategy: Strategy::Streaming,
        ..Default::default()
    };

    let (handle, task) = session::spawn(&meta, root, peers, &download)
        .await
        .context("could not start the download")?;

    Ok(Arc::new(Torrent::new(
        meta,
        handle,
        task,
        root.to_path_buf(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "15c74d4165fc2ffff997d576bf44b4b25cbeb04e";

    /// Parse without the archive.org fallback, which needs the network.
    async fn parse_offline(input: &str) -> Result<Input> {
        parse_input(input, &IaClient::new().unwrap()).await
    }

    #[tokio::test]
    async fn a_magnet_link_parses() {
        let input = parse_offline(&format!("magnet:?xt=urn:btih:{HASH}&dn=nasa"))
            .await
            .unwrap();
        let magnet = input.magnet();
        assert_eq!(magnet.info_hash.to_hex(), HASH);
        assert_eq!(magnet.display_name.as_deref(), Some("nasa"));
        assert!(matches!(input, Input::Magnet(_)));
    }

    #[tokio::test]
    async fn a_bare_infohash_is_accepted_and_given_trackers() {
        // People paste these. Refusing would be technically defensible and
        // practically annoying.
        let magnet = parse_offline(HASH).await.unwrap().magnet();
        assert_eq!(magnet.info_hash.to_hex(), HASH);
        assert_eq!(
            magnet.trackers.len(),
            DEFAULT_TRACKERS.len(),
            "an infohash alone gives no way to find the swarm"
        );
    }

    #[tokio::test]
    async fn surrounding_whitespace_is_forgiven() {
        assert!(parse_offline(&format!("  {HASH}\n")).await.is_ok());
    }

    #[tokio::test]
    async fn things_that_cannot_be_an_identifier_are_refused_without_a_round_trip() {
        // Anything with a space or a slash is not an archive.org identifier,
        // so it must fail here rather than costing an API call to find out.
        let err = parse_offline("have a go at this").await.unwrap_err();
        assert!(format!("{err}").contains("magnet link"), "{err}");
        assert!(parse_offline("").await.is_err());
        assert!(parse_offline("some/path.torrent").await.is_err());
    }

    #[tokio::test]
    async fn tracker_urls_survive_being_encoded_into_a_magnet() {
        let magnet = parse_offline(HASH).await.unwrap().magnet();
        assert!(
            magnet
                .trackers
                .iter()
                .any(|url| url.contains("opentrackr.org:1337")),
            "colons and slashes must round trip: {:?}",
            magnet.trackers
        );
    }
}
