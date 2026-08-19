//! Playback: how to play a file, and the pieces of it MSE needs.
//!
//! For a file a browser already opens, this reports `direct` and the page uses
//! the ordinary `/stream` endpoint, which is the fast, well-worn path. For
//! anything else it reports `transcode` and the page drives a `MediaSource`
//! against the segment endpoints below.
//!
//! Segments are generated on demand and nothing is retained between requests.
//! That makes seeking free (ask for any segment at any time) and leaves no
//! session to leak when a viewer closes the tab.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use balerin_bt::InfoHash;
use serde::Serialize;

use crate::ffmpeg::{self, Plan, Probe};
use crate::fmp4;
use crate::media::{self, Playback};
use crate::routes::ApiError;
use crate::state::{AppState, Torrent};
use crate::subtitles;

/// How the page should play a file.
#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum PlayInfo {
    /// Browsers open this as it is. Use `/stream`.
    Direct {
        url: String,
        /// Size of this file, so the page can work out its bitrate once the
        /// browser reports a duration, and say whether the swarm can keep up.
        length: u64,
        tracks: Vec<Track>,
    },
    /// Needs converting. Drive MediaSource against the segment endpoints.
    Transcode {
        mime: String,
        duration: f64,
        /// Size of this file. Divided by duration this gives the bitrate
        /// playback must be fed at, which is the only figure that decides
        /// whether a torrent can be watched live or merely downloaded.
        length: u64,
        segments: u32,
        segment_seconds: f64,
        init: String,
        segment_prefix: String,
        tracks: Vec<Track>,
        /// True when only the wrapper is being changed, which is worth saying
        /// because it is the cheap case and the quality is untouched.
        remux_only: bool,
    },
    /// Nothing we can do for this one.
    Unsupported { reason: String, download: String },
}

/// A subtitle track the page can offer.
#[derive(Debug, Serialize)]
pub struct Track {
    pub url: String,
    pub label: String,
    pub language: Option<String>,
}

fn parse(hash: &str) -> Result<InfoHash, ApiError> {
    InfoHash::parse(hash).map_err(|_| ApiError::not_found("that is not an infohash"))
}

fn torrent(state: &AppState, hash: &InfoHash) -> Result<Arc<Torrent>, ApiError> {
    state
        .get(hash)
        .ok_or_else(|| ApiError::not_found("no such torrent"))
}

/// Work out how to play a file, probing it if need be.
pub async fn info(
    State(state): State<Arc<AppState>>,
    Path((hash, file)): Path<(String, usize)>,
) -> Result<Json<PlayInfo>, ApiError> {
    let info_hash = parse(&hash)?;
    let torrent = torrent(&state, &info_hash)?;
    let entry = torrent
        .meta
        .files
        .get(file)
        .ok_or_else(|| ApiError::not_found("no such file in this torrent"))?;
    torrent.touch();

    let stream_url = format!("/stream/{hash}/{file}");
    let download = format!("{stream_url}?download=true");
    let classified = media::classify(&entry.path);
    let tracks = sidecar_tracks(&torrent, file, &hash);

    // The cheap path, and the common one.
    if classified.playback == Playback::Native {
        return Ok(Json(PlayInfo::Direct {
            url: stream_url,
            length: entry.length,
            tracks,
        }));
    }

    if classified.playback == Playback::NotMedia {
        return Ok(Json(PlayInfo::Unsupported {
            reason: "this is not a video or audio file".into(),
            download,
        }));
    }

    let Some(tools) = state.tools.clone() else {
        return Ok(Json(PlayInfo::Unsupported {
            reason: "this container needs converting, and ffmpeg was not found on your PATH. \
                     Install ffmpeg and restart balerin, or download the file and play it in VLC."
                .into(),
            download,
        }));
    };

    let probe = match state.probe(&tools, &info_hash, file).await {
        Ok(probe) => probe,
        Err(err) => {
            return Ok(Json(PlayInfo::Unsupported {
                reason: format!("could not read this file: {err}"),
                download,
            }));
        }
    };

    if probe.video.is_none() && probe.audio.is_none() {
        return Ok(Json(PlayInfo::Unsupported {
            reason: "there is no video or audio track in this file".into(),
            download,
        }));
    }
    if probe.duration <= 0.0 {
        return Ok(Json(PlayInfo::Unsupported {
            reason: "this file has no duration, so it cannot be played in segments".into(),
            download,
        }));
    }

    let plan = ffmpeg::plan(&probe);
    let mut tracks = tracks;
    tracks.extend(embedded_tracks(&probe, &hash, file));

    Ok(Json(PlayInfo::Transcode {
        mime: plan.mime.clone(),
        duration: probe.duration,
        length: entry.length,
        segments: ffmpeg::segment_count(probe.duration),
        segment_seconds: ffmpeg::SEGMENT_SECONDS,
        init: format!("/api/play/{hash}/{file}/init.mp4"),
        segment_prefix: format!("/api/play/{hash}/{file}/seg/"),
        tracks,
        remux_only: plan.copy_video && (plan.copy_audio || !plan.has_audio),
    }))
}

