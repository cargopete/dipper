//! Turning a magnet into a running session.
//!
//! The orchestration differs from the CLI's (no spinners, and the answer goes
//! into a JSON response rather than onto a terminal) but the actual work is
//! the same `balerin_bt` primitives underneath: discovery, then BEP 9.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use balerin_bt::discovery::{Discovery, DiscoveryConfig};
use balerin_bt::infohash::generate_peer_id;
use balerin_bt::session::{DownloadConfig, VerifyPolicy};
use balerin_bt::{Dht, Magnet, Metainfo, Strategy, peer, session};
use balerin_ia::{IaClient, metadata, torrent as ia_torrent};
use tokio::task::JoinHandle;

use crate::state::{ServeConfig, Torrent};

/// How many peers to try for the metadata before giving up.
const METADATA_PEERS: usize = 8;

/// How often to go back to the trackers for more peers.
///
/// Not faster. Trackers ask for an announce every half hour or so and are
/// entitled to be annoyed at less; five minutes is already generous of them,
/// and the loop skips the request entirely whenever the swarm is healthy.
const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long the engine waits for more peers before it gives up.
///
/// Must be comfortably more than [`REANNOUNCE_INTERVAL`], or the session ends
/// in the gap between two announcements and takes the player down with it.
const PEER_REFILL_GRACE: Duration = Duration::from_secs(11 * 60);

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
        // This caller does re-announce, so the engine should wait to be fed
        // rather than concluding the swarm is dead the moment its list runs dry.
        peer_refill_grace: PEER_REFILL_GRACE,
        // The whole point: fetch what the player is waiting on, not what the
        // swarm would prefer we fetched.
        strategy: Strategy::Streaming,
        ..Default::default()
    };

    let (handle, task) = session::spawn(&meta, root, peers, &download)
        .await
        .context("could not start the download")?;

    let refill = spawn_refill(&meta, handle.clone(), config);

    Ok(Arc::new(Torrent::new(
        meta,
        handle,
        task,
        refill,
        root.to_path_buf(),
    )))
}

/// Keep going back to the trackers for peers, for as long as the download needs
/// them.
///
/// The engine deliberately knows nothing about trackers: it is handed addresses
/// and asked for pieces. This is the other half of that bargain. Without it the
/// engine works through every address discovery found at the start and then
/// runs on whatever survived, which on a public swarm is usually one or two of
/// the sixty a tracker named.
///
/// Trackers only, no DHT: a second DHT would want the same UDP port as the one
/// already running, and the trackers are where the peers actually came from.
fn spawn_refill(
    meta: &Metainfo,
    handle: balerin_bt::SessionHandle,
    config: &ServeConfig,
) -> JoinHandle<()> {
    // The Archive is the case this must not touch. Its items arrive with a
    // webseed that serves the whole file over HTTPS, and its trackers refuse
    // third-party seeding, so there is no peer to find and asking every five
    // minutes is noise on someone else's server for nothing.
    if !meta.webseeds.is_empty() {
        return tokio::spawn(async {});
    }

    let magnet = meta.magnet();
    let total_length = meta.total_length;
    let max_peers = config.max_peers;
    let discovery_config = DiscoveryConfig {
        tracker_timeout: config.tracker_timeout,
        dht_budget: Duration::ZERO,
        use_dht: false,
        port: config.peer_port,
    };

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(REANNOUNCE_INTERVAL);
        // The first tick is immediate and the list is seconds old at that
        // point, so it is spent rather than acted on.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let stats = handle.stats();
            if stats.is_complete() {
                return;
            }
            // A full complement of peers is not a problem in want of solving.
            if stats.peers_connected >= max_peers {
                continue;
            }

            let discovery = Discovery::new(magnet.info_hash);
            // Announced honestly: trackers use `left` to tell seeders from
            // leechers, and claiming zero would have us listed as a seeder we
            // are not.
            let left = total_length.saturating_sub(stats.bytes_on_disk);
            discovery
                .run(
                    &magnet,
                    generate_peer_id(),
                    left,
                    None,
                    &discovery_config,
                    |_, _| {},
                )
                .await;

            let found = discovery.peers();
            let fresh = handle.add_peers(found.iter().copied());
            tracing::debug!(
                connected = stats.peers_connected,
                found = found.len(),
                fresh,
                "re-announced for more peers"
            );
        }
    })
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
