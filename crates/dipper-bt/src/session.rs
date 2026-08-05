//! The download coordinator.
//!
//! One task per peer, one per webseed, all pulling work from a shared
//! [`Picker`] and pushing verified pieces into [`Storage`]. Peers hold no
//! piece state of their own beyond what they have; the picker owns assignment,
//! so a peer dying mid-piece just releases it back.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Mutex, mpsc, watch};
use tokio::task::JoinHandle;

use crate::error::{Error, Result};
use crate::infohash::generate_peer_id;
use crate::metainfo::Metainfo;
use crate::peer::PeerConnection;
use crate::picker::{Picker, Strategy};
use crate::resume::ResumeState;
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
    /// How to work out what is already on disk.
    pub verify: VerifyPolicy,
    /// Which piece to fetch next. Leave as [`Strategy::Rarest`] unless a
    /// reader is waiting on specific bytes, which is what [`spawn`] is for.
    pub strategy: Strategy,
}

/// What to do about data that is already in the download directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyPolicy {
    /// Trust a cleanly-written resume file; re-hash otherwise. This is what
    /// you want: re-hashing a 40 GB item on every start is a coffee break
    /// before anything happens.
    #[default]
    Auto,
    /// Always re-hash. Slow, and the right answer if you suspect the files.
    Always,
    /// Assume nothing is there. Fast, and will happily redownload everything.
    Never,
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
            verify: VerifyPolicy::Auto,
            strategy: Strategy::Rarest,
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
    /// We already had this much before starting. `rehashed` is false when the
    /// count came from a resume file rather than from re-reading every byte.
    ///
    /// `bytes` is the real total of the pieces we hold, not `have` multiplied
    /// by the piece length: held pieces are scattered, and the last one is
    /// usually short.
    Resumed {
        have: usize,
        total: usize,
        bytes: u64,
        rehashed: bool,
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
    /// Peers currently handshaked and running, for live reporting.
    connected: AtomicU64,
}

/// Keeps [`Stats::connected`] honest across every way a peer task can end,
/// including the error paths. A plain decrement at the bottom of `run_peer`
/// would leak a count on every `?`.
struct PeerCount(Arc<Stats>);

impl PeerCount {
    fn new(stats: &Arc<Stats>) -> Self {
        stats.connected.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(stats))
    }
}