/// Subtitle files sitting alongside the video in the same torrent.
fn sidecar_tracks(torrent: &Torrent, file: usize, hash: &str) -> Vec<Track> {
    let Some(video) = torrent.meta.files.get(file) else {
        return Vec::new();
    };
    torrent
        .meta
        .files
        .iter()
        .enumerate()
        .filter(|(_, candidate)| subtitles::belongs_to(&video.path, &candidate.path))
        .map(|(index, candidate)| {
            let name = candidate.path.rsplit('/').next().unwrap_or(&candidate.path);
            Track {
                url: format!("/api/subtitles/{hash}/{index}"),
                label: name.to_string(),
                language: subtitles::language_of(&candidate.path),
            }
        })
        .collect()
}

/// Subtitle streams inside the video file itself.
fn embedded_tracks(probe: &Probe, hash: &str, file: usize) -> Vec<Track> {
    probe
        .subtitles
        .iter()
        .map(|track| Track {
            url: format!("/api/play/{hash}/{file}/subs/{}", track.index),
            label: track
                .title
                .clone()
                .or_else(|| track.language.clone())
                .unwrap_or_else(|| format!("Track {}", track.index + 1)),
            language: track.language.clone(),
        })
        .collect()
}

/// The MSE initialisation segment: `ftyp` and `moov`.
pub async fn init(
    State(state): State<Arc<AppState>>,
    Path((hash, file)): Path<(String, usize)>,
) -> Result<Response, ApiError> {
    let data = generate(&state, &hash, file, 0).await?;
    let init = fmp4::init_segment(&data)
        .ok_or_else(|| ApiError::server("ffmpeg produced no initialisation segment"))?;
    Ok(mp4_response(init.to_vec()))
}

/// One media segment: `moof` and `mdat` onwards.
pub async fn segment(
    State(state): State<Arc<AppState>>,
    Path((hash, file, index)): Path<(String, usize, u32)>,
) -> Result<Response, ApiError> {
    let data = generate(&state, &hash, file, index).await?;
    let media = fmp4::media_segment(&data)
        .ok_or_else(|| ApiError::server("ffmpeg produced no fragment for that segment"))?;
    Ok(mp4_response(media.to_vec()))
}

