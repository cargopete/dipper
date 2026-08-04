//! The `resolve` and `download` commands: magnet or torrent in, bytes out.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use dipper_bt::discovery::{Discovery, DiscoveryConfig, Source as PeerSource};
use dipper_bt::infohash::generate_peer_id;
use dipper_bt::session::{DownloadConfig, PieceSource, Progress};
use dipper_bt::{Dht, Magnet, Metainfo, peer, session};
use indicatif::{ProgressBar, ProgressStyle};

use crate::fmt;
use crate::torrent_source::Source;

/// Knobs shared by `resolve` and `download`.
#[derive(Debug, Clone)]
pub struct EngineOptions {
    pub no_dht: bool,
    pub no_webseeds: bool,
    pub port: u16,
    pub max_peers: usize,
    pub dht_budget: Duration,
    pub tracker_timeout: Duration,
    pub quiet: bool,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            no_dht: false,
            no_webseeds: false,
            port: 6881,
            max_peers: 30,
            dht_budget: Duration::from_secs(20),
            tracker_timeout: Duration::from_secs(10),
            quiet: false,
        }
    }
}

/// Find peers and, if we do not have it already, the torrent metadata.
///
/// This is the interesting half of magnet support: everything after it is
/// identical to having had a `.torrent` all along.
pub async fn resolve_metadata(
    source: &Source,
    options: &EngineOptions,
) -> Result<(Metainfo, Vec<std::net::SocketAddr>)> {
    let magnet = source.magnet();
    if !options.quiet && source.metainfo().is_none() {
        // Worth saying out loud: from here on, everything about this torrent
        // comes from strangers.
        println!("resolving {}", source.describe());
    }
    let peers = find_peers(&magnet, source.metainfo(), options).await?;

    if let Some(meta) = source.metainfo() {
        let mut meta = meta.clone();
        meta.apply_magnet(&magnet);
        return Ok((meta, peers));
    }

    if peers.is_empty() {
        bail!(
            "no peers found for {}. Trackers and the DHT both came up empty; \
             the swarm may be dead, or your network may be blocking outbound BitTorrent",
            magnet.info_hash
        );
    }

    let spinner = spinner(options, "asking peers for the torrent metadata");
    let peer_id = generate_peer_id();
    let (mut meta, from) =
        peer::fetch_metadata_from_peers(&peers, magnet.info_hash, peer_id, options.port, 8)
            .await
            .context("no peer would serve the torrent metadata")?;
    // Only now do we know the file list. It came from a stranger, and it is
    // trustworthy solely because it hashed to the infohash we asked for.
    meta.apply_magnet(&magnet);
    finish(spinner, format!("metadata from {from}: {}", meta.name));
    Ok((meta, peers))
}

async fn find_peers(
    magnet: &Magnet,
    meta: Option<&Metainfo>,
    options: &EngineOptions,
) -> Result<Vec<std::net::SocketAddr>> {
    // A torrent file usually carries more trackers than its magnet does.
    let mut magnet = magnet.clone();
    if let Some(meta) = meta {
        for tracker in &meta.announce {
            if !magnet.trackers.contains(tracker) {
                magnet.trackers.push(tracker.clone());
            }
        }
    }

    let dht = if options.no_dht {
        None
    } else {
        match Dht::client() {
            Ok(dht) => Some(dht),
            Err(err) => {
                tracing::warn!(%err, "could not start the DHT; carrying on without it");
                None
            }
        }
    };

    let spinner = spinner(
        options,
        &format!(
            "looking for peers ({} trackers{})",
            magnet.trackers.len(),
            if dht.is_some() { " + dht" } else { "" }
        ),
    );

    let discovery = Discovery::new(magnet.info_hash);
    let config = DiscoveryConfig {
        tracker_timeout: options.tracker_timeout,
        dht_budget: options.dht_budget,
        use_dht: !options.no_dht,
        port: options.port,
    };
    let left = meta.map(|m| m.total_length).unwrap_or(0);

    discovery
        .run(
            &magnet,
            generate_peer_id(),
            left,
            dht.as_ref(),
            &config,
            |source, peers| {
                let label = match source {
                    PeerSource::Magnet => "magnet",
                    PeerSource::Tracker => "tracker",
                    PeerSource::Dht => "dht",
                    PeerSource::Pex => "pex",
                };
                tracing::debug!(source = label, count = peers.len(), "peers found");
            },
        )
        .await;

    let peers = discovery.peers();
    finish(spinner, format!("{} peers", peers.len()));
    Ok(peers)
}

