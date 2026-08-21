//! What the server holds between requests.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use balerion_bt::{DownloadSummary, InfoHash, Metainfo, SessionHandle};
use tokio::task::JoinHandle;

/// How long an unwatched, unkept torrent lingers before it is swept.
///
/// Not zero, because closing the tab at 90% and losing the lot would be
/// maddening, and not never, because otherwise a browse through five films
/// quietly costs you twenty gigabytes.
pub const IDLE_GRACE: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub host: IpAddr,
    pub port: u16,
    /// Each torrent gets `data_dir/<infohash>/` to itself.
    ///
    /// One directory per torrent rather than one shared one, so sweeping an
    /// abandoned download is a `remove_dir_all` of a directory nothing else
    /// writes to. Sharing a root would make deletion a matter of working out
    /// which files belong to whom, and getting that wrong deletes a film
    /// somebody is watching.
    pub data_dir: PathBuf,
    pub max_peers: usize,
    /// Blocks of 16 KiB kept in flight per peer.
    ///
    /// The default of 16 means a quarter of a megabyte outstanding per peer,
    /// which across forty peers is ten megabytes of queued requests. On a fast
    /// link that is exactly right. On a thin one it is a minute of other
    /// people's blocks sitting in front of the piece playback is stalled on,
    /// so [`ServeConfig::thin_pipe`] winds both figures down.
    pub pipeline_depth: usize,
    pub use_dht: bool,
    pub use_webseeds: bool,
    pub peer_port: u16,
    pub dht_budget: Duration,
    pub tracker_timeout: Duration,
    /// Required of every request that did not come from this machine.
    ///
    /// `None` on loopback, which is the default and the whole point: a password
    /// on your own machine protects you from nobody. Set whenever `--host`
    /// binds the player somewhere other people can reach, because `/api/resolve`
    /// downloads whatever it is handed.
    pub access_token: Option<String>,
    /// Port for the media-only listener a television fetches from, when one is
    /// wanted. `None` keeps everything on loopback, which is the default
    /// because it is the only setting that exposes nothing.
    pub cast_port: Option<u16>,
}

/// When the interesting things happened, measured from the moment the viewer
/// asked for this.
///
/// Every performance claim about balerion used to be reasoning from the code
/// rather than from a stopwatch, which is a poor way to decide what to spend a
/// week on. There is no way to tell a slow swarm from a slow disk from a slow
/// encoder without numbers, so here are the four that matter.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct Timeline {
    /// Turning what was pasted into a file list: discovery, and BEP 9 when the
    /// answer was not already on disk. Usually the longest of the four.
    pub resolved_ms: Option<u64>,
    /// First peer handshaked and trading.
    pub first_peer_ms: Option<u64>,
    /// First piece verified and written.
    pub first_piece_ms: Option<u64>,
    /// First byte handed to the player, which is the one a viewer feels.
    pub first_byte_ms: Option<u64>,
}

/// The timeline plus the clock it is measured against.
#[derive(Debug)]
pub struct Clock {
    asked_at: Instant,
    marks: Mutex<Timeline>,
}

impl Clock {
    pub fn started(asked_at: Instant) -> Self {
        Self {
            asked_at,
            marks: Mutex::new(Timeline::default()),
        }
    }

    pub fn snapshot(&self) -> Timeline {
        *self.marks.lock().expect("timeline lock")
    }