impl Drop for PeerCount {
    fn drop(&mut self) {
        self.0.connected.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Keeps the resume sidecar roughly up to date without writing it on every
/// piece. Lives behind its own `Arc` so the coordinator can mark the file
/// clean after all the workers have gone.
struct ResumeWriter {
    root: PathBuf,
    info_hash: crate::infohash::InfoHash,
    piece_length: u64,
    total_length: u64,
    have: Mutex<Bitfield>,
    since_flush: AtomicU64,
}

/// Consecutive failures before we conclude a webseed is not coming back.
const WEBSEED_MAX_FAILURES: u32 = 5;

/// Backoff between webseed retries: 0.5s, 1s, 2s, 4s, capped at 5s.
fn webseed_backoff(failures: u32) -> Duration {
    Duration::from_millis(500 * 2u64.saturating_pow(failures.saturating_sub(1).min(6)))
        .min(Duration::from_secs(5))
}

/// Write the sidecar every this many pieces. Often enough that a kill -9 costs
/// seconds of re-hashing, rare enough that we are not writing a file per piece.
const RESUME_FLUSH_EVERY: u64 = 32;

impl ResumeWriter {
    fn new(root: PathBuf, meta: &Metainfo, have: Bitfield) -> Self {
        Self {
            root,
            info_hash: meta.info_hash,
            piece_length: meta.piece_length,
            total_length: meta.total_length,
            have: Mutex::new(have),
            since_flush: AtomicU64::new(0),
        }
    }

    async fn state(&self, clean: bool) -> ResumeState {
        ResumeState {
            info_hash: self.info_hash,
            piece_length: self.piece_length,
            total_length: self.total_length,
            have: self.have.lock().await.clone(),
            clean,
        }
    }

    /// Record a verified piece, flushing occasionally.
    async fn completed(&self, index: usize) {
        self.have.lock().await.set(index);
        if self.since_flush.fetch_add(1, Ordering::Relaxed) + 1 >= RESUME_FLUSH_EVERY {
            self.since_flush.store(0, Ordering::Relaxed);
            self.flush(false).await;
        }
    }

    /// Write the sidecar. Failures are logged, never fatal: the worst case is
    /// that the next run re-hashes, which is exactly where we started.
    async fn flush(&self, clean: bool) {
        let state = self.state(clean).await;
        if let Err(err) = state.save(&self.root).await {
            tracing::debug!(%err, "could not write the resume file");
        }
    }
}

struct Shared {
    meta: Metainfo,
    storage: Arc<Storage>,
    picker: Arc<Mutex<Picker>>,
    progress: mpsc::UnboundedSender<Progress>,
    stats: Arc<Stats>,
    resume: Arc<ResumeWriter>,
    /// Published on every verified piece so a reader can await one without
    /// polling. Sent while the picker lock is held, so it never goes backwards.
    have: watch::Sender<Bitfield>,
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

        // Write before marking. The other order lets a worker announce a piece
        // it has not finished writing, and a download that ends in that window
        // reports success over a file with a hole in it.
        self.storage.write_piece(index, data).await?;

        // In the endgame several workers race for the same piece, so the
        // second one home must not be counted twice.
        let (have, total) = {
            let mut picker = self.picker.lock().await;
            if picker.have().has(index) {
                return Ok(true);
            }
            picker.complete(index);
            // Published under the lock: two workers finishing at once would
            // otherwise be free to publish their snapshots out of order, and a
            // reader awaiting a piece would see it appear and then vanish.
            let _ = self.have.send(picker.have().clone());
            (picker.completed(), picker.piece_count())
        };
        // fetch_max, not store: two workers finishing out of order would
        // otherwise let the slower one write back a stale, lower count.
        self.stats.have.fetch_max(have as u64, Ordering::Relaxed);
        self.resume.completed(index).await;
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

/// A live view of a running download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStats {
    pub pieces_have: usize,
    pub pieces_total: usize,
    /// Bytes fetched during this run. Zero for a torrent that was already
    /// complete when it started, which is why it must not be shown as
    /// progress.
    pub bytes_downloaded: u64,
    /// Bytes actually held, verified and on disk, including anything a
    /// previous run left behind. This is the number a progress display wants.
    pub bytes_on_disk: u64,
    pub peers_connected: usize,
    pub failed_hashes: u64,
}

impl SessionStats {
    pub fn is_complete(&self) -> bool {
        self.pieces_have >= self.pieces_total
    }
}

/// A running download you can question and steer.
///
/// This is what turns the engine into something a media player can sit on top
/// of: ask for the bytes you need next with [`SessionHandle::prioritise`], wait
/// for them with [`SessionHandle::wait_for_piece`], then read them with
/// [`SessionHandle::read_range`]. Cheap to clone, one per request handler.
#[derive(Clone)]
pub struct SessionHandle {
    meta: Metainfo,
    storage: Arc<Storage>,
    picker: Arc<Mutex<Picker>>,
    stats: Arc<Stats>,
    have: watch::Receiver<Bitfield>,
}

impl SessionHandle {
    pub fn meta(&self) -> &Metainfo {
        &self.meta
    }

    /// A snapshot of the pieces verified so far.
    pub fn have(&self) -> Bitfield {
        self.have.borrow().clone()
    }

    pub fn has_piece(&self, index: usize) -> bool {
        self.have.borrow().has(index)
    }

    /// Wait until `index` is on disk and verified.
    ///
    /// Returns false when the session ended without it, which is the caller's
    /// cue to give up: the bytes on disk for that piece are still zeros, and
    /// serving them would be quietly handing out corruption.
    pub async fn wait_for_piece(&self, index: usize) -> bool {
        let mut have = self.have.clone();
        loop {
            if have.borrow_and_update().has(index) {
                return true;
            }
            if have.changed().await.is_err() {
                // Every sender is gone, so no further piece will ever land.
                return false;
            }
        }
    }

    /// Nominate the piece spans a reader needs soonest, most urgent first.
    ///
    /// Only has an effect under [`Strategy::Streaming`]. Replaces the previous
    /// nomination outright, because a seek makes the old one worthless.
    pub async fn prioritise(&self, spans: Vec<Range<usize>>) {
        self.picker.lock().await.set_priority(spans);
    }

    /// Read a byte span of the concatenated piece space.
    ///
    /// Only ask for spans whose pieces have landed. Files are sparse until
    /// written, so a premature read succeeds and returns zeros.
    pub async fn read_range(&self, start: u64, len: u64) -> Result<Vec<u8>> {
        self.storage.read_range(start, len).await
    }

    pub fn stats(&self) -> SessionStats {
        let have = self.have.borrow();
        // Summed rather than multiplied out: held pieces are scattered and the
        // last one is nearly always short, so `count * piece_length` overstates
        // and can exceed the torrent's own size.
        let bytes_on_disk = (0..have.len())
            .filter(|index| have.has(*index))
            .filter_map(|index| self.meta.piece_size(index))
            .sum();

        SessionStats {
            pieces_have: have.count_set(),
            pieces_total: have.len(),
            bytes_downloaded: self.stats.downloaded.load(Ordering::Relaxed),
            bytes_on_disk,
            peers_connected: self.stats.connected.load(Ordering::Relaxed) as usize,
            failed_hashes: self.stats.failed_hashes.load(Ordering::Relaxed),
        }
    }
}

/// A started session, before anyone has begun draining its progress.
struct Running {
    handle: SessionHandle,
    tasks: Vec<JoinHandle<()>>,
    progress: mpsc::UnboundedReceiver<Progress>,
    stats: Arc<Stats>,
    resume: Arc<ResumeWriter>,
    root: PathBuf,
    piece_count: usize,
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
    on_progress: F,
) -> Result<DownloadSummary>
where
    F: FnMut(Progress),
{
    let running = start(meta, root, peers, config).await?;
    finish(running, on_progress).await
}

/// Start a download and hand back a handle, leaving it running in the
/// background.
///
/// The returned [`JoinHandle`] resolves to the same summary [`download`] would
/// have produced. Dropping it does not stop the download; abort it to do that.
/// Set `config.strategy` to [`Strategy::Streaming`] if a reader is going to be
/// waiting on specific bytes.
pub async fn spawn(
    meta: &Metainfo,
    root: impl AsRef<Path>,
    peers: Vec<std::net::SocketAddr>,
    config: &DownloadConfig,
) -> Result<(SessionHandle, JoinHandle<Result<DownloadSummary>>)> {
    let running = start(meta, root, peers, config).await?;
    let handle = running.handle.clone();
    // Progress still has to be drained or the channel grows without bound.
    let task = tokio::spawn(async move { finish(running, |_| {}).await });
    Ok((handle, task))
}

/// Everything both entry points do: work out what is on disk, then set the
/// workers going.
async fn start(
    meta: &Metainfo,
    root: impl AsRef<Path>,
    peers: Vec<std::net::SocketAddr>,
    config: &DownloadConfig,
) -> Result<Running> {
    let storage = Storage::create(root.as_ref(), meta).await?;
    let (tx, rx) = mpsc::unbounded_channel();

    // Work out what we already have. A cleanly-saved resume file makes this
    // instant; anything else means re-reading and re-hashing the lot.
    let resume = match config.verify {
        VerifyPolicy::Auto => ResumeState::load(storage.root(), meta)
            .await
            .filter(|state| state.clean),
        VerifyPolicy::Always | VerifyPolicy::Never => None,
    };
    let (existing, rehashed) = match resume {
        Some(state) => (state.have, false),
        None if config.verify == VerifyPolicy::Never => {
            (Bitfield::empty(meta.piece_count()), false)
        }
        None => {
            let progress = tx.clone();
            let have = storage
                .verify_all(|checked, total| {
                    let _ = progress.send(Progress::Verifying { checked, total });
                })
                .await?;
            (have, true)
        }
    };
    let existing_bytes: u64 = (0..meta.piece_count())
        .filter(|index| existing.has(*index))
        .filter_map(|index| meta.piece_size(index))
        .sum();
    let _ = tx.send(Progress::Resumed {
        have: existing.count_set(),
        total: meta.piece_count(),
        bytes: existing_bytes,
        rehashed,
    });

    let root = storage.root().to_path_buf();
    let stats = Arc::new(Stats::default());
    stats
        .have
        .store(existing.count_set() as u64, Ordering::Relaxed);

    let resume = Arc::new(ResumeWriter::new(root.clone(), meta, existing.clone()));
    // Mark the download in progress straight away: if we are killed before
    // the next flush, the unclean flag sends the next run back to re-hashing.
    resume.flush(false).await;

    let mut picker = Picker::with_have(existing.clone());
    picker.set_strategy(config.strategy);
    let picker = Arc::new(Mutex::new(picker));
    let storage = Arc::new(storage);
    let (have_tx, have_rx) = watch::channel(existing);

    let shared = Arc::new(Shared {
        meta: meta.clone(),
        storage: Arc::clone(&storage),
        picker: Arc::clone(&picker),
        progress: tx.clone(),
        stats: Arc::clone(&stats),
        resume: Arc::clone(&resume),
        have: have_tx,
    });

    let handle = SessionHandle {
        meta: meta.clone(),
        storage,
        picker,
        stats: Arc::clone(&stats),
        have: have_rx,
    };

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

    // The workers hold their own senders through `Shared`; ours and the one
    // inside our `Shared` would keep the channel open for ever, and the drain
    // in `finish` would never end.
    drop(tx);
    drop(shared);

    Ok(Running {
        handle,
        tasks,
        progress: rx,
        stats,
        resume,
        root,
        piece_count: meta.piece_count(),
    })
}

/// Drain progress until every worker has stopped, then settle up.
async fn finish<F>(running: Running, mut on_progress: F) -> Result<DownloadSummary>
where
    F: FnMut(Progress),
{
    let Running {
        handle,
        tasks,
        mut progress,
        stats,
        resume,
        root,
        piece_count,
    } = running;
    // The handle holds a watch receiver, not a progress sender, so it cannot
    // wedge the drain. Dropping it early would still be wrong: `spawn` gave a
    // clone to the caller, and a live receiver is what lets them keep reading.
    drop(handle);

    while let Some(update) = progress.recv().await {
        on_progress(update);
    }
    for task in tasks {
        let _ = task.await;
    }
    // Everyone has stopped, so what we have now is final: write it down and
    // mark it clean, which is what lets the next run skip re-hashing.
    resume.flush(true).await;

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

    if summary.pieces < piece_count {
        return Err(Error::Peer(format!(
            "stopped with {}/{piece_count} pieces; no source could supply the rest",
            summary.pieces,
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
    // Counted from here rather than from `connect`, so the live figure means
    // "peers actually trading with us" rather than "sockets we opened".
    let _counted = PeerCount::new(&shared.stats);
    let _ = shared.progress.send(Progress::PeerConnected {
        addr: addr.to_string(),
        client: peer.client_name().map(str::to_string),
    });

    if let Some(have) = &peer.have {
        shared.picker.lock().await.add_peer(have);
    }
    peer.send(Message::Interested).await?;

    let mut idle_rounds = 0u32;
    let mut bad_pieces = 0u32;
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
                if !shared
                    .accept(index, &data, PieceSource::Peer(addr.to_string()))
                    .await?
                {
                    // Attribution is unambiguous here: this peer supplied
                    // every block of the piece. Three bad pieces and we stop
                    // giving it the benefit of the doubt.
                    bad_pieces += 1;
                    if bad_pieces >= 3 {
                        return Err(Error::PieceMismatch {
                            index: index as u32,
                        });
                    }
                }
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
                // A piece that arrives intact but fails its hash counts as a
                // failure too, or a webseed serving corrupt bytes would keep
                // us fetching the same piece for ever.
                let verified = shared
                    .accept(index, &data, PieceSource::Webseed(url.to_string()))
                    .await?;
                if verified {
                    failures = 0;
                } else {
                    failures += 1;
                    if failures >= WEBSEED_MAX_FAILURES {
                        return Err(Error::PieceMismatch {
                            index: index as u32,
                        });
                    }
                    tokio::time::sleep(webseed_backoff(failures)).await;
                }
            }
            Err(err) => {
                shared.picker.lock().await.release(index);
                failures += 1;
                tracing::debug!(webseed = url, piece = index, %err, "webseed piece failed");
                // Give up eventually, but not immediately: archive.org's data
                // nodes throw the occasional 500 and recover seconds later,
                // and burning through every worker on a transient error means
                // stopping at 27 pieces out of 28.
                if failures >= WEBSEED_MAX_FAILURES {
                    return Err(err);
                }
                tokio::time::sleep(webseed_backoff(failures)).await;
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
            if let Progress::Resumed { have, total, .. } = p {
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
            if let Progress::Resumed { have, total, .. } = p {
                resumed = Some((have, total));
            }
        })
        .await;
        assert_eq!(resumed, Some((1, 3)));
    }
}
