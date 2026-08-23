//! The JSON API and the static page.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use balerion_bt::wire::Bitfield;
use balerion_bt::{InfoHash, Metainfo};
use serde::{Deserialize, Serialize};

use crate::media::{self, Kind, Playback};
use crate::state::{AppState, IDLE_GRACE, Torrent, mark_kept};
use crate::torrent;

/// An error the page can show the user verbatim.
pub struct ApiError(StatusCode, String);

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, message.into())
    }

    pub fn server(message: impl Into<String>) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, message.into())
    }

    /// The request was reasonable but the data has not arrived yet.
    ///
    /// Distinct from a failure on purpose: the caller should wait and ask
    /// again rather than give up, and a player that treats "not yet" as
    /// "never" stops dead a few seconds into a slow torrent.
    pub fn not_ready(message: impl Into<String>) -> Self {
        Self(StatusCode::SERVICE_UNAVAILABLE, message.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let Self(status, message) = self;
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        // The chain matters here: "no peer would serve the torrent metadata:
        // connection refused" is a great deal more use than either half.
        let message = err
            .chain()
            .map(|cause| cause.to_string())
            .collect::<Vec<_>>()
            .join(": ");
        Self(StatusCode::BAD_REQUEST, message)
    }
}

fn not_found(what: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, what.to_string())
}

#[derive(Debug, Serialize)]
pub struct FileInfo {
    pub index: usize,
    pub path: String,
    pub name: String,
    pub length: u64,
    pub playable: bool,
    pub kind: &'static str,
    pub reason: Option<&'static str>,
    /// Read out of the filename when it says, so a season pack can be listed as
    /// episodes rather than as thirteen nearly identical names.
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct TorrentInfo {
    pub infohash: String,
    pub name: String,
    pub total_length: u64,
    pub piece_count: usize,
    pub piece_length: u64,
    pub files: Vec<FileInfo>,
    /// Which file the viewer most likely wants, if any is playable.
    pub suggested: Option<usize>,
    pub webseeds: usize,
    pub kept: bool,
}

fn describe(meta: &Metainfo, kept: bool) -> TorrentInfo {
    let files = meta
        .files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let media = media::classify(&file.path);
            let (season, episode) = match media::episode_of(&file.path) {
                Some((season, episode)) => (Some(season), Some(episode)),
                None => (None, None),
            };
            FileInfo {
                index,
                season,
                episode,
                path: file.path.clone(),
                name: file
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&file.path)
                    .to_string(),
                length: file.length,
                playable: media.playback == Playback::Native,
                kind: match media.kind {
                    Kind::Video => "video",
                    Kind::Audio => "audio",
                    Kind::Other => "other",
                },
                reason: media.reason,
            }
        })
        .collect();

    TorrentInfo {
        infohash: meta.info_hash.to_hex(),
        name: meta.name.clone(),
        total_length: meta.total_length,
        piece_count: meta.piece_count(),
        piece_length: meta.piece_length,
        files,
        suggested: media::best_to_play(&meta.files),
        webseeds: meta.webseeds.len(),
        kept,
    }
}

#[derive(Debug, Deserialize)]
pub struct ResolveRequest {
    pub magnet: String,
    /// Keep this one until it is explicitly deleted.
    ///
    /// The difference between the two ways in. Watching is ephemeral: the
    /// torrent fetches what is ahead of the playhead and the sweep collects it
    /// once nobody is watching. Downloading is not: it fetches the whole file,
    /// writes the marker that survives a restart, and stays until somebody says
    /// otherwise.
    ///
    /// Set here rather than by a second call to `/keep` afterwards, because
    /// between those two calls the torrent is an ordinary unkept one, and a
    /// sweep landing in that gap would collect the download somebody just
    /// asked for.
    #[serde(default)]
    pub keep: bool,
}

