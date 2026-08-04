//! The download coordinator.
//!
//! One task per peer, one per webseed, all pulling work from a shared
//! [`Picker`] and pushing verified pieces into [`Storage`]. Peers hold no
//! piece state of their own beyond what they have; the picker owns assignment,
//! so a peer dying mid-piece just releases it back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Mutex, mpsc};

use crate::error::{Error, Result};
use crate::infohash::generate_peer_id;
use crate::metainfo::Metainfo;
use crate::peer::PeerConnection;
use crate::picker::Picker;
use crate::storage::Storage;
use crate::webseed::Webseed;
use crate::wire::{BLOCK_SIZE, Bitfield, Message};

#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// How many peer connections to keep going at once.
    pub max_peers: usize,
    /// How many webseed fetches to run in parallel.
    pub webseed_tasks: usize,
    /// Blocks in flight per peer. Throughput is dominated by this, not by any
    /// clever piece strategy: one request at a time wastes a whole round trip
    /// per 16 KiB.
    pub pipeline_depth: usize,
    pub peer_timeout: Duration,
    pub webseed_timeout: Duration,
    /// The port we claim to listen on.
    pub port: u16,
    /// Re-hash existing files before starting, instead of trusting them.
    pub verify_existing: bool,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            max_peers: 30,
            webseed_tasks: 4,
            pipeline_depth: 16,
            peer_timeout: Duration::from_secs(20),
            webseed_timeout: Duration::from_secs(60),
            port: 6881,
            verify_existing: true,
        }
    }
}

