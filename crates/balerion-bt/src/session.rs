//! The download coordinator.
//!
//! One task per peer, one per webseed, all pulling work from a shared
//! [`Picker`] and pushing verified pieces into [`Storage`]. Peers hold no
//! piece state of their own beyond what they have; the picker owns assignment,
//! so a peer dying mid-piece just releases it back.

use std::collections::{HashMap, VecDeque};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Mutex, Notify, mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};

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
    /// How long to wait for a message on a connection that already works.
    pub peer_timeout: Duration,
    /// How long to spend getting one open in the first place.
    ///
    /// Separate from `peer_timeout` because most addresses on a public swarm
    /// never answer, and with one figure for both jobs each of them holds a
    /// connection slot for the full read timeout while doing nothing.
    pub peer_connect_timeout: Duration,
    pub webseed_timeout: Duration,
    /// The port we claim to listen on.
    ///
    /// Set this from [`crate::inbound::Inbound::port`] whenever `inbound` is
    /// present: the listener may have had to take a different port from the one
    /// it was asked for, and announcing the wrong one is the fault the listener
    /// exists to fix.
    pub port: u16,
    /// Retry an obfuscated handshake when a plaintext one is refused.
    ///
    /// On by default. The cost is one extra connection to an address that
    /// accepted a socket and then hung up, which on a public swarm is a great
    /// many of them; the gain is the peers that will not speak plaintext at
    /// all, and which are otherwise invisible. Worth turning off when every
    /// address is known to be friendly, which mostly means in tests.
    pub use_encryption: bool,
    /// The shared listening socket, when this process has one.
    ///
    /// `None` keeps the old behaviour exactly: outbound connections only. With
    /// one, peers that cannot be dialled can dial us, which on a public swarm is
    /// a large fraction of it.
    pub inbound: Option<crate::inbound::Inbound>,
    /// How to work out what is already on disk.
    pub verify: VerifyPolicy,
    /// Which piece to fetch next. Leave as [`Strategy::Rarest`] unless a
    /// reader is waiting on specific bytes, which is what [`spawn`] is for.
    pub strategy: Strategy,
    /// How long to wait for someone to hand us more peers once every address
    /// we know of is spent, before concluding that nobody will.
    ///
    /// The session does not know what a tracker is and cannot go looking, so
    /// this is the window in which a caller who does can call
    /// [`SessionHandle::add_peers`].
    ///
    /// Zero by default, which is the old behaviour exactly: run out of
    /// addresses with nothing connected and the search stops. A caller that
    /// re-announces on a timer should set this comfortably longer than that
    /// timer, or the session gives up in the gap between announcements. A
    /// caller that does not should leave it alone rather than pay a wait for a
    /// refill that is never coming.
    pub peer_refill_grace: Duration,
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
            peer_connect_timeout: crate::peer::DEFAULT_CONNECT_TIMEOUT,
            webseed_timeout: Duration::from_secs(60),
            port: 6881,
            use_encryption: true,
            inbound: None,
            verify: VerifyPolicy::Auto,
            strategy: Strategy::Rarest,
            peer_refill_grace: Duration::ZERO,
        }
    }
}

impl DownloadConfig {
    fn timeouts(&self) -> crate::peer::Timeouts {
        crate::peer::Timeouts {
            connect: self.peer_connect_timeout,
            read: self.peer_timeout,
            encrypt: self.use_encryption,
        }
    }
}

/// How many times one address is worth trying.
///
/// A peer that connects and then finds nothing to give us goes back in the
/// queue, because under [`Strategy::Streaming`] the picker often has nothing to
/// hand out for a moment and retiring a live seeder over that would be
/// perverse. This bounds the resulting churn: without it, a peer that connects
/// and immediately idles out would be requeued for ever.
const MAX_PEER_ATTEMPTS: u32 = 3;

