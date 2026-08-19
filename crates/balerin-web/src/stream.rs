//! Serving bytes that have not arrived yet.
//!
//! The trick that makes this work at all: the browser tells us what it needs
//! by asking for it, so the `Range` header drives which pieces the engine
//! fetches next. A fixed front-to-back order would hang forever on the very
//! common MP4 that keeps its `moov` index box at the end of the file, because
//! the player's first request is for the tail.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use balerin_bt::InfoHash;
use serde::Deserialize;

use crate::media;
use crate::range::{self, Requested};
use crate::state::{AppState, Torrent};

/// Most we will answer a single range request with.
///
/// A player asking `bytes=0-` means "everything from here", and promising the
/// lot would mean holding one response open for the entire film. Answering
/// with a slice is legal, and the player simply asks again.
///
/// Kept modest deliberately. A larger chunk is fewer round trips, but on a
/// thin link it also means one response held open for minutes, which browsers
/// eventually give up on, and a seek cannot interrupt work already committed.
const MAX_CHUNK: u64 = 1024 * 1024;

/// Readahead is expressed in seconds of downloading rather than in bytes.
///
/// A fixed byte figure is wrong at both ends: 24 MB is a trivial prefetch on a
/// fast connection and four minutes of competing traffic on a slow one, where
/// every byte spent ahead of the playhead is a byte not spent on the piece
/// currently blocking playback.
const READAHEAD_SECONDS: f64 = 30.0;
const MIN_READAHEAD: u64 = 2 * 1024 * 1024;
const MAX_READAHEAD: u64 = 48 * 1024 * 1024;

/// Pieces at the end of the file to keep warm for index boxes.
const TAIL_PIECES: usize = 2;

#[derive(Debug, Deserialize)]
pub struct StreamQuery {
    /// Ask for it as a file rather than as something to play inline.
    #[serde(default)]
    pub download: bool,
}

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Path((hash, file_index)): Path<(String, usize)>,
    Query(query): Query<StreamQuery>,
    headers: HeaderMap,
) -> Response {
    let Ok(hash) = InfoHash::parse(&hash) else {
        return (StatusCode::BAD_REQUEST, "that is not an infohash").into_response();
    };
    let Some(torrent) = state.get(&hash) else {
        return (StatusCode::NOT_FOUND, "no such torrent").into_response();
    };
    let Some(file) = torrent.meta.files.get(file_index) else {
        return (StatusCode::NOT_FOUND, "no such file in this torrent").into_response();
    };
    // Copied out rather than borrowed: the torrent has to move into the body
    // stream below, and it outlives this function.
    let (offset, length, path) = (file.offset, file.length, file.path.clone());

    torrent.touch();
    *torrent.playing.lock().expect("playing lock") = Some(file_index);

    let media = media::classify(&path);

    let (start, end, status) = match range::parse(
        headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok()),
        length,
    ) {
        Requested::Unsatisfiable => {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{length}"))],
            )
                .into_response();
        }
        // No Range header at all: a download rather than a player. Serve the
        // whole file, however long that takes.
        Requested::Whole => (0, length, StatusCode::OK),
        Requested::Partial { start, end } => (
            start,
            end.min(start + MAX_CHUNK),
            StatusCode::PARTIAL_CONTENT,
        ),
    };

    let served = end - start;
    prioritise(&torrent, (offset, length), offset + start, served).await;

    let mut response = HeaderMap::new();
    response.insert(header::ACCEPT_RANGES, "bytes".parse().expect("static"));
    response.insert(
        header::CONTENT_TYPE,
        media
            .content_type
            .parse()
            .unwrap_or_else(|_| "application/octet-stream".parse().expect("static")),
    );
    response.insert(
        header::CONTENT_LENGTH,
        served.to_string().parse().expect("a number"),
    );
    // Caching a partially-downloaded torrent would be an excellent way to
    // serve someone a hole later.
    response.insert(header::CACHE_CONTROL, "no-store".parse().expect("static"));
    if status == StatusCode::PARTIAL_CONTENT {
        response.insert(
            header::CONTENT_RANGE,
            // The header is inclusive at both ends; `end` is exclusive here.
            format!("bytes {start}-{}/{length}", end - 1)
                .parse()
                .expect("a range"),
        );
    }
    if query.download {
        let name = path.rsplit('/').next().unwrap_or(&path);
        if let Ok(value) = format!("attachment; filename=\"{}\"", name.replace('"', "")).parse() {
            response.insert(header::CONTENT_DISPOSITION, value);
        }
    }

    (
        status,
        response,
        Body::from_stream(body(torrent, offset + start, offset + end)),
    )
        .into_response()
}

/// Tell the engine what the player is waiting on, most urgent first.
///
/// Order matters enormously on a slow connection. Everything the viewer is not
/// about to watch is competing with the piece that is currently blocking
/// playback, so the whole list is arranged worst-case-first and the torrent at
/// large comes dead last.
async fn prioritise(torrent: &Arc<Torrent>, file: (u64, u64), global_start: u64, served: u64) {
    let (file_offset, file_length) = file;
    let meta = &torrent.meta;

    // Scale the prefetch to what the connection is actually managing. Fetching
    // 24 MB ahead at 200 KB/s means two minutes of work queued in front of the
    // piece the player is stalled on.
    let readahead =
        ((torrent.rate() * READAHEAD_SECONDS) as u64).clamp(MIN_READAHEAD, MAX_READAHEAD);

    let mut spans = vec![
        // What the browser is waiting for right now.
        meta.pieces_for_span(global_start, served),
        // What it will ask for next.
        meta.pieces_for_span(global_start + served, readahead),
    ];

    // The end of the file, for index boxes parked in the tail. Cheap, and the
    // difference between a non-faststart MP4 playing and hanging.
    let tail_start =
        file_offset + file_length.saturating_sub(meta.piece_length * TAIL_PIECES as u64);
    spans.push(meta.pieces_for_span(tail_start, meta.piece_length * TAIL_PIECES as u64));

    // The rest of this file, but nothing else in the torrent. Without this the
    // picker falls through to rarest-first across everything, and a 900 MB
    // extras track starts competing with the film you are watching.
    spans.push(meta.pieces_for_span(file_offset, file_length));

    // Only a torrent the viewer asked to keep earns the right to fetch the
    // files nobody is watching.
    if torrent.is_kept() {
        spans.push(0..meta.piece_count());
    }

    torrent.handle.prioritise(spans).await;
}

/// Yield the requested span piece by piece, waiting for each in turn.
fn body(
    torrent: Arc<Torrent>,
    start: u64,
    end: u64,
) -> impl futures_util::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    let piece_length = torrent.meta.piece_length;

    futures_util::stream::unfold((torrent, start), move |(torrent, next)| async move {
        if next >= end {
            return None;
        }

        let piece = (next / piece_length) as usize;
        if !torrent.handle.wait_for_piece(piece).await {
            // The session stopped before this piece arrived. The bytes on disk
            // are still zeros, and serving them would hand out silent
            // corruption dressed up as a film.
            let err =
                std::io::Error::other(format!("the download stopped before piece {piece} arrived"));
            // Park the cursor at the end so the stream terminates rather than
            // spinning on the same failure.
            return Some((Err(err), (torrent, end)));
        }
        torrent.touch();

        // Read to the end of this piece, or to the end of the request.
        let piece_end = (piece as u64 + 1) * piece_length;
        let to = end.min(piece_end);
        let item = torrent
            .handle
            .read_range(next, to - next)
            .await
            .map_err(std::io::Error::other);
        Some((item, (torrent, to)))
    })
}
