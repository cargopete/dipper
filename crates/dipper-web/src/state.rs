//! What the server holds between requests.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dipper_bt::{DownloadSummary, InfoHash, Metainfo, SessionHandle};
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
}

/// One torrent the server is looking after.
pub struct Torrent {
    pub meta: Metainfo,
    pub handle: SessionHandle,
    pub task: JoinHandle<dipper_bt::Result<DownloadSummary>>,
    pub root: PathBuf,
    /// Set when the viewer asks to keep this offline. Kept torrents fetch the
    /// whole file rather than only what is ahead of the playhead, and survive
    /// the sweep.
    keep: AtomicBool,
    /// Which file the viewer is watching, if any.
    pub playing: Mutex<Option<usize>>,
    last_read: Mutex<Instant>,
    rate: Mutex<Rate>,
}

impl Torrent {
    pub fn new(
        meta: Metainfo,
        handle: SessionHandle,
        task: JoinHandle<dipper_bt::Result<DownloadSummary>>,
        root: PathBuf,
    ) -> Self {
        Self {
            meta,
            handle,
            task,
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

pub struct AppState {
    pub config: ServeConfig,
    /// Used only to turn an archive.org identifier into a torrent. Held here
    /// rather than built per request so its rate limiting actually applies.
    pub ia: dipper_ia::IaClient,
    torrents: Mutex<HashMap<InfoHash, Arc<Torrent>>>,
}

impl AppState {
    pub fn new(config: ServeConfig) -> Self {
        Self {
            config,
            ia: dipper_ia::IaClient::new().expect("the HTTP client failed to build"),
            torrents: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, hash: &InfoHash) -> Option<Arc<Torrent>> {
        self.torrents.lock().expect("torrents lock").get(hash).cloned()
    }

    pub fn insert(&self, hash: InfoHash, torrent: Arc<Torrent>) {
        self.torrents
            .lock()
            .expect("torrents lock")
            .insert(hash, torrent);
    }

    pub fn remove(&self, hash: &InfoHash) -> Option<Arc<Torrent>> {
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
/// dipper acquiring a state store to go wrong.
const KEEP_MARKER: &str = ".dipper-keep";

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
    fn a_fresh_rate_reports_the_first_sample_directly() {
        let mut rate = Rate::new();
        // Too soon to divide by, so nothing is claimed yet.
        assert_eq!(rate.sample(1000), 0.0);

        std::thread::sleep(Duration::from_millis(300));
        let measured = rate.sample(1000);
        assert!(measured > 2_000.0, "roughly 1000 bytes over 0.3s, got {measured}");
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
        assert!(rate.smoothed < 1_000.0, "stalled but still claiming {}", rate.smoothed);
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