/// Turn a pasted magnet into a running download.
// One span covering every piece, not a range to be collected.
#[allow(clippy::single_range_in_vec_init)]
pub async fn resolve(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<TorrentInfo>, ApiError> {
    // The clock starts when the viewer asks, not when the download does: the
    // wait they actually experience includes everything before the first byte.
    let clock = Arc::new(crate::state::Clock::started(std::time::Instant::now()));
    // Said out loud because "I pressed Download and nothing happened" is
    // otherwise indistinguishable between a click that never arrived, a magnet
    // that would not parse, and a swarm that would not answer.
    tracing::info!(
        keep = request.keep,
        asked = request.magnet.chars().take(72).collect::<String>(),
        "asked to resolve"
    );
    let input = torrent::parse_input(&request.magnet, &state.sources.ia).await?;

    // Already going? Hand back what we have rather than starting a second copy
    // of the same download on top of the first one's files.
    if let Some(existing) = state.get(&input.magnet().info_hash) {
        existing.touch();
        // Watched first and downloaded afterwards is the ordinary way round:
        // you start something, decide you want to keep it, and press Download
        // while it is already running.
        if request.keep && !existing.is_kept() {
            make_kept(&existing).await;
        }
        return Ok(Json(describe(&existing.meta, existing.is_kept())));
    }

    let root = state.root_for(&input.magnet().info_hash);

    /* Watched before? Then the file list is already sitting beside the data and
     * there is no reason to ask the swarm for something we know. This is the
     * expensive half of resolving a magnet: discovery is seconds, but BEP 9 is
     * thirty of them when it works and a minute and a half when the swarm has
     * seeders that will send data and none that will answer a metadata
     * request. */
    let input = match crate::library::recall(&root).await {
        Some(mut meta) if meta.info_hash == input.magnet().info_hash => {
            meta.apply_magnet(&input.magnet());
            tracing::debug!(
                name = meta.name,
                "file list read from disk rather than the swarm"
            );
            torrent::Input::Complete(Box::new(meta))
        }
        _ => input,
    };

    // And if the whole thing is already here, it needs no peers whatsoever.
    // Skipping discovery is the difference between opening instantly and
    // waiting out a tracker timeout for a film that is entirely on disk.
    let (meta, peers) = match &input {
        torrent::Input::Complete(meta)
            if crate::library::is_complete_on_disk(&root, meta).await =>
        {
            ((**meta).clone(), Vec::new())
        }
        _ => torrent::resolve(&input, &state.config).await?,
    };

    clock.resolved();

    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|err| ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // Either it was marked kept on a previous run, or this request is the one
    // asking for it.
    let kept = request.keep || crate::state::is_marked_kept(&root);
    let started = torrent::start(
        meta,
        peers,
        &root,
        &state.config,
        state.inbound.clone(),
        clock,
    )
    .await?;
    started.set_kept(kept);
    if kept {
        // Resuming something already marked for keeping, or downloading it for
        // the first time: either way, fetch the lot rather than only what a
        // playhead would need.
        started
            .handle
            .prioritise(vec![0..started.meta.piece_count()])
            .await;
        if request.keep && let Err(err) = mark_kept(&root, true).await {
            // Not fatal: the torrent is kept in memory either way, and the
            // marker only matters across a restart. Worth saying so, because
            // the symptom otherwise is a download quietly vanishing days later.
            tracing::warn!(%err, "could not write the keep marker");
        }
    }

    let info = describe(&started.meta, kept);
    state.insert(started.meta.info_hash, started);
    Ok(Json(info))
}

#[derive(Debug, Deserialize)]
pub struct ProgressRequest {
    /// Seconds into the file.
    pub seconds: f64,
    /// How long the player says the file is. Taken from the browser rather
    /// than from ffprobe because for a direct-played file nothing on this side
    /// has ever measured it.
    pub duration: f64,
}

/// Record where a viewer has got to.
///
/// Called every few seconds while something is playing, so it does as little as
/// possible: the write to disk is batched by a task that runs every ten
/// seconds, and losing the last few of those costs nobody anything.
pub async fn progress(
    State(state): State<Arc<AppState>>,
    Path((hash, file)): Path<(String, usize)>,
    Json(request): Json<ProgressRequest>,
) -> Result<StatusCode, ApiError> {
    let info_hash = InfoHash::parse(&hash).map_err(|_| not_found("that is not an infohash"))?;
    let torrent = state
        .get(&info_hash)
        .ok_or_else(|| not_found("no such torrent"))?;

    // The file's own name, not the torrent's: a season pack is one torrent and
    // twelve different things to be part way through.
    let name = torrent
        .meta
        .files
        .get(file)
        .map(|entry| {
            entry
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&entry.path)
                .to_string()
        })
        .unwrap_or_else(|| torrent.meta.name.clone());

    state
        .history
        .record(&hash, file, &name, request.seconds, request.duration);
    torrent.touch();
    Ok(StatusCode::NO_CONTENT)
}