/// Print what a magnet or torrent turns out to be, without downloading it.
pub async fn resolve_command(source: &Source, options: &EngineOptions) -> Result<()> {
    let (meta, peers) = resolve_metadata(source, options).await?;

    println!("name:        {}", meta.name);
    println!("infohash:    {}", meta.info_hash);
    println!("size:        {}", fmt::bytes(meta.total_length));
    println!(
        "pieces:      {} x {}",
        meta.piece_count(),
        fmt::bytes(meta.piece_length)
    );
    println!("files:       {}", meta.files.len());
    println!("peers:       {}", peers.len());
    if !meta.announce.is_empty() {
        println!("trackers:    {}", meta.announce.join("\n             "));
    }
    if !meta.webseeds.is_empty() {
        println!("webseeds:    {}", meta.webseeds.join("\n             "));
    }
    println!();
    for file in &meta.files {
        println!("  {:>10}  {}", fmt::bytes(file.length), file.path);
    }
    Ok(())
}

/// Download everything, verifying as we go.
pub async fn download_command(
    source: &Source,
    output: Option<PathBuf>,
    options: &EngineOptions,
) -> Result<()> {
    let (mut meta, peers) = resolve_metadata(source, options).await?;
    if options.no_webseeds {
        meta.webseeds.clear();
    }

    let root = output.unwrap_or_else(|| PathBuf::from("."));
    println!(
        "{} ({}, {} pieces) -> {}",
        meta.name,
        fmt::bytes(meta.total_length),
        meta.piece_count(),
        root.display()
    );
    if meta.webseeds.is_empty() && peers.is_empty() {
        bail!("nothing to download from: no peers and no webseeds");
    }

    let bar = if options.quiet {
        ProgressBar::hidden()
    } else {
        let bar = ProgressBar::new(meta.total_length);
        bar.set_style(
            ProgressStyle::with_template("{bar:32} {bytes}/{total_bytes}  {bytes_per_sec}  {msg}")
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
        );
        bar
    };

    let config = DownloadConfig {
        max_peers: options.max_peers,
        port: options.port,
        ..Default::default()
    };
    let started = Instant::now();
    let already_have = std::cell::Cell::new(0u64);

    let summary = session::download(&meta, &root, peers, &config, |update| match update {
        Progress::Verifying { checked, total } => {
            bar.set_message(format!("checking existing data {checked}/{total}"));
        }
        Progress::Resumed { have, total } => {
            let bytes: u64 = (0..have).filter_map(|i| meta.piece_size(i)).sum();
            already_have.set(bytes);
            if have > 0 {
                bar.set_position(bytes);
                bar.set_message(format!("resuming with {have}/{total} pieces"));
            }
        }
        Progress::PeerConnected { addr, client } => {
            tracing::debug!(addr, client, "peer connected");
        }
        Progress::PeerLost { addr, reason } => {
            tracing::debug!(addr, reason, "peer gone");
        }
        Progress::Piece { bytes, from, .. } => {
            bar.set_position(already_have.get() + bytes);
            bar.set_message(match from {
                PieceSource::Webseed(_) => "webseed".to_string(),
                PieceSource::Peer(addr) => addr,
            });
        }
        Progress::PieceFailed { index, .. } => {
            tracing::warn!(index, "piece failed its hash check; refetching");
        }
        Progress::Done { .. } => bar.finish_and_clear(),
    })
    .await
    .context("download failed")?;

    let elapsed = started.elapsed();
    println!(
        "done: {} in {} pieces ({} from peers, {} from webseeds) in {:.1}s",
        fmt::bytes(summary.bytes),
        summary.pieces,
        summary.from_peers,
        summary.from_webseeds,
        elapsed.as_secs_f64()
    );
    if summary.failed_hashes > 0 {
        println!(
            "note: {} piece(s) arrived corrupt and were refetched",
            summary.failed_hashes
        );
    }
    println!("saved to {}", summary.root.display());
    Ok(())
}

fn spinner(options: &EngineOptions, message: &str) -> Option<ProgressBar> {
    if options.quiet {
        return None;
    }
    let bar = ProgressBar::new_spinner();
    bar.enable_steady_tick(Duration::from_millis(120));
    bar.set_message(message.to_string());
    Some(bar)
}

fn finish(spinner: Option<ProgressBar>, message: String) {
    if let Some(bar) = spinner {
        bar.finish_and_clear();
        println!("{message}");
    }
}