/// Produce (or recall) the raw ffmpeg output for a segment.
async fn generate(
    state: &Arc<AppState>,
    hash: &str,
    file: usize,
    index: u32,
) -> Result<Arc<Vec<u8>>, ApiError> {
    let info_hash = parse(hash)?;
    let torrent = torrent(state, &info_hash)?;
    if torrent.meta.files.get(file).is_none() {
        return Err(ApiError::not_found("no such file in this torrent"));
    }
    torrent.touch();

    if let Some(cached) = state.cached_segment(&info_hash, file, index) {
        return Ok(cached);
    }

    let tools = state
        .tools
        .clone()
        .ok_or_else(|| ApiError::server("ffmpeg is not available"))?;
    let probe = state
        .probe(&tools, &info_hash, file)
        .await
        .map_err(|err| ApiError::server(format!("could not read this file: {err}")))?;

    // Refuse a segment past the end before spending an encoder on it. ffmpeg
    // asked to seek beyond the file emits a header and an empty fragment
    // rather than failing, which would reach the browser as a valid-looking
    // segment containing no frames and stall the SourceBuffer with no clue as
    // to why.
    let total = ffmpeg::segment_count(probe.duration);
    if index >= total {
        return Err(ApiError::not_found(format!(
            "segment {index} is past the end of this file, which has {total}"
        )));
    }

    let plan: Plan = ffmpeg::plan(&probe);

    // Bounded so a viewer scrubbing wildly cannot spawn an unbounded number of
    // encoders and starve the download that feeds them.
    let _permit = state
        .transcodes
        .acquire()
        .await
        .map_err(|_| ApiError::server("the transcoder is shutting down"))?;

    // Check again: another request may have produced it while we queued.
    if let Some(cached) = state.cached_segment(&info_hash, file, index) {
        return Ok(cached);
    }

    let url = state.stream_url(hash, file);
    let entry = torrent
        .meta
        .files
        .get(file)
        .ok_or_else(|| ApiError::not_found("no such file in this torrent"))?;

    // Work out whether a disappointing result was a real failure or simply
    // data that has not arrived. Judged after the attempt rather than before,
    // so a conservative estimate can never refuse a segment ffmpeg would have
    // managed perfectly well.
    let blame = |fallback: String| {
        let held = held_fraction(&torrent, entry, probe.duration, index);
        if held < 1.0 {
            ApiError::not_ready(format!(
                "that part of the file has not downloaded yet ({}% of it is here)",
                (held * 100.0).round() as u32
            ))
        } else {
            ApiError::server(fallback)
        }
    };

    match tools.segment(&url, index, &plan).await {
        // ffmpeg can exit happily having produced only a header, which would
        // reach the browser as a valid-looking segment containing no frames.
        Ok(data) if fmp4::first_moof(&data).is_none() => Err(blame(format!(
            "ffmpeg produced no fragment for segment {index}"
        ))),
        Ok(data) => {
            let data = Arc::new(data);
            state.cache_segment(&info_hash, file, index, Arc::clone(&data));
            Ok(data)
        }
        Err(err) => Err(blame(err.to_string())),
    }
}

/// How much of the source a segment needs is actually on disk, from 0 to 1.
///
/// The byte span is estimated by assuming a roughly even bitrate, which is
/// wrong for variable bitrate video but easily good enough to tell "this has
/// not arrived" from "this is broken". A margin either side covers the skew.
/// Only ever used to explain a failure, never to prevent an attempt.
fn held_fraction(
    torrent: &Torrent,
    entry: &balerin_bt::TorrentFile,
    duration: f64,
    index: u32,
) -> f64 {
    let Some((from, span)) = estimated_span(entry.offset, entry.length, duration, index) else {
        return 1.0;
    };
    let pieces = torrent.meta.pieces_for_span(from, span);
    if pieces.is_empty() {
        return 1.0;
    }
    let have = torrent.handle.have();
    let total = pieces.len();
    let held = pieces.filter(|piece| have.has(*piece)).count();
    held as f64 / total as f64
}

/// Roughly which bytes of the torrent a segment needs, as (offset, length).
///
/// Assumes an even bitrate, which is wrong for variable bitrate video, so a
/// generous margin is added either side: a keyframe can sit well before the
/// moment we want, and a container index may live at the far end of the file.
/// `None` when there is nothing sensible to estimate from.
fn estimated_span(offset: u64, length: u64, duration: f64, index: u32) -> Option<(u64, u64)> {
    if duration <= 0.0 || length == 0 || !duration.is_finite() {
        return None;
    }
    let per_second = length as f64 / duration;
    let start = f64::from(index) * ffmpeg::SEGMENT_SECONDS * per_second;
    let len = ffmpeg::SEGMENT_SECONDS * per_second;
    let margin = (length as f64 * 0.02).max(4.0 * 1024.0 * 1024.0);

    let from = offset + (start - margin).max(0.0) as u64;
    let span = (len + margin * 2.0) as u64;
    Some((from, span))
}

fn mp4_response(body: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "video/mp4"),
            // Regenerating a segment is cheap and a stale one is poison, since
            // the transcode settings could have changed under it.
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