/// One thing a viewer is part way through.
#[derive(Debug, Serialize)]
pub struct Continuing {
    pub infohash: String,
    pub file: usize,
    pub name: String,
    pub seconds: f64,
    pub duration: f64,
    /// How far through, from 0 to 1, so a page can draw a bar without dividing.
    pub fraction: f64,
    /// Whether the torrent is still on this machine. Something swept a week ago
    /// can still be offered, it simply has to be fetched again first.
    pub held: bool,
}

/// What to offer picking up again.
pub async fn continuing(State(state): State<Arc<AppState>>) -> Json<Vec<Continuing>> {
    let found = state
        .history
        .continuing(24)
        .into_iter()
        .filter_map(|(key, position)| {
            let (hash, file) = key.split_once('/')?;
            let file: usize = file.parse().ok()?;
            let held = InfoHash::parse(hash)
                .ok()
                .is_some_and(|hash| state.get(&hash).is_some());
            Some(Continuing {
                infohash: hash.to_string(),
                file,
                name: position.name.clone(),
                seconds: position.seconds,
                duration: position.duration,
                fraction: position.fraction(),
                held,
            })
        })
        .collect();
    Json(found)
}

/// Where a television should be pointed, when casting is switched on.
#[derive(Debug, Serialize)]
pub struct CastInfo {
    /// The base URL to hand a receiver, or none when casting is off or this
    /// machine has no address another device could reach.
    pub base: Option<String>,
    pub enabled: bool,
}

/// Told to the page so it can offer a link rather than making anyone work out
/// their own address.
pub async fn cast_info(State(state): State<Arc<AppState>>) -> Json<CastInfo> {
    let port = state.config.cast_port;
    let base =
        port.and_then(|port| crate::cast::lan_address().map(|ip| format!("http://{ip}:{port}")));
    Json(CastInfo {
        enabled: port.is_some(),
        base,
    })
}

#[derive(Debug, Serialize)]
pub struct Stats {
    pub infohash: String,
    pub name: String,
    pub pieces_have: usize,
    pub pieces_total: usize,
    /// Fetched during this run only. Kept for the live rate readout.
    pub bytes_downloaded: u64,
    /// Verified and held, including whatever an earlier run left. This is what
    /// progress should be measured against: a resumed torrent has downloaded
    /// almost nothing this session while holding nearly all of the file.
    pub bytes_on_disk: u64,
    pub total_length: u64,
    pub peers: usize,
    pub rate: f64,
    pub failed_hashes: u64,
    pub complete: bool,
    pub kept: bool,
    pub playing: Option<usize>,
    /// How long each stage of getting this playing took, in milliseconds.
    pub timings: crate::state::Timeline,
    /// How the transcoder is coping. Process-wide rather than per torrent,
    /// because the encoders share one machine.
    pub encoder: crate::state::EncoderStats,
    /// How far along converting this one to play anywhere has got, from 0 to
    /// 1, or `None` when nothing is being converted. Absent also covers both
    /// "already done" and "never needed", which `ready` tells apart.
    pub preparing: Option<f64>,
    /// Is there a converted copy sitting ready?
    pub ready: bool,
    /// The file a finished download opens by default, so the shelf can hand
    /// Safari a real media resource within the Watch tap. That is the narrow
    /// window in which WebKit allows its AirPlay receiver picker to open.
    pub suggested: Option<usize>,
    /// A browser-reachable source for that file. It is deliberately a path:
    /// when a receiver is selected the page substitutes the media-only LAN
    /// listener, while the phone continues using the origin it actually knows.
    pub watch_url: Option<String>,
    /// Run lengths of the piece bitmap, starting with a run of missing pieces.
    ///
    /// A run-length encoding rather than a bit per piece: this is polled once
    /// a second, and a 40 GB torrent has enough pieces to make the naive
    /// version genuinely rude.
    pub runs: Vec<usize>,
}