    fn since_asked(&self) -> u64 {
        self.asked_at.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    /// Record a milestone, keeping the first answer.
    ///
    /// First rather than latest throughout: these are "when did it start
    /// working", and overwriting them with the most recent peer or piece would
    /// turn a milestone into a heartbeat.
    fn mark(&self, pick: impl Fn(&mut Timeline) -> &mut Option<u64>) {
        let mut marks = self.marks.lock().expect("timeline lock");
        let slot = pick(&mut marks);
        if slot.is_none() {
            *slot = Some(self.since_asked());
        }
    }

    pub fn resolved(&self) {
        self.mark(|marks| &mut marks.resolved_ms);
    }

    pub fn first_peer(&self) {
        self.mark(|marks| &mut marks.first_peer_ms);
    }

    pub fn first_piece(&self) {
        self.mark(|marks| &mut marks.first_piece_ms);
    }

    pub fn first_byte(&self) {
        self.mark(|marks| &mut marks.first_byte_ms);
    }
}

/// One torrent the server is looking after.
pub struct Torrent {
    pub meta: Metainfo,
    pub handle: SessionHandle,
    pub task: JoinHandle<balerion_bt::Result<DownloadSummary>>,
    /// The loop that goes back to the trackers for more peers. Aborted with the
    /// download: an announce for a torrent nobody is watching any more is a
    /// request to a stranger's server on behalf of nothing.
    pub refill: JoinHandle<()>,
    pub root: PathBuf,
    /// Set when the viewer asks to keep this offline. Kept torrents fetch the
    /// whole file rather than only what is ahead of the playhead, and survive
    /// the sweep.
    keep: AtomicBool,
    /// Which file the viewer is watching, if any.
    pub playing: Mutex<Option<usize>>,
    /// How long each stage of getting this playing actually took.
    pub clock: Arc<Clock>,
    last_read: Mutex<Instant>,
    rate: Mutex<Rate>,
}

impl Torrent {
    pub fn new(
        meta: Metainfo,
        handle: SessionHandle,
        task: JoinHandle<balerion_bt::Result<DownloadSummary>>,
        refill: JoinHandle<()>,
        root: PathBuf,
        clock: Arc<Clock>,
    ) -> Self {
        Self {
            clock,
            meta,
            handle,
            task,
            refill,
            root,
            keep: AtomicBool::new(false),
            playing: Mutex::new(None),
            last_read: Mutex::new(Instant::now()),
            rate: Mutex::new(Rate::new()),
        }
    }

    pub fn is_kept(&self) -> bool {
        self.keep.load(Ordering::Relaxed)
    }

    pub fn set_kept(&self, kept: bool) {
        self.keep.store(kept, Ordering::Relaxed);
    }

    /// Called whenever a reader touches this torrent, to stay the sweep.
    pub fn touch(&self) {
        *self.last_read.lock().expect("rate lock") = Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.last_read.lock().expect("rate lock").elapsed()
    }

    /// Bytes per second, smoothed. Sampled here rather than in the engine so
    /// the engine stays a thing that downloads rather than a thing that also
    /// keeps statistics for a user interface.
    pub fn rate(&self) -> f64 {
        let downloaded = self.handle.stats().bytes_downloaded;
        self.rate.lock().expect("rate lock").sample(downloaded)
    }
}

/// Exponentially smoothed download rate.
struct Rate {
    at: Instant,
    bytes: u64,
    smoothed: f64,
}

impl Rate {
    fn new() -> Self {
        Self {
            at: Instant::now(),
            bytes: 0,
            smoothed: 0.0,
        }
    }

    fn sample(&mut self, bytes: u64) -> f64 {
        let elapsed = self.at.elapsed().as_secs_f64();
        // Below a quarter second the divisor is small enough to produce
        // nonsense, so hold the previous answer.
        if elapsed >= 0.25 {
            let instant = bytes.saturating_sub(self.bytes) as f64 / elapsed;
            // Weighted towards the latest sample. Heavier smoothing looks
            // calmer but keeps claiming throughput seconds after a download
            // has plainly stopped, which is the one thing a rate readout must
            // not do.
            self.smoothed = if self.bytes == 0 && self.smoothed == 0.0 {
                instant
            } else {
                self.smoothed * 0.4 + instant * 0.6
            };
            self.at = Instant::now();
            self.bytes = bytes;
        }
        self.smoothed
    }
}

/// How the transcoder is coping, across the whole process.
///
/// One number decides whether a file can be watched as it converts or only
/// downloaded and watched afterwards: how long ffmpeg takes to produce six
/// seconds of video compared with six seconds. Under 1.0 and playback will
/// stall no matter how fast the swarm is, which until this existed presented as
/// a mysterious buffering problem that looked like the network's fault.
#[derive(Debug, Default)]
pub struct Encoder {
    segments: AtomicU64,
    total_ms: AtomicU64,
    slowest_ms: AtomicU64,
}

/// What the transcoder has managed so far.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct EncoderStats {
    pub segments: u64,
    /// Mean wall-clock milliseconds per segment.
    pub mean_ms: u64,
    pub slowest_ms: u64,
    /// Seconds of video produced per second of encoding. Below 1.0, the
    /// encoder is the bottleneck and no amount of bandwidth will help.
    pub realtime: f64,
}

