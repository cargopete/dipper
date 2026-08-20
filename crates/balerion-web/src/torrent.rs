//! Turning a magnet into a running session.
//!
//! The orchestration differs from the CLI's (no spinners, and the answer goes
//! into a JSON response rather than onto a terminal) but the actual work is
//! the same `balerion_bt` primitives underneath: discovery, then BEP 9.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use balerion_bt::discovery::{Discovery, DiscoveryConfig};
use balerion_bt::infohash::generate_peer_id;
use balerion_bt::session::{DownloadConfig, VerifyPolicy};
use balerion_bt::{Dht, Magnet, Metainfo, Strategy, peer, session};
use balerion_ia::{IaClient, metadata, torrent as ia_torrent};
use tokio::task::JoinHandle;

use crate::state::{Clock, ServeConfig, Torrent};

/// How many peers to ask for the metadata at once.
const METADATA_PEERS: usize = 8;

/// How long to keep trying for a file list before giving up on the swarm.
///
/// Discovery finishes in a few seconds and returns whatever it has; a magnet
/// resolved from that one snapshot fails if those particular peers happen to be
/// unreachable, which on a public swarm is most of them. The DHT keeps finding
/// more for as long as it is asked, so the useful thing to do with a failure is
/// look again rather than report it.
const METADATA_PATIENCE: Duration = Duration::from_secs(90);

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

    /* An archive.org item needs no peers whatsoever: it arrives with a webseed
     * that serves the whole file over HTTPS, and its trackers refuse
     * third-party seeding so there is nobody in the swarm to find. Running
     * discovery anyway meant waiting out the full DHT budget and every tracker
     * timeout before playing something whose file list we already had.
     *
     * Measured before this existed: fifty seconds to open a Popeye cartoon, of
     * which one was the metadata and forty-nine were spent looking for peers
     * that do not exist. `spawn_refill` already declined to announce for
     * webseeded torrents for the same reason; this is the other half of it. */
    if let Input::Complete(meta) = input
        && !meta.webseeds.is_empty()
        && config.use_webseeds
    {
        tracing::debug!(
            name = meta.name,
            webseeds = meta.webseeds.len(),
            "webseeded torrent; not looking for peers that are not there"
        );
        return Ok(((**meta).clone(), Vec::new()));
    }
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
            /* Ask, and if nobody answers, go and find more peers rather than
             * giving up on the swarm. The addresses a first sweep returns are
             * mostly unreachable on a public swarm: fake, or behind a NAT that
             * will not accept us. The one that will answer is frequently in the
             * next batch the DHT turns up.
             *
             * What comes back is trustworthy solely because
             * `from_verified_info_dict` hashes it against the infohash we asked
             * for. A peer can otherwise choose our file layout for us. */
            let started = std::time::Instant::now();
            let mut asked = 0usize;
            let mut last_error = None;
            let mut candidates = peers.clone();
            // Addresses the peers we spoke to introduced us to over BEP 11.
            // These are frequently better than anything a tracker returns,
            // because they come from something demonstrably in the swarm now.
            let mut introduced: Vec<SocketAddr> = Vec::new();

            loop {
                if !candidates.is_empty() {
                    asked += candidates.len();
                    match peer::fetch_metadata_collecting(
                        &candidates,
                        magnet.info_hash,
                        generate_peer_id(),
                        config.peer_port,
                        METADATA_PEERS,
                        &mut introduced,
                    )
                    .await
                    {
                        Ok((meta, from)) => {
                            tracing::debug!(from = %from, name = meta.name, "metadata recovered");
                            break meta;
                        }
                        Err(err) => last_error = Some(err),
                    }
                }

                if started.elapsed() >= METADATA_PATIENCE {
                    let err = last_error
                        .map(|err| err.to_string())
                        .unwrap_or_else(|| "no peers to ask".to_string());
                    bail!(
                        "no peer would serve the file list after {}s and {asked} peers: {err}. \
                         The swarm may have seeders that will send data but none that will \
                         answer a metadata request, which a different release of the same \
                         thing usually will",
                        started.elapsed().as_secs()
                    );
                }

                // Look again. Only addresses we have not already tried are worth
                // another connection.
                let before: std::collections::HashSet<SocketAddr> =
                    discovery.peers().into_iter().collect();
                // Peer exchange first: it costs nothing, it has already
                // happened, and on a swarm where the tracker's list is spent it
                // is the only source still producing anything.
                let from_pex = discovery.add(introduced.drain(..));
                if from_pex > 0 {
                    tracing::debug!(from_pex, "peer exchange named addresses we had not tried");
                }
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
                        |_, _| {},
                    )
                    .await;
                candidates = discovery
                    .peers()
                    .into_iter()
                    .filter(|addr| !before.contains(addr))
                    .collect();
                tracing::debug!(
                    fresh = candidates.len(),
                    "looking again for a peer with the file list"
                );
            }
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
    inbound: Option<balerion_bt::Inbound>,
    clock: Arc<Clock>,
) -> Result<Arc<Torrent>> {
    let download = DownloadConfig {
        max_peers: config.max_peers,
        pipeline_depth: config.pipeline_depth,
        // `config.peer_port` is the port actually bound, not the one asked
        // for: `serve` overwrites it once the listener is up, so what we
        // announce and what we are on cannot drift apart.
        port: config.peer_port,
        inbound,
        verify: VerifyPolicy::Auto,
        // This caller does re-announce, so the engine should wait to be fed
        // rather than concluding the swarm is dead the moment its list runs dry.
        peer_refill_grace: PEER_REFILL_GRACE,
        // The whole point: fetch what the player is waiting on, not what the
        // swarm would prefer we fetched.
        strategy: Strategy::Streaming,
        ..Default::default()
    };

    // Watching the progress stream rather than discarding it, so the first peer
    // and the first piece are recorded as they happen instead of being guessed
    // at afterwards from a poll.
    let marks = Arc::clone(&clock);
    let name = meta.name.clone();
    let (handle, task) =
        session::spawn_with_progress(&meta, root, peers, &download, move |update| match update {
            balerion_bt::Progress::PeerConnected { .. } => marks.first_peer(),
            balerion_bt::Progress::Piece { .. } => {
                let first = marks.snapshot().first_piece_ms.is_none();
                marks.first_piece();
                if first {
                    tracing::info!(name, timings = ?marks.snapshot(), "first piece down");
                }
            }
            _ => {}
        })
        .await
        .context("could not start the download")?;

    // Leave a torrent file beside the data. Without it the directory is an
    // anonymous pile of bytes and the next run cannot tell what is in it
    // without going back to the swarm to ask something we already know.
    crate::library::remember(root, &meta).await;

    let refill = spawn_refill(&meta, handle.clone(), config);

    Ok(Arc::new(Torrent::new(
        meta,
        handle,
        task,
        refill,
        root.to_path_buf(),
        clock,
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
    handle: balerion_bt::SessionHandle,
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