fn runs(have: &Bitfield) -> Vec<usize> {
    let mut runs = Vec::new();
    // Always starts with the count of missing pieces, which is zero when the
    // first piece is already down. The reader can rely on the alternation.
    let mut current = false;
    let mut count = 0usize;
    for index in 0..have.len() {
        if have.has(index) == current {
            count += 1;
        } else {
            runs.push(count);
            current = !current;
            count = 1;
        }
    }
    runs.push(count);
    runs
}

fn stats_for(state: &AppState, hash: &InfoHash, torrent: &Arc<Torrent>) -> Stats {
    let snapshot = torrent.handle.stats();
    let have = torrent.handle.have();
    let suggested = media::best_to_play(&torrent.meta.files);
    let watch_url = suggested.map(|file| {
        if crate::convert::ready(&torrent.root, file).is_some() {
            format!("/ready/{hash}/{file}")
        } else {
            format!("/stream/{hash}/{file}")
        }
    });
    Stats {
        infohash: hash.to_hex(),
        name: torrent.meta.name.clone(),
        pieces_have: snapshot.pieces_have,
        pieces_total: snapshot.pieces_total,
        bytes_downloaded: snapshot.bytes_downloaded,
        bytes_on_disk: snapshot.bytes_on_disk,
        total_length: torrent.meta.total_length,
        peers: snapshot.peers_connected,
        rate: torrent.rate(),
        failed_hashes: snapshot.failed_hashes,
        complete: snapshot.is_complete(),
        kept: torrent.is_kept(),
        playing: *torrent.playing.lock().expect("playing lock"),
        timings: torrent.clock.snapshot(),
        encoder: state.encoder.stats(),
        preparing: state.conversion_of(hash).map(|p| p.fraction()),
        // Any video file in it having a converted copy is enough to say so:
        // that is the one the viewer will open.
        ready: torrent.meta.files.iter().enumerate().any(|(index, entry)| {
            crate::media::classify(&entry.path).kind == crate::media::Kind::Video
                && crate::convert::ready(&torrent.root, index).is_some()
        }),
        suggested,
        watch_url,
        runs: runs(&have),
    }
}

pub async fn stats(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
) -> Result<Json<Stats>, ApiError> {
    let hash = InfoHash::parse(&hash).map_err(|_| not_found("that is not an infohash"))?;
    let torrent = state
        .get(&hash)
        .ok_or_else(|| not_found("no such torrent"))?;
    Ok(Json(stats_for(&state, &hash, &torrent)))
}

/// Open a torrent Balerion already holds without re-resolving its magnet.
///
/// The Downloaded shelf has an infohash, not the original magnet.  Feeding that
/// hash back through the general resolver normally happens to find the live
/// torrent, but it also puts the viewer behind the swarm-shaped loading path.
/// A finished local episode must not mention peers at all.
pub async fn open(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
) -> Result<Json<TorrentInfo>, ApiError> {
    let hash = InfoHash::parse(&hash).map_err(|_| not_found("that is not an infohash"))?;
    let torrent = state
        .get(&hash)
        .ok_or_else(|| not_found("that downloaded item is no longer here"))?;
    torrent.touch();
    Ok(Json(describe(&torrent.meta, torrent.is_kept())))
}

/// Everything the server currently has on disk or in flight.
pub async fn list(State(state): State<Arc<AppState>>) -> Json<Vec<Stats>> {
    // The moment anything kept finishes downloading, start preparing it. This
    // is the poll the page already makes once a second, so it costs nothing
    // and there is no event to wait for.
    for (hash, _) in state.all() {
        prepare_if_ready(&state, &hash);
    }
    let mut all: Vec<Stats> = state
        .all()
        .iter()
        .map(|(hash, torrent)| stats_for(&state, hash, torrent))
        .collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    Json(all)
}

#[derive(Debug, Deserialize)]
pub struct KeepRequest {
    pub keep: bool,
}