impl Encoder {
    pub fn record(&self, took: Duration) {
        let ms = took.as_millis().min(u64::MAX as u128) as u64;
        self.segments.fetch_add(1, Ordering::Relaxed);
        self.total_ms.fetch_add(ms, Ordering::Relaxed);
        self.slowest_ms.fetch_max(ms, Ordering::Relaxed);
    }

    pub fn stats(&self) -> EncoderStats {
        let segments = self.segments.load(Ordering::Relaxed);
        let total_ms = self.total_ms.load(Ordering::Relaxed);
        let mean_ms = total_ms.checked_div(segments).unwrap_or(0);
        EncoderStats {
            segments,
            mean_ms,
            slowest_ms: self.slowest_ms.load(Ordering::Relaxed),
            realtime: if mean_ms == 0 {
                0.0
            } else {
                crate::ffmpeg::SEGMENT_SECONDS * 1000.0 / mean_ms as f64
            },
        }
    }
}

/// How much encoded video to keep, in bytes.
///
/// Bounded by size rather than by count, which the previous figure of "24
/// segments" was not: a six second segment is two megabytes of a Prelinger
/// short and twelve of a 4K feature, so counting them bounded memory at
/// anywhere between fifty and three hundred megabytes depending on what
/// somebody happened to be watching.
///
/// Deliberately not a disk cache. Keeping every segment of a transcode beside
/// the torrent would roughly double what a converted film costs to hold, which
/// is a poor trade for making a scrub backwards instant.
const SEGMENT_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// Concurrent ffmpeg processes.
///
/// A viewer dragging the scrubber can ask for a great many segments at once,
/// and every encoder competes with the download that feeds it. Derived from the
/// machine rather than fixed at three, which was fine on a laptop and arbitrary
/// on anything else: a quarter of the cores, never fewer than two, never more
/// than six, because past that they are contending for memory bandwidth rather
/// than getting anything done.
fn max_transcodes() -> usize {
    std::thread::available_parallelism()
        .map(|cores| (cores.get() / 4).clamp(2, 6))
        .unwrap_or(3)
}