/// Addresses waiting for a connection slot, and how often each has been tried.
///
/// The queue exists because of what a public swarm actually looks like. A
/// tracker will happily name 60 peers of which most are unreachable: behind a
/// NAT nobody can dial, or simply fake. Taking the first `max_peers` of that
/// list once and never revisiting it means the dead addresses hold their slots
/// for the whole download, and a swarm of 44 seeders is served by whichever one
/// or two happened to answer.
#[derive(Debug, Default)]
struct PeerQueue {
    inner: std::sync::Mutex<PeerQueueInner>,
    /// Woken when addresses arrive, so a supervisor with empty hands does not
    /// have to poll to notice.
    added: Notify,
}

#[derive(Debug, Default)]
struct PeerQueueInner {
    pending: VecDeque<std::net::SocketAddr>,
    attempts: HashMap<std::net::SocketAddr, u32>,
}

impl PeerQueue {
    fn new(peers: impl IntoIterator<Item = std::net::SocketAddr>) -> Self {
        let queue = Self::default();
        queue.push(peers);
        queue
    }

    /// Add addresses, ignoring any already queued or already spent.
    ///
    /// Returns how many were actually new. Re-announcing returns mostly the
    /// same list every time, so without the filtering a refill would push the
    /// same dead addresses back in front of the live ones.
    fn push(&self, peers: impl IntoIterator<Item = std::net::SocketAddr>) -> usize {
        let mut inner = self.inner.lock().expect("peer queue lock");
        let mut added = 0;
        for addr in peers {
            let spent = inner
                .attempts
                .get(&addr)
                .is_some_and(|tries| *tries >= MAX_PEER_ATTEMPTS);
            if spent || inner.pending.contains(&addr) {
                continue;
            }
            inner.pending.push_back(addr);
            added += 1;
        }
        drop(inner);
        if added > 0 {
            self.added.notify_waiters();
        }
        added
    }

    /// Take the next address to try, counting the attempt.
    fn pop(&self) -> Option<std::net::SocketAddr> {
        let mut inner = self.inner.lock().expect("peer queue lock");
        let addr = inner.pending.pop_front()?;
        *inner.attempts.entry(addr).or_insert(0) += 1;
        Some(addr)
    }