/// A sidecar subtitle file from the torrent, as WebVTT.
pub async fn sidecar(
    State(state): State<Arc<AppState>>,
    Path((hash, file)): Path<(String, usize)>,
) -> Result<Response, ApiError> {
    let info_hash = parse(&hash)?;
    let torrent = torrent(&state, &info_hash)?;
    let entry = torrent
        .meta
        .files
        .get(file)
        .ok_or_else(|| ApiError::not_found("no such file in this torrent"))?;
    torrent.touch();

    // Subtitle files are tiny, so waiting for the whole thing is reasonable.
    // Refuse anything implausible rather than buffering a mislabelled film.
    const LIMIT: u64 = 8 * 1024 * 1024;
    if entry.length > LIMIT {
        return Err(ApiError::bad_request(
            "that subtitle file is implausibly large",
        ));
    }

    let span = torrent.meta.pieces_for_span(entry.offset, entry.length);
    for piece in span {
        if !torrent.handle.wait_for_piece(piece).await {
            return Err(ApiError::server(
                "the download stopped before the subtitles arrived",
            ));
        }
    }

    let bytes = torrent
        .handle
        .read_range(entry.offset, entry.length)
        .await
        .map_err(|err| ApiError::server(err.to_string()))?;

    let text = subtitles::decode(&bytes);
    let vtt = if entry.path.to_ascii_lowercase().ends_with(".vtt") {
        text
    } else {
        subtitles::srt_to_vtt(&text)
    };
    Ok(vtt_response(vtt))
}

/// A subtitle stream embedded in the video, extracted as WebVTT.
pub async fn embedded(
    State(state): State<Arc<AppState>>,
    Path((hash, file, track)): Path<(String, usize, usize)>,
) -> Result<Response, ApiError> {
    let info_hash = parse(&hash)?;
    let torrent = torrent(&state, &info_hash)?;
    torrent.touch();

    let tools = state
        .tools
        .clone()
        .ok_or_else(|| ApiError::server("ffmpeg is not available"))?;
    let url = state.stream_url(&hash, file);
    let vtt = tools
        .subtitles(&url, track)
        .await
        .map_err(|err| ApiError::server(err.to_string()))?;
    let _ = info_hash;
    Ok(vtt_response(vtt))
}

fn vtt_response(body: String) -> Response {
    ([(header::CONTENT_TYPE, "text/vtt; charset=utf-8")], body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: f64 = 60.0;

    #[test]
    fn the_first_segment_starts_at_the_beginning_of_the_file() {
        // The margin must not push the offset before the file starts, which
        // would sample another file's pieces entirely.
        let (from, _) = estimated_span(1_000, 600_000_000, MINUTE, 0).unwrap();
        assert_eq!(from, 1_000);
    }

    #[test]
    fn later_segments_move_proportionally_through_the_file() {
        let length = 600_000_000u64;
        // Ten minutes, so each six second segment is a hundredth of the file.
        let (early, _) = estimated_span(0, length, 600.0, 1).unwrap();
        let (late, _) = estimated_span(0, length, 600.0, 50).unwrap();
        assert!(
            late > early,
            "segment 50 should sit further in than segment 1"
        );
        // Segment 50 is halfway through ten minutes.
        let halfway = length / 2;
        let margin = (length as f64 * 0.02) as u64;
        assert!(
            late.abs_diff(halfway) <= margin + 1,
            "expected near {halfway}, got {late}"
        );
    }

    #[test]
    fn the_span_is_offset_by_where_the_file_sits_in_the_torrent() {
        // A file partway through a multi file torrent must not be checked
        // against the pieces of whatever precedes it.
        let (from, _) = estimated_span(500_000_000, 100_000_000, MINUTE, 2).unwrap();
        assert!(from >= 500_000_000, "the file offset was dropped");
    }

    #[test]
    fn nonsense_inputs_yield_no_estimate_rather_than_a_panic() {
        assert!(estimated_span(0, 1_000, 0.0, 0).is_none());
        assert!(estimated_span(0, 1_000, -5.0, 0).is_none());
        assert!(estimated_span(0, 0, MINUTE, 0).is_none());
        assert!(estimated_span(0, 1_000, f64::NAN, 0).is_none());
        assert!(estimated_span(0, 1_000, f64::INFINITY, 0).is_none());
    }

    #[test]
    fn a_small_file_still_gets_a_usable_margin() {
        // Two percent of a tiny file is nothing, so the floor has to apply or
        // the check would demand an implausibly precise span.
        let (_, span) = estimated_span(0, 1_000, MINUTE, 0).unwrap();
        assert!(span >= 8 * 1024 * 1024, "margin floor not applied: {span}");
    }
}