pub struct AppState {
    pub config: ServeConfig,
    /// The clients every search goes through, and the same set the relay
    /// carries. One shape rather than three fields, so the player and the relay
    /// cannot drift apart about what a search can reach.
    pub sources: Arc<crate::find::Sources>,
    /// ffmpeg, if this machine has it. `None` disables transcoding and the
    /// player falls back to offering downloads.
    pub tools: Option<crate::ffmpeg::Tools>,
    /// Where every viewer got to, across restarts and across torrents that
    /// have since been swept.
    pub history: Arc<crate::history::History>,
    /// whisper.cpp, when this machine has it and a model.
    ///
    /// The last resort for subtitles and the only one that always works: a
    /// transcript needs no account, no allowance and no luck, and it is in step
    /// with the audio by construction because it came from the audio.
    pub whisper: Option<crate::whisper::Whisper>,
    /// Files currently being transcribed, so two requests cannot start two
    /// transcriptions of the same thing and race to write the same file.
    transcribing: Mutex<std::collections::HashSet<(InfoHash, usize)>>,
    /// OpenSubtitles, when a key is configured.
    ///
    /// `None` is an ordinary state to be in: the player then shows whatever
    /// subtitles came in the torrent and offers nothing else, which is exactly
    /// what it did before.
    pub osdb: Option<balerion_osdb::OsdbClient>,
    /// The socket peers dial to reach us, when one could be bound.
    ///
    /// `None` means outbound connections only, which works and finds fewer
    /// peers. Every session started here claims its share of it, so one port
    /// serves every torrent in the process.
    pub inbound: Option<balerion_bt::Inbound>,
    /// Where this server can be reached, so ffmpeg can read back through our
    /// own range endpoint and inherit the piece prioritisation for free.
    pub self_base: Mutex<String>,
    pub transcodes: tokio::sync::Semaphore,
    /// How the transcoder is keeping up, which is the other half of "can this
    /// be watched live" and until now the unmeasured half.
    pub encoder: Encoder,
    torrents: Mutex<HashMap<InfoHash, Arc<Torrent>>>,
    probes: Mutex<HashMap<(InfoHash, usize), Arc<crate::ffmpeg::Probe>>>,
    /// What OpenSubtitles has for a file, remembered so that asking twice
    /// about the same thing does not cost two requests against their limit.
    offers: Mutex<HashMap<(InfoHash, usize), Option<crate::fetched::Offer>>>,
    /// Subtitle alignments, keyed by the video and the subtitle file.
    ///
    /// Worth remembering because working one out means decoding a quarter of an
    /// hour of audio, and a browser asks for a subtitle track again on every
    /// reload. `None` records that we looked and could not tell, which is just
    /// as much worth not repeating.
    alignments: Mutex<HashMap<(InfoHash, usize, usize), Option<crate::subsync::Alignment>>>,
    segments: Mutex<SegmentCache>,
}

/// Torrent, file, segment, and which audio track it carries.
///
/// The audio track is part of the key because switching tracks must not be
/// answered out of the cache with segments carrying the old one, which is a
/// fault that presents as the feature simply not working.
type SegmentKey = (InfoHash, usize, u32, usize);

/// A bounded cache of generated segments, oldest evicted first.
#[derive(Default)]
struct SegmentCache {
    order: std::collections::VecDeque<SegmentKey>,
    data: HashMap<SegmentKey, Arc<Vec<u8>>>,
    held: usize,
}

impl SegmentCache {
    /// Recompute what is held. Called after any removal that was not an
    /// eviction, so the running total cannot drift away from the truth.
    fn recount(&mut self) {
        self.held = self.data.values().map(|data| data.len()).sum();
    }
}

impl AppState {
    pub fn new(config: ServeConfig) -> Self {
        Self {
            config,
            sources: Arc::new(crate::find::Sources::from_env()),
            tools: None,
            osdb: balerion_osdb::OsdbClient::from_env(),
            whisper: None,
            // Replaced by `serve` with whatever is on disk. Empty here so that
            // a test or a fixture never has to touch the filesystem.
            history: Arc::new(crate::history::History::empty()),
            transcribing: Mutex::new(std::collections::HashSet::new()),
            inbound: None,
            self_base: Mutex::new(String::new()),
            transcodes: tokio::sync::Semaphore::new(max_transcodes()),
            encoder: Encoder::default(),
            torrents: Mutex::new(HashMap::new()),
            probes: Mutex::new(HashMap::new()),
            offers: Mutex::new(HashMap::new()),
            alignments: Mutex::new(HashMap::new()),
            segments: Mutex::new(SegmentCache::default()),
        }
    }