/// Turn a running torrent into a kept one: marker on disk, whole file wanted.
///
/// Shared by the `/keep` route and by a `Download` that arrives for something
/// `Watch` already started, so the two ways of asking cannot drift apart.
// One span covering every piece, not a range to be collected.
#[allow(clippy::single_range_in_vec_init)]
async fn make_kept(torrent: &Arc<crate::state::Torrent>) {
    torrent.set_kept(true);
    if let Err(err) = mark_kept(&torrent.root, true).await {
        // Worth saying, not worth failing: the flag is still live in memory,
        // it just will not survive a restart.
        tracing::warn!(%err, "could not write the keep marker");
    }
    torrent
        .handle
        .prioritise(vec![0..torrent.meta.piece_count()])
        .await;
}

/// Convert a kept torrent once it is all here, so it plays anywhere.
///
/// Called on every poll rather than once, because "it has finished
/// downloading" is not an event this side is told about. Cheap: it returns
/// immediately unless the torrent is complete, needs converting, and is not
/// already being converted.
pub fn prepare_if_ready(state: &Arc<AppState>, hash: &InfoHash) {
    let Some(torrent) = state.get(hash) else {
        return;
    };
    if torrent.is_kept() && torrent.handle.stats().is_complete() {
        crate::convert::prepare(state, *hash);
    }
}

/// Mark a torrent to survive the sweep, and fetch all of it rather than only
/// what the playhead needs.
// One span covering every piece, not a range to be collected.
#[allow(clippy::single_range_in_vec_init)]
pub async fn keep(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
    Json(request): Json<KeepRequest>,
) -> Result<Json<Stats>, ApiError> {
    let hash = InfoHash::parse(&hash).map_err(|_| not_found("that is not an infohash"))?;
    let torrent = state
        .get(&hash)
        .ok_or_else(|| not_found("no such torrent"))?;

    if request.keep {
        make_kept(&torrent).await;
        prepare_if_ready(&state, &hash);
    } else {
        torrent.set_kept(false);
        if let Err(err) = mark_kept(&torrent.root, false).await {
            tracing::warn!(%err, "could not write the keep marker");
        }
    }
    torrent.touch();
    Ok(Json(stats_for(&state, &hash, &torrent)))
}

/// Stop a torrent and delete its data.
pub async fn remove(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
) -> Result<StatusCode, ApiError> {
    let hash = InfoHash::parse(&hash).map_err(|_| not_found("that is not an infohash"))?;
    let torrent = state
        .remove(&hash)
        .ok_or_else(|| not_found("no such torrent"))?;
    discard(&state, &torrent).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Stop the workers and remove the files. The directory belongs to this
/// torrent alone, which is what makes deleting it safe.
async fn discard(state: &AppState, torrent: &Arc<Torrent>) {
    torrent.task.abort();
    torrent.refill.abort();
    let root = state.root_for(&torrent.meta.info_hash);
    if root.starts_with(&state.config.data_dir)
        && let Err(err) = tokio::fs::remove_dir_all(&root).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%err, path = %root.display(), "could not remove the torrent directory");
    }
}

/// Periodically drop torrents nobody is watching and nobody asked to keep.
pub async fn sweep(state: Arc<AppState>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    loop {
        ticker.tick().await;
        for (hash, torrent) in state.all() {
            if torrent.is_kept() || torrent.idle_for() < IDLE_GRACE {
                continue;
            }
            tracing::info!(
                name = torrent.meta.name,
                "sweeping an idle torrent nobody asked to keep"
            );
            state.remove(&hash);
            discard(&state, &torrent).await;
        }
    }
}

/// The page and its assets are baked into the binary, so a new build means new
/// assets. Without this a browser happily keeps serving the previous build's
/// JavaScript from cache, and the resulting "I already fixed that" is a
/// thoroughly miserable way to spend an afternoon.
const NO_CACHE: (header::HeaderName, &str) = (header::CACHE_CONTROL, "no-cache");

