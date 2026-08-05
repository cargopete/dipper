//! The JSON API and the static page.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use dipper_bt::wire::Bitfield;
use dipper_bt::{InfoHash, Metainfo};
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
            FileInfo {
                index,
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
}

/// Turn a pasted magnet into a running download.
// One span covering every piece, not a range to be collected.
#[allow(clippy::single_range_in_vec_init)]
pub async fn resolve(
    State(state): State<Arc<AppState>>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<TorrentInfo>, ApiError> {
    let input = torrent::parse_input(&request.magnet, &state.ia).await?;

    // Already going? Hand back what we have rather than starting a second copy
    // of the same download on top of the first one's files.
    if let Some(existing) = state.get(&input.magnet().info_hash) {
        existing.touch();
        return Ok(Json(describe(&existing.meta, existing.is_kept())));
    }

    let (meta, peers) = torrent::resolve(&input, &state.config).await?;
    let root = state.root_for(&meta.info_hash);
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|err| ApiError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    let kept = crate::state::is_marked_kept(&root);
    let started = torrent::start(meta, peers, &root, &state.config).await?;
    started.set_kept(kept);
    if kept {
        // Resuming something already marked for keeping: fetch the lot.
        started
            .handle
            .prioritise(vec![0..started.meta.piece_count()])
            .await;
    }

    let info = describe(&started.meta, kept);
    state.insert(started.meta.info_hash, started);
    Ok(Json(info))
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

fn stats_for(hash: &InfoHash, torrent: &Arc<Torrent>) -> Stats {
    let snapshot = torrent.handle.stats();
    let have = torrent.handle.have();
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
        runs: runs(&have),
    }
}

pub async fn stats(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
) -> Result<Json<Stats>, ApiError> {
    let hash = InfoHash::parse(&hash).map_err(|_| not_found("that is not an infohash"))?;
    let torrent = state.get(&hash).ok_or_else(|| not_found("no such torrent"))?;
    Ok(Json(stats_for(&hash, &torrent)))
}

/// Everything the server currently has on disk or in flight.
pub async fn list(State(state): State<Arc<AppState>>) -> Json<Vec<Stats>> {
    let mut all: Vec<Stats> = state
        .all()
        .iter()
        .map(|(hash, torrent)| stats_for(hash, torrent))
        .collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    Json(all)
}

#[derive(Debug, Deserialize)]
pub struct KeepRequest {
    pub keep: bool,
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
    let torrent = state.get(&hash).ok_or_else(|| not_found("no such torrent"))?;

    torrent.set_kept(request.keep);
    if let Err(err) = mark_kept(&torrent.root, request.keep).await {
        // Worth saying, not worth failing: the flag is still live in memory,
        // it just will not survive a restart.
        tracing::warn!(%err, "could not write the keep marker");
    }
    if request.keep {
        torrent
            .handle
            .prioritise(vec![0..torrent.meta.piece_count()])
            .await;
    }
    torrent.touch();
    Ok(Json(stats_for(&hash, &torrent)))
}

/// Stop a torrent and delete its data.
pub async fn remove(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
) -> Result<StatusCode, ApiError> {
    let hash = InfoHash::parse(&hash).map_err(|_| not_found("that is not an infohash"))?;
    let torrent = state.remove(&hash).ok_or_else(|| not_found("no such torrent"))?;
    discard(&state, &torrent).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Stop the workers and remove the files. The directory belongs to this
/// torrent alone, which is what makes deleting it safe.
async fn discard(state: &AppState, torrent: &Arc<Torrent>) {
    torrent.task.abort();
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

pub async fn index() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (NO_CACHE.0, NO_CACHE.1),
        ],
        include_str!("../assets/index.html"),
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(runs(&bits(10, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9])), vec![0, 10]);
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