    /// The URL ffmpeg should read a file from: our own range endpoint.
    ///
    /// Carries the access token when there is one, and that is not belt and
    /// braces. The guard lets loopback through untouched, which covers the
    /// ordinary case where the server is bound to `0.0.0.0` and therefore
    /// addresses itself as `127.0.0.1`. Bind it to one specific address —
    /// a Tailscale address, say, which is the sensible thing to do on a machine
    /// that is always on — and it addresses itself as *that*, which is not
    /// loopback, and the transcoder is refused by its own server. Every file
    /// needing conversion then fails with a 401 that looks like a broken
    /// ffmpeg.
    pub fn stream_url(&self, hash: &str, file: usize) -> String {
        // Once every piece is on disk, read the file. Going back out through
        // our own endpoint for bytes already sitting on the filesystem is a
        // detour, and not a harmless one: ffmpeg seeks an HTTP source by
        // closing the connection and opening another one at an offset it has
        // guessed, and it guesses badly enough on a large MKV that some
        // segments decode from the wrong place and one in every few hundred
        // produces no fragment at all. Measured on a 605 MiB episode: segment
        // 410 returned `ffmpeg produced no fragment` over HTTP, reproducibly,
        // and 1,049,572 bytes of perfectly good video from the same file on
        // disk with the same arguments.
        if let Some(path) = InfoHash::parse(hash)
            .ok()
            .and_then(|hash| self.local_source(&hash, file))
        {
            return path;
        }
        let base = self.self_base.lock().expect("self_base lock").clone();
        match &self.config.access_token {
            Some(token) => format!(
                "{base}/stream/{hash}/{file}?{}={token}",
                crate::access::TOKEN
            ),
            None => format!("{base}/stream/{hash}/{file}"),
        }
    }

    /// Where this file really is, when the whole of it is already there.
    ///
    /// `None` while anything is still missing, because a sparse file reads as
    /// zeroes rather than as an error and ffmpeg would happily encode the
    /// silence. The HTTP endpoint is the one that knows how to wait for a
    /// piece, so a partial download keeps going through it.
    pub fn local_source(&self, hash: &InfoHash, file: usize) -> Option<String> {
        let torrent = self.get(hash)?;
        if !torrent.handle.stats().is_complete() {
            return None;
        }
        self.file_path(hash, file)
    }

    /// Where this file is on disk, whether or not all of it has arrived.
    ///
    /// Separate from [`AppState::local_source`] because a caller that has
    /// checked for itself which pieces it needs knows better than a blanket
    /// "is the whole torrent here". A caller that has not must use the other
    /// one: a sparse file reads as zeroes rather than as an error.
    pub fn file_path(&self, hash: &InfoHash, file: usize) -> Option<String> {
        let torrent = self.get(hash)?;
        let entry = torrent.meta.files.get(file)?;
        let path = balerion_bt::storage::safe_join(&torrent.root, &entry.path).ok()?;
        // An absolute path is unambiguous to ffmpeg; a relative one could be
        // read as a protocol, and we never have one here anyway.
        path.is_absolute()
            .then(|| path.to_str().map(str::to_string))
            .flatten()
    }

    /// Probe a file, remembering the answer. What is in a file never changes,
    /// and probing costs a round trip plus however long the header takes to
    /// arrive from the swarm.
    pub async fn probe(
        &self,
        tools: &crate::ffmpeg::Tools,
        hash: &InfoHash,
        file: usize,
    ) -> anyhow::Result<Arc<crate::ffmpeg::Probe>> {
        if let Some(cached) = self.probes.lock().expect("probes lock").get(&(*hash, file)) {
            return Ok(Arc::clone(cached));
        }
        let probe = Arc::new(tools.probe(&self.stream_url(&hash.to_hex(), file)).await?);
        self.probes
            .lock()
            .expect("probes lock")
            .insert((*hash, file), Arc::clone(&probe));
        Ok(probe)
    }

    /// Claim the right to transcribe one file.
    ///
    /// Returns false when somebody already holds it. The claim is released by
    /// [`AppState::finished_transcribing`], which the job must call whatever
    /// happens to it, or the file can never be attempted again.
    pub fn begin_transcribing(&self, hash: &InfoHash, file: usize) -> bool {
        self.transcribing
            .lock()
            .expect("transcribing lock")
            .insert((*hash, file))
    }

    pub fn finished_transcribing(&self, hash: &InfoHash, file: usize) {
        self.transcribing
            .lock()
            .expect("transcribing lock")
            .remove(&(*hash, file));
    }

    pub fn is_transcribing(&self, hash: &InfoHash, file: usize) -> bool {
        self.transcribing
            .lock()
            .expect("transcribing lock")
            .contains(&(*hash, file))
    }

    /// What a previous look found, if we have looked.
    pub fn cached_offer(
        &self,
        hash: &InfoHash,
        file: usize,
    ) -> Option<Option<crate::fetched::Offer>> {
        self.offers
            .lock()
            .expect("offers lock")
            .get(&(*hash, file))
            .cloned()
    }