    /// Put a peer that worked back at the end of the queue.
    ///
    /// At the end rather than the front: it has just told us it has nothing for
    /// us right now, so everything untried deserves a look first.
    fn requeue(&self, addr: std::net::SocketAddr) {
        self.push([addr]);
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
    peers: Arc<PeerQueue>,
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

    /// Offer the session more peers to try.
    ///
    /// The engine has no idea what a tracker is, on purpose: it is handed
    /// addresses and asked for pieces. This is how a caller that does know
    /// keeps it supplied, by re-announcing on a timer and passing on whatever
    /// comes back. Addresses already tried to exhaustion are ignored, so
    /// passing the same list repeatedly is free.
    ///
    /// Returns how many were new. Worth logging: on a hostile swarm a refill
    /// that adds nothing means every address the tracker knows is spent, and no
    /// amount of waiting will change that.
    pub fn add_peers(&self, peers: impl IntoIterator<Item = std::net::SocketAddr>) -> usize {
        self.peers.push(peers)
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
    spawn_with_progress(meta, root, peers, config, |_| {}).await
}

/// The same, but watching it happen.
///
/// The progress stream has to be drained by somebody or its channel grows
/// without bound, so a caller that wants the events costs nothing over one that
/// does not. `on_progress` runs on the draining task, so keep it quick: it is
/// between the workers and the only thing emptying their channel.
pub async fn spawn_with_progress<F>(
    meta: &Metainfo,
    root: impl AsRef<Path>,
    peers: Vec<std::net::SocketAddr>,
    config: &DownloadConfig,
    on_progress: F,
) -> Result<(SessionHandle, JoinHandle<Result<DownloadSummary>>)>
where
    F: FnMut(Progress) + Send + 'static,
{
    let running = start(meta, root, peers, config).await?;
    let handle = running.handle.clone();
    let task = tokio::spawn(async move { finish(running, on_progress).await });
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

    let queue = Arc::new(PeerQueue::new(peers));

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
        peers: Arc::clone(&queue),
    };

    let mut tasks = Vec::new();

    if !shared.is_complete().await {
        // One supervisor rather than one task per address, so a dead peer
        // frees its slot for the next candidate instead of holding it until
        // the download ends.
        let shared_for_peers = Arc::clone(&shared);
        let queue_for_peers = Arc::clone(&queue);
        let peer_config = config.clone();
        // Claim this torrent's share of the listening socket, if the process
        // has one. The claim is handed to the supervisor and released when it
        // ends, so a swept torrent stops being offered connections.
        let (claim, incoming) = match &config.inbound {
            Some(inbound) => {
                let (claim, rx) = inbound.register(meta.info_hash);
                (Some(claim), Some(rx))
            }
            None => (None, None),
        };
        tasks.push(tokio::spawn(async move {
            supervise_peers(
                queue_for_peers,
                shared_for_peers,
                peer_config,
                incoming,
                claim,
            )
            .await;
        }));

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

/// Keep up to `max_peers` peer connections going for as long as there is work.
///
/// Fills every free slot from the queue, waits for whichever worker finishes
/// first, and fills again. A worker that ended cleanly goes back in the queue
/// because it is a peer that demonstrably talks to us; one that ended with an
/// error is dropped and not retried here. A later [`SessionHandle::add_peers`]
/// may still offer that address again, which is deliberate (a peer that refused
/// five minutes ago may not refuse now) and bounded by [`MAX_PEER_ATTEMPTS`].
///
/// Gives up when the queue is empty, nothing is connected, and nobody has
/// offered more addresses within `peer_refill_grace`. That last condition is
/// what stops a dead swarm hanging the caller for ever while still leaving room
/// for a caller who re-announces to keep the session alive.
///
/// Peers that dialled us arrive on `incoming` and take slots from the same
/// budget. They do not extend the grace period on their own: a caller who wants
/// to sit and wait for someone to find us has to say so by setting one, or a
/// download with no sources would hang rather than failing.
async fn supervise_peers(
    queue: Arc<PeerQueue>,
    shared: Arc<Shared>,
    config: DownloadConfig,
    mut incoming: Option<mpsc::Receiver<crate::inbound::Incoming>>,
    // Held, not read: dropping it is what tells the listener to stop routing
    // connections here once this session is over.
    _claim: Option<crate::inbound::Registration>,
) {
    let peer_id = generate_peer_id();
    let mut workers: JoinSet<std::net::SocketAddr> = JoinSet::new();

    while !shared.is_complete().await {
        while workers.len() < config.max_peers {
            let Some(addr) = queue.pop() else { break };
            let shared = Arc::clone(&shared);
            let config = config.clone();
            let queue = Arc::clone(&queue);
            workers.spawn(async move {
                let reason = match run_peer(addr, peer_id, &shared, &config, &queue).await {
                    Ok(()) => "finished".to_string(),
                    Err(err) => err.to_string(),
                };
                let requeue = reason == "finished";
                let _ = shared.progress.send(Progress::PeerLost {
                    addr: addr.to_string(),
                    reason,
                });
                // The address is handed back through the join value rather than
                // by touching the queue here, so requeueing cannot race the
                // supervisor's own decision to stop.
                if requeue { addr } else { UNUSABLE }
            });
        }

        if workers.is_empty() {
            // Nothing connected and nothing left to try. Wait to be given more,
            // by a caller re-announcing or by somebody dialling us.
            let arrival = tokio::time::timeout(config.peer_refill_grace, async {
                match incoming.as_mut() {
                    Some(rx) => {
                        tokio::select! {
                            () = queue.added.notified() => None,
                            connection = rx.recv() => connection,
                        }
                    }
                    None => {
                        queue.added.notified().await;
                        None
                    }
                }
            })
            .await;

            match arrival {
                Err(_) => {
                    tracing::debug!(
                        "no peers left to try and none offered; stopping the peer search"
                    );
                    return;
                }
                Ok(Some(connection)) => {
                    spawn_incoming(&mut workers, connection, peer_id, &shared, &config, &queue);
                }
                Ok(None) => {}
            }
            continue;
        }

        // Slots are full, or the queue is empty and some workers are still
        // going. Either way the next thing to react to is whichever comes
        // first: a worker ending, fresh addresses arriving, or someone dialling
        // us.
        tokio::select! {
            finished = workers.join_next() => {
                if let Some(Ok(addr)) = finished
                    && addr != UNUSABLE
                {
                    queue.requeue(addr);
                }
            }
            () = queue.added.notified() => {}
            connection = next_incoming(&mut incoming), if workers.len() < config.max_peers => {
                if let Some(connection) = connection {
                    spawn_incoming(&mut workers, connection, peer_id, &shared, &config, &queue);
                }
            }
        }
    }
}

/// The next peer to dial us, or never, when this process is not listening.
///
/// `pending` rather than an early return, because this is a `select!` arm: an
/// arm that resolves immediately to "nothing" would spin the loop.
async fn next_incoming(
    rx: &mut Option<mpsc::Receiver<crate::inbound::Incoming>>,
) -> Option<crate::inbound::Incoming> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Put an accepted connection to work in the same pool as the dialled ones.
fn spawn_incoming(
    workers: &mut JoinSet<std::net::SocketAddr>,
    connection: crate::inbound::Incoming,
    peer_id: [u8; 20],
    shared: &Arc<Shared>,
    config: &DownloadConfig,
    queue: &Arc<PeerQueue>,
) {
    let addr = connection.addr;
    let shared = Arc::clone(shared);
    let config = config.clone();
    let queue = Arc::clone(queue);
    workers.spawn(async move {
        let reason = match run_incoming(connection, peer_id, &shared, &config, &queue).await {
            Ok(()) => "finished".to_string(),
            Err(err) => err.to_string(),
        };
        let _ = shared.progress.send(Progress::PeerLost {
            addr: addr.to_string(),
            reason,
        });
        // Never requeued. The address we saw is the port their connection came
        // *from*, which is ephemeral and is not the port they listen on, so
        // dialling it back later would reach nobody.
        UNUSABLE
    });
}

/// Stands in for "this address is not worth another go" in a worker's join
/// value. An unspecified address with port 0 is not a peer anyone can dial.
const UNUSABLE: std::net::SocketAddr =
    std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0);

/// One peer, from connect to exhaustion.
///
/// `queue` is here so that peers this one introduces us to over BEP 11 go
/// straight back into the pool of addresses to try. On a public swarm that is
/// frequently where the useful peer comes from: most of what a tracker names is
/// unreachable, and the members of a swarm know each other rather better than
/// the tracker does.
async fn run_peer(
    addr: std::net::SocketAddr,
    peer_id: [u8; 20],
    shared: &Shared,
    config: &DownloadConfig,
    queue: &PeerQueue,
) -> Result<()> {
    let peer = PeerConnection::connect(
        addr,
        shared.meta.info_hash,
        peer_id,
        config.port,
        config.timeouts(),
    )
    .await?;
    drive_peer(peer, shared, config, queue).await
}

/// The same, for a peer that dialled us instead.
///
/// Worth having at all because a peer behind a NAT can reach us and cannot be
/// reached, so without this half a public swarm is invisible in both directions
/// at once.
async fn run_incoming(
    incoming: crate::inbound::Incoming,
    peer_id: [u8; 20],
    shared: &Shared,
    config: &DownloadConfig,
    queue: &PeerQueue,
) -> Result<()> {
    let peer = PeerConnection::accept(
        incoming,
        shared.meta.info_hash,
        peer_id,
        config.port,
        config.timeouts(),
    )
    .await?;
    drive_peer(peer, shared, config, queue).await
}

/// Everything after the handshake, whichever side made the connection.
async fn drive_peer(
    mut peer: PeerConnection,
    shared: &Shared,
    config: &DownloadConfig,
    queue: &PeerQueue,
) -> Result<()> {
    peer.set_piece_count(shared.meta.piece_count())?;
    let outcome = trade_with(&mut peer, shared, config, queue).await;

    // Take this peer's pieces back out of the availability counts, whatever
    // happened. `peer.have` has been kept current by every `Have` message that
    // arrived, so the bitfield removed here is the one that was counted in.
    // Skipping this is how a swarm that has churned all evening ends up
    // believing every piece is equally common, and rarest-first quietly stops
    // being rarest-first.
    if let Some(have) = &peer.have {
        shared.picker.lock().await.remove_peer(have);
    }
    outcome
}

async fn trade_with(
    peer: &mut PeerConnection,
    shared: &Shared,
    config: &DownloadConfig,
    queue: &PeerQueue,
) -> Result<()> {
    let addr = peer.addr;
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
        // Anything this peer has told us about since the last pass. Done here
        // rather than at each `recv` because every message arrives through one,
        // including the ones inside `fetch_piece_from_peer`.
        let introduced = peer.take_pex_peers();
        if !introduced.is_empty() {
            let fresh = queue.push(introduced);
            if fresh > 0 {
                tracing::debug!(%addr, fresh, "peer exchange offered addresses we had not tried");
            }
        }

        if shared.is_complete().await {
            return Ok(());
        }

        // While choked, keep reading: `unchoke`, `have` and `bitfield` all
        // arrive on the same stream and all matter.
        if peer.peer_choking {
            let message = peer.recv(config.peer_timeout).await?;
            note_have(&message, shared).await;
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
            let message = peer.recv(config.peer_timeout).await?;
            note_have(&message, shared).await;
            continue;
        };
        idle_rounds = 0;

        match fetch_piece_from_peer(peer, shared, index, config).await {
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

/// Keep the picker's availability counts in step with a `have` message.
///
/// Every path that reads from a peer has to do this, not just the one inside
/// [`fetch_piece_from_peer`]: the connection updates its own bitfield on every
/// message, and if the picker does not hear about the same pieces then the
/// removal when the peer leaves subtracts counts that were never added. The
/// subtraction saturates rather than wrapping, so the failure is a drift in
/// rarest-first rather than a panic, which is precisely the sort of fault that
/// goes unnoticed for a year.
async fn note_have(message: &Message, shared: &Shared) {
    if let Message::Have(index) = message {
        shared.picker.lock().await.peer_has(*index as usize);
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
mod peer_queue_tests {
    use super::*;

    fn addr(port: u16) -> std::net::SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    #[test]
    fn addresses_come_back_in_the_order_they_went_in() {
        let queue = PeerQueue::new([addr(1), addr(2), addr(3)]);
        assert_eq!(queue.pop(), Some(addr(1)));
        assert_eq!(queue.pop(), Some(addr(2)));
        assert_eq!(queue.pop(), Some(addr(3)));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn a_duplicate_does_not_take_a_second_slot_in_the_queue() {
        // Re-announcing returns almost the same list every time. Without this,
        // a refill would push the addresses we have already given up on back in
        // front of the ones we have not tried.
        let queue = PeerQueue::new([addr(1), addr(2)]);
        assert_eq!(queue.push([addr(1), addr(2), addr(3)]), 1);
        assert_eq!(queue.pop(), Some(addr(1)));
        assert_eq!(queue.pop(), Some(addr(2)));
        assert_eq!(queue.pop(), Some(addr(3)));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn an_address_is_retired_once_its_attempts_are_spent() {
        let queue = PeerQueue::new([]);
        for attempt in 1..=MAX_PEER_ATTEMPTS {
            assert_eq!(queue.push([addr(9)]), 1, "attempt {attempt}");
            assert_eq!(queue.pop(), Some(addr(9)));
        }
        // Spent. A tracker naming it again must not buy it another go, or a
        // peer that accepts and hangs up would be dialled for ever.
        assert_eq!(queue.push([addr(9)]), 0);
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn a_requeued_peer_goes_behind_everything_untried() {
        // It has just told us it has nothing for us right now, so anything we
        // have never spoken to deserves a look first.
        let queue = PeerQueue::new([addr(1), addr(2)]);
        let first = queue.pop().unwrap();
        queue.requeue(first);
        assert_eq!(queue.pop(), Some(addr(2)), "the untried one comes first");
        assert_eq!(queue.pop(), Some(addr(1)));
    }

    #[test]
    fn pushing_nothing_reports_nothing_and_wakes_nobody() {
        let queue = PeerQueue::new([addr(1)]);
        assert_eq!(queue.push([]), 0);
        // Already queued, so still nothing new.
        assert_eq!(queue.push([addr(1)]), 0);
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