/// A stamp that changes whenever the assets do.
///
/// `no-cache` on its own was not enough, and the way it failed is instructive:
/// it asks a browser to revalidate, there is no validator to revalidate
/// against, and Chrome is entitled to hand the page its in-memory copy from
/// earlier in the same session. Reloading the page then runs the previous
/// build's JavaScript while the server is serving the new one, and the two
/// disagree about a bug you have already fixed.
///
/// So the page asks for the scripts by a name that changes with their contents.
/// A different build is a different URL and there is nothing to reuse.
fn asset_stamp() -> &'static str {
    use std::sync::OnceLock;
    static STAMP: OnceLock<String> = OnceLock::new();
    STAMP.get_or_init(|| {
        // Not a cryptographic hash and not pretending to be one: it only has to
        // differ when the bytes differ.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in include_str!("../assets/app.js")
            .bytes()
            .chain(include_str!("../assets/lib.js").bytes())
            .chain(include_str!("../assets/app.css").bytes())
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    })
}

pub async fn index() -> impl IntoResponse {
    let stamp = asset_stamp();
    let page = include_str!("../assets/index.html")
        .replace("/app.css\"", &format!("/app.css?v={stamp}\""))
        .replace("/lib.js\"", &format!("/lib.js?v={stamp}\""))
        .replace("/app.js\"", &format!("/app.js?v={stamp}\""));
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (NO_CACHE.0, NO_CACHE.1),
        ],
        page,
    )
}

pub async fn stylesheet() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (NO_CACHE.0, NO_CACHE.1),
        ],
        include_str!("../assets/app.css"),
    )
}

pub async fn script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (NO_CACHE.0, NO_CACHE.1),
        ],
        include_str!("../assets/app.js"),
    )
}

/// The page's arithmetic, kept apart so it can be tested without a browser.
///
/// Loaded before `app.js`, which reads its functions off the window. A separate
/// file rather than a concatenation because `node --test` has to be able to
/// require it, and because a file with tests beside it is a file people keep
/// tested.
pub async fn library_script() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (NO_CACHE.0, NO_CACHE.1),
        ],
        include_str!("../assets/lib.js"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_page_asks_for_its_scripts_by_a_stamped_name() {
        // The trap this closes: a reload that runs the previous build's
        // JavaScript against the new server, and a bug you have already fixed
        // appearing to still be there.
        let body = axum::body::to_bytes(index().await.into_response().into_body(), 1 << 20)
            .await
            .unwrap();
        let page = String::from_utf8(body.to_vec()).unwrap();
        let stamp = asset_stamp();
        assert_eq!(stamp.len(), 16, "sixteen hex characters");
        for asset in ["/app.js", "/lib.js", "/app.css"] {
            assert!(
                page.contains(&format!("{asset}?v={stamp}")),
                "{asset} should be asked for by its stamped name"
            );
            assert!(
                !page.contains(&format!("\"{asset}\"")),
                "{asset} should not also appear unstamped"
            );
        }
    }

    #[test]
    fn the_stamp_does_not_wander_between_calls() {
        assert_eq!(asset_stamp(), asset_stamp());
    }

    fn bits(count: usize, set: &[usize]) -> Bitfield {
        let mut field = Bitfield::empty(count);
        for index in set {
            field.set(*index);
        }
        field
    }

    #[test]
    fn an_empty_torrent_is_one_long_run_of_nothing() {
        assert_eq!(runs(&bits(10, &[])), vec![10]);
    }

    #[test]
    fn a_complete_torrent_leads_with_a_zero_length_gap() {
        // The alternation always starts with missing, so a torrent whose first
        // piece is present opens with a zero. Readers depend on that.
        assert_eq!(
            runs(&bits(10, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])),
            vec![0, 10]
        );
    }

    #[test]
    fn runs_alternate_missing_and_present() {
        // 2 missing, 3 present, 1 missing, 1 present, 3 missing.
        assert_eq!(runs(&bits(10, &[2, 3, 4, 6])), vec![2, 3, 1, 1, 3]);
    }

    #[test]
    fn the_runs_always_add_up_to_the_piece_count() {
        for set in [vec![], vec![0], vec![9], vec![0, 9], vec![1, 3, 5, 7]] {
            let field = bits(10, &set);
            assert_eq!(runs(&field).iter().sum::<usize>(), 10, "set {set:?}");
        }
    }
}