    pub fn remember_offer(
        &self,
        hash: &InfoHash,
        file: usize,
        offer: Option<crate::fetched::Offer>,
    ) {
        self.offers
            .lock()
            .expect("offers lock")
            .insert((*hash, file), offer);
    }

    /// The alignment worked out for one subtitle file, if we have one.
    ///
    /// The outer option is "have we looked"; the inner is "did we find
    /// anything we believe".
    pub fn cached_alignment(
        &self,
        hash: &InfoHash,
        file: usize,
        subtitle: usize,
    ) -> Option<Option<crate::subsync::Alignment>> {
        self.alignments
            .lock()
            .expect("alignments lock")
            .get(&(*hash, file, subtitle))
            .copied()
    }

    pub fn remember_alignment(
        &self,
        hash: &InfoHash,
        file: usize,
        subtitle: usize,
        alignment: Option<crate::subsync::Alignment>,
    ) {
        self.alignments
            .lock()
            .expect("alignments lock")
            .insert((*hash, file, subtitle), alignment);
    }

    pub fn cached_segment(
        &self,
        hash: &InfoHash,
        file: usize,
        index: u32,
        audio: usize,
    ) -> Option<Arc<Vec<u8>>> {
        self.segments
            .lock()
            .expect("segments lock")
            .data
            .get(&(*hash, file, index, audio))
            .map(Arc::clone)
    }

    pub fn cache_segment(
        &self,
        hash: &InfoHash,
        file: usize,
        index: u32,
        audio: usize,
        data: Arc<Vec<u8>>,
    ) {
        let mut cache = self.segments.lock().expect("segments lock");
        let key = (*hash, file, index, audio);
        let size = data.len();
        match cache.data.insert(key, data) {
            Some(previous) => cache.held = cache.held + size - previous.len(),
            None => {
                cache.order.push_back(key);
                cache.held += size;
            }
        }

        while cache.held > SEGMENT_CACHE_BYTES && cache.order.len() > 1 {
            let Some(oldest) = cache.order.pop_front() else {
                break;
            };
            if let Some(evicted) = cache.data.remove(&oldest) {
                cache.held = cache.held.saturating_sub(evicted.len());
            }
        }
    }

    /// Forget everything remembered about a torrent that is going away.
    fn forget(&self, hash: &InfoHash) {
        self.probes
            .lock()
            .expect("probes lock")
            .retain(|(held, _), _| held != hash);
        self.alignments
            .lock()
            .expect("alignments lock")
            .retain(|(held, _, _), _| held != hash);
        self.offers
            .lock()
            .expect("offers lock")
            .retain(|(held, _), _| held != hash);
        let mut cache = self.segments.lock().expect("segments lock");
        cache.data.retain(|(held, _, _, _), _| held != hash);
        cache.order.retain(|(held, _, _, _)| held != hash);
        cache.recount();
    }

    pub fn get(&self, hash: &InfoHash) -> Option<Arc<Torrent>> {
        self.torrents
            .lock()
            .expect("torrents lock")
            .get(hash)
            .cloned()
    }

    pub fn insert(&self, hash: InfoHash, torrent: Arc<Torrent>) {
        self.torrents
            .lock()
            .expect("torrents lock")
            .insert(hash, torrent);
    }

    pub fn remove(&self, hash: &InfoHash) -> Option<Arc<Torrent>> {
        // Drop the probe and any cached segments with it, or a torrent
        // re-added later would be served another one's frames.
        self.forget(hash);
        self.torrents.lock().expect("torrents lock").remove(hash)
    }

    pub fn all(&self) -> Vec<(InfoHash, Arc<Torrent>)> {
        self.torrents
            .lock()
            .expect("torrents lock")
            .iter()
            .map(|(hash, torrent)| (*hash, Arc::clone(torrent)))
            .collect()
    }

    /// Where one torrent's files live.
    pub fn root_for(&self, hash: &InfoHash) -> PathBuf {
        self.config.data_dir.join(hash.to_hex())
    }
}