/// Progress reports, emitted as the download runs.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Rechecking files already on disk.
    Verifying {
        checked: usize,
        total: usize,
    },
    /// We already had this many pieces before starting.
    Resumed {
        have: usize,
        total: usize,
    },
    PeerConnected {
        addr: String,
        client: Option<String>,
    },
    PeerLost {
        addr: String,
        reason: String,
    },
    /// A piece verified and hit the disk.
    Piece {
        index: usize,
        from: PieceSource,
        have: usize,
        total: usize,
        bytes: u64,
    },
    /// A piece failed its hash check and will be fetched again.
    PieceFailed {
        index: usize,
        from: PieceSource,
    },
    Done {
        bytes: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PieceSource {
    Peer(String),
    Webseed(String),
}

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadSummary {
    pub pieces: usize,
    pub bytes: u64,
    pub from_peers: usize,
    pub from_webseeds: usize,
    pub failed_hashes: usize,
    pub root: PathBuf,
}

/// Counters the coordinator reads after every worker has finished. Kept apart
/// from [`Shared`] so the coordinator can drop its `Shared` (and with it its
/// copy of the progress sender) and still report a summary. Holding on to a
/// sender is what deadlocks a drain loop.
#[derive(Debug, Default)]
struct Stats {
    have: AtomicU64,
    downloaded: AtomicU64,
    from_peers: AtomicU64,
    from_webseeds: AtomicU64,
    failed_hashes: AtomicU64,
}

struct Shared {
    meta: Metainfo,
    storage: Storage,
    picker: Mutex<Picker>,
    progress: mpsc::UnboundedSender<Progress>,
    stats: Arc<Stats>,
}

impl Shared {
    /// Verify, write and record a piece. Returns false if it did not hash.
    async fn accept(&self, index: usize, data: &[u8], from: PieceSource) -> Result<bool> {
        if !self.storage.verify(index, data) {
            self.stats.failed_hashes.fetch_add(1, Ordering::Relaxed);
            self.picker.lock().await.release(index);
            let _ = self.progress.send(Progress::PieceFailed { index, from });
            return Ok(false);
        }

        // In the endgame several workers race for the same piece, so the
        // second one home must not be counted twice.
        let (have, total) = {
            let mut picker = self.picker.lock().await;
            if picker.have().has(index) {
                return Ok(true);
            }
            picker.complete(index);
            (picker.completed(), picker.piece_count())
        };
        self.storage.write_piece(index, data).await?;
        self.stats.have.store(have as u64, Ordering::Relaxed);
        let bytes = self
            .stats
            .downloaded
            .fetch_add(data.len() as u64, Ordering::Relaxed)
            + data.len() as u64;
        match &from {
            PieceSource::Peer(_) => self.stats.from_peers.fetch_add(1, Ordering::Relaxed),
            PieceSource::Webseed(_) => self.stats.from_webseeds.fetch_add(1, Ordering::Relaxed),
        };
        let _ = self.progress.send(Progress::Piece {
            index,
            from,
            have,
            total,
            bytes,
        });
        Ok(true)
    }

    async fn is_complete(&self) -> bool {
        self.picker.lock().await.is_complete()
    }
}

/// Download a torrent into `root`, using both peers and webseeds.
///
/// `on_progress` is called from the caller's task, so it may do terminal I/O
/// without any locking of its own.
pub async fn download<F>(
    meta: &Metainfo,
    root: impl AsRef<Path>,
    peers: Vec<std::net::SocketAddr>,
    config: &DownloadConfig,
    mut on_progress: F,
) -> Result<DownloadSummary>
where
    F: FnMut(Progress),
{
    let storage = Storage::create(root.as_ref(), meta).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();

    // Work out what we already have. A fresh download costs one pass over
    // empty files, which is cheap; a resume saves everything.
    let existing = if config.verify_existing {
        let progress = tx.clone();
        storage
            .verify_all(|checked, total| {
                let _ = progress.send(Progress::Verifying { checked, total });
            })
            .await?
    } else {
        Bitfield::empty(meta.piece_count())
    };
    let _ = tx.send(Progress::Resumed {
        have: existing.count_set(),
        total: meta.piece_count(),
    });

    let root = storage.root().to_path_buf();
    let stats = Arc::new(Stats::default());
    stats
        .have
        .store(existing.count_set() as u64, Ordering::Relaxed);

    let shared = Arc::new(Shared {
        meta: meta.clone(),
        storage,
        picker: Mutex::new(Picker::with_have(existing)),
        progress: tx.clone(),
        stats: Arc::clone(&stats),
    });

    let mut tasks = Vec::new();

    if !shared.is_complete().await {
        let peer_id = generate_peer_id();
        for addr in peers.into_iter().take(config.max_peers) {
            let shared = Arc::clone(&shared);
            let config = config.clone();
            tasks.push(tokio::spawn(async move {
                let reason = match run_peer(addr, peer_id, &shared, &config).await {
                    Ok(()) => "finished".to_string(),
                    Err(err) => err.to_string(),
                };
                let _ = shared.progress.send(Progress::PeerLost {
                    addr: addr.to_string(),
                    reason,
                });
            }));
        }

        for url in &meta.webseeds {
            for _ in 0..config.webseed_tasks.max(1) {
                let shared = Arc::clone(&shared);
                let url = url.clone();
                let timeout = config.webseed_timeout;
                tasks.push(tokio::spawn(async move {
                    if let Err(err) = run_webseed(&url, &shared, timeout).await {
                        tracing::debug!(webseed = url, %err, "webseed stopped");
                    }
                }));
            }
        }
    }

    // Drain progress until every worker is done. The channel only closes once
    // the last sender is gone, so we must drop ours *and* our `Shared`, which
    // holds one of its own.
    drop(tx);
    drop(shared);
    while let Some(update) = rx.recv().await {
        on_progress(update);
    }
    for task in tasks {
        let _ = task.await;
    }

    let bytes = stats.downloaded.load(Ordering::Relaxed);
    let summary = DownloadSummary {
        pieces: stats.have.load(Ordering::Relaxed) as usize,
        bytes,
        from_peers: stats.from_peers.load(Ordering::Relaxed) as usize,
        from_webseeds: stats.from_webseeds.load(Ordering::Relaxed) as usize,
        failed_hashes: stats.failed_hashes.load(Ordering::Relaxed) as usize,
        root,
    };
    on_progress(Progress::Done { bytes });

    if summary.pieces < meta.piece_count() {
        return Err(Error::Peer(format!(
            "stopped with {}/{} pieces; no source could supply the rest",
            summary.pieces,
            meta.piece_count()
        )));
    }
    Ok(summary)
}

/// One peer, from connect to exhaustion.
async fn run_peer(
    addr: std::net::SocketAddr,
    peer_id: [u8; 20],
    shared: &Shared,
    config: &DownloadConfig,
) -> Result<()> {
    let mut peer = PeerConnection::connect(
        addr,
        shared.meta.info_hash,
        peer_id,
        config.port,
        config.peer_timeout,
    )
    .await?;
    peer.set_piece_count(shared.meta.piece_count())?;
    let _ = shared.progress.send(Progress::PeerConnected {
        addr: addr.to_string(),
        client: peer.client_name().map(str::to_string),
    });

    if let Some(have) = &peer.have {
        shared.picker.lock().await.add_peer(have);
    }
    peer.send(Message::Interested).await?;

    let mut idle_rounds = 0u32;
    loop {
        if shared.is_complete().await {
            return Ok(());
        }

        // While choked, keep reading: `unchoke`, `have` and `bitfield` all
        // arrive on the same stream and all matter.
        if peer.peer_choking {
            peer.recv(config.peer_timeout).await?;
            continue;
        }

        let Some(have) = peer.have.clone() else {
            return Err(Error::Peer(format!("{addr}: no bitfield")));
        };
        let piece = shared
            .picker
            .lock()
            .await
            .next_for(&have, u64::from(addr.port()) + u64::from(idle_rounds));

        let Some(index) = piece else {
            // Nothing we need right now; wait for the peer to gain a piece.
            idle_rounds += 1;
            if idle_rounds > 3 {
                return Ok(());
            }
            peer.recv(config.peer_timeout).await?;
            continue;
        };
        idle_rounds = 0;

        match fetch_piece_from_peer(&mut peer, shared, index, config).await {
            Ok(Some(data)) => {
                shared
                    .accept(index, &data, PieceSource::Peer(addr.to_string()))
                    .await?;
            }
            Ok(None) => {
                // Choked or otherwise interrupted; let someone else have it.
                shared.picker.lock().await.release(index);
            }
            Err(err) => {
                shared.picker.lock().await.release(index);
                return Err(err);
            }
        }
    }
}

/// Request every block of one piece with a sliding window, and assemble it.
async fn fetch_piece_from_peer(
    peer: &mut PeerConnection,
    shared: &Shared,
    index: usize,
    config: &DownloadConfig,
) -> Result<Option<Vec<u8>>> {
    let piece_size = shared
        .meta
        .piece_size(index)
        .ok_or_else(|| Error::Metainfo(format!("piece {index} is out of range")))?
        as u32;

    let mut offsets: Vec<u32> = (0..piece_size).step_by(BLOCK_SIZE as usize).collect();
    offsets.reverse(); // pop() from the front of the piece
    let mut blocks: HashMap<u32, Bytes> = HashMap::new();
    let mut outstanding = 0usize;

    loop {
        while outstanding < config.pipeline_depth.max(1)
            && let Some(begin) = offsets.pop()
        {
            let length = BLOCK_SIZE.min(piece_size - begin);
            peer.send(Message::Request {
                index: index as u32,
                begin,
                length,
            })
            .await?;
            outstanding += 1;
        }

        if blocks.values().map(|b| b.len() as u32).sum::<u32>() >= piece_size {
            break;
        }

        match peer.recv(config.peer_timeout).await? {
            Message::Piece {
                index: got,
                begin,
                block,
            } => {
                if got as usize != index {
                    // A block for a piece we abandoned. Harmless; ignore it.
                    continue;
                }
                outstanding = outstanding.saturating_sub(1);
                blocks.insert(begin, block);
            }
            Message::Choke => return Ok(None),
            Message::Have(have) => {
                shared.picker.lock().await.peer_has(have as usize);
            }
            _ => {}
        }
    }

    // Stitch the blocks together in offset order.
    let mut data = vec![0u8; piece_size as usize];
    for (begin, block) in blocks {
        let from = begin as usize;
        let to = from + block.len();
        if to > data.len() {
            return Err(Error::Peer(format!(
                "peer sent a block running past the end of piece {index}"
            )));
        }
        data[from..to].copy_from_slice(&block);
    }
    Ok(Some(data))
}

/// One webseed worker: take pieces and range-GET them.
async fn run_webseed(url: &str, shared: &Shared, timeout: Duration) -> Result<()> {
    let seed = Webseed::new(url, timeout)?;
    // A webseed has everything, by definition.
    let mut everything = Bitfield::empty(shared.meta.piece_count());
    for index in 0..shared.meta.piece_count() {
        everything.set(index);
    }

    let mut failures = 0u32;
    loop {
        if shared.is_complete().await {
            return Ok(());
        }
        let Some(index) = shared
            .picker
            .lock()
            .await
            .next_for(&everything, u64::from(failures) + url.len() as u64)
        else {
            return Ok(());
        };

        match seed.fetch_piece(&shared.meta, index).await {
            Ok(data) => {
                failures = 0;
                shared
                    .accept(index, &data, PieceSource::Webseed(url.to_string()))
                    .await?;
            }
            Err(err) => {
                shared.picker.lock().await.release(index);
                failures += 1;
                tracing::debug!(webseed = url, piece = index, %err, "webseed piece failed");
                // Three strikes and we assume this webseed is not going to
                // work, rather than hammering it for every piece.
                if failures >= 3 {
                    return Err(err);
                }
            }
        }
    }
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

    fn content(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn single_file_meta(content: &[u8], piece_length: usize) -> Metainfo {
        let mut hashes = Vec::new();
        for chunk in content.chunks(piece_length) {
            hashes.extend_from_slice(&Sha1::digest(chunk));
        }
        let mut info = Vec::new();
        info.extend(b"d");
        info.extend(bstr(b"length"));
        info.extend(format!("i{}e", content.len()).into_bytes());
        info.extend(bstr(b"name"));
        info.extend(bstr(b"payload.bin"));
        info.extend(bstr(b"piece length"));
        info.extend(format!("i{piece_length}e").into_bytes());
        info.extend(bstr(b"pieces"));
        info.extend(bstr(&hashes));
        info.extend(b"e");
        Metainfo::from_info_dict(&info).unwrap()
    }

    #[tokio::test]
    async fn an_already_complete_download_short_circuits() {
        let dir = tempfile::tempdir().unwrap();
        let content = content(2000);
        let meta = single_file_meta(&content, 1024);

        // Pre-write the file, as a completed download would leave it.
        tokio::fs::write(dir.path().join("payload.bin"), &content)
            .await
            .unwrap();

        let mut resumed = None;
        let summary = download(&meta, dir.path(), vec![], &DownloadConfig::default(), |p| {
            if let Progress::Resumed { have, total } = p {
                resumed = Some((have, total));
            }
        })
        .await
        .unwrap();

        assert_eq!(resumed, Some((2, 2)), "both pieces recognised on disk");
        assert_eq!(summary.pieces, 2);
        assert_eq!(summary.bytes, 0, "nothing needed downloading");
    }

    #[tokio::test]
    async fn a_download_with_no_sources_fails_rather_than_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let meta = single_file_meta(&content(2000), 1024);
        let err = download(
            &meta,
            dir.path(),
            vec![],
            &DownloadConfig::default(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert!(format!("{err}").contains("0/2 pieces"), "{err}");
    }

    #[tokio::test]
    async fn a_partial_download_resumes_from_what_is_there() {
        let dir = tempfile::tempdir().unwrap();
        let content = content(3000);
        let meta = single_file_meta(&content, 1024);

        // Write only the first piece, leaving the rest zeroed.
        let mut partial = content.clone();
        partial[1024..].fill(0);
        tokio::fs::write(dir.path().join("payload.bin"), &partial)
            .await
            .unwrap();

        let mut resumed = None;
        let _ = download(&meta, dir.path(), vec![], &DownloadConfig::default(), |p| {
            if let Progress::Resumed { have, total } = p {
                resumed = Some((have, total));
            }
        })
        .await;
        assert_eq!(resumed, Some((1, 3)));
    }
}