/// Marker file recording that a torrent should survive the sweep.
///
/// A file rather than a database, so the keep flag outlives a restart without
/// balerion acquiring a state store to go wrong.
const KEEP_MARKER: &str = ".balerion-keep";

pub fn keep_marker(root: &Path) -> PathBuf {
    root.join(KEEP_MARKER)
}

pub fn is_marked_kept(root: &Path) -> bool {
    keep_marker(root).exists()
}

pub async fn mark_kept(root: &Path, kept: bool) -> std::io::Result<()> {
    let marker = keep_marker(root);
    if kept {
        tokio::fs::write(&marker, b"kept by the viewer\n").await
    } else {
        match tokio::fs::remove_file(&marker).await {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_segment_cache_is_bounded_by_bytes_not_by_count() {
        // The old bound was a count, which meant anywhere between fifty and
        // three hundred megabytes depending on what somebody was watching.
        let state = AppState::new(ServeConfig::default());
        let hash = InfoHash::new([1u8; 20]);
        let big = Arc::new(vec![0u8; 64 * 1024 * 1024]);

        for index in 0..8u32 {
            state.cache_segment(&hash, 0, index, 0, Arc::clone(&big));
        }

        let held = state.segments.lock().unwrap().held;
        assert!(
            held <= SEGMENT_CACHE_BYTES,
            "holding {held} bytes, cap is {SEGMENT_CACHE_BYTES}"
        );
        // The most recent one must survive: it is the one being played.
        assert!(state.cached_segment(&hash, 0, 7, 0).is_some());
        assert!(
            state.cached_segment(&hash, 0, 0, 0).is_none(),
            "oldest goes first"
        );
    }

    #[test]
    fn re_caching_the_same_segment_does_not_double_count_it() {
        let state = AppState::new(ServeConfig::default());
        let hash = InfoHash::new([2u8; 20]);
        let data = Arc::new(vec![0u8; 1024]);

        state.cache_segment(&hash, 0, 0, 0, Arc::clone(&data));
        state.cache_segment(&hash, 0, 0, 0, Arc::clone(&data));
        assert_eq!(state.segments.lock().unwrap().held, 1024);
    }

    #[test]
    fn forgetting_a_torrent_gives_its_bytes_back() {
        let state = AppState::new(ServeConfig::default());
        let hash = InfoHash::new([3u8; 20]);
        state.cache_segment(&hash, 0, 0, 0, Arc::new(vec![0u8; 4096]));
        state.forget(&hash);
        assert_eq!(
            state.segments.lock().unwrap().held,
            0,
            "the running total must not drift away from what is actually held"
        );
    }

    #[test]
    fn a_fresh_rate_reports_the_first_sample_directly() {
        let mut rate = Rate::new();
        // Too soon to divide by, so nothing is claimed yet.
        assert_eq!(rate.sample(1000), 0.0);

        std::thread::sleep(Duration::from_millis(300));
        let measured = rate.sample(1000);
        assert!(
            measured > 2_000.0,
            "roughly 1000 bytes over 0.3s, got {measured}"
        );
    }

    #[test]
    fn a_stalled_download_decays_towards_zero() {
        let mut rate = Rate::new();
        std::thread::sleep(Duration::from_millis(300));
        rate.sample(100_000);

        // Nothing arrives for a while: the reported rate must come down.
        for _ in 0..8 {
            std::thread::sleep(Duration::from_millis(300));
            rate.sample(100_000);
        }
        assert!(
            rate.smoothed < 1_000.0,
            "stalled but still claiming {}",
            rate.smoothed
        );
    }

    #[test]
    fn counters_that_go_backwards_do_not_panic() {
        // Defensive: saturating_sub means a reset counter reads as zero
        // rather than wrapping to something absurd.
        let mut rate = Rate::new();
        std::thread::sleep(Duration::from_millis(300));
        rate.sample(5_000);
        std::thread::sleep(Duration::from_millis(300));
        assert!(rate.sample(10).is_finite());
    }
}
