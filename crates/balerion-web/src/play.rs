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
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use balerion_bt::InfoHash;
use serde::{Deserialize, Serialize};

use crate::ffmpeg::{self, Plan, Probe};
use crate::fmp4;
use crate::media::{self, Playback};
use crate::routes::ApiError;
use crate::state::{AppState, Torrent};
use crate::{subsync, subtitles};

/// How the page should play a file.
#[derive(Debug, Serialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum PlayInfo {
    /// Browsers open this as it is. Use `/stream`.
    Direct {
        url: String,
        /// Seconds to start at, when this was watched before and not finished.
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_at: Option<f64>,
        /// Size of this file, so the page can work out its bitrate once the
        /// browser reports a duration, and say whether the swarm can keep up.
        length: u64,
        tracks: Vec<Track>,
        /// True while subtitles are being transcribed from the audio, so the
        /// page can say "not yet" rather than "there are none".
        subtitles_pending: bool,
    },
    /// Not yet. The file is still arriving and nothing can be said about it.
    ///
    /// Distinct from `Unsupported` on purpose, and the distinction matters more
    /// than it looks: ffprobe has to read the head of the file, and on a fresh
    /// torrent those bytes may not have landed. Reporting that as "this file
    /// cannot be played, download it and use VLC" sends somebody away from a
    /// perfectly good file thirty seconds before it would have worked.
    NotReady {
        reason: String,
        /// So the page can show progress rather than an unchanging line.
        pieces_have: usize,
        pieces_total: usize,
    },
    /// Needs converting. Drive MediaSource against the segment endpoints.
    Transcode {
        mime: String,
        /// Seconds to start at, when this was watched before and not finished.
        #[serde(skip_serializing_if = "Option::is_none")]
        resume_at: Option<f64>,
        duration: f64,
        /// Size of this file. Divided by duration this gives the bitrate
        /// playback must be fed at, which is the only figure that decides
        /// whether a torrent can be watched live or merely downloaded.
        length: u64,
        segments: u32,
        segment_seconds: f64,
        init: String,
        segment_prefix: String,
        /// Appended to every segment URL, carrying the audio choice when it is
        /// not the default. Separate from the prefix because the index goes
        /// between them.
        segment_suffix: String,
        /// Which audio track this plan describes.
        audio: usize,
        /// An HLS playlist covering the same segments.
        ///
        /// Safari plays this natively, which is also what makes AirPlay work:
        /// the television is handed this URL and fetches it itself, which it
        /// cannot do with a MediaSource blob.
        playlist: String,
        tracks: Vec<Track>,
        /// Every audio track in the file, when there is more than one.
        ///
        /// Empty for the ordinary case of a single track, so a page that does
        /// nothing with this shows no menu rather than a menu of one.
        audio_tracks: Vec<AudioTrack>,
        /// True while subtitles are being transcribed from the audio.
        subtitles_pending: bool,
        /// True when only the wrapper is being changed, which is worth saying
        /// because it is the cheap case and the quality is untouched.
        remux_only: bool,
    },
    /// Nothing we can do for this one.
    Unsupported { reason: String, download: String },
}

/// An audio track the page can offer.
#[derive(Debug, Serialize)]
pub struct AudioTrack {
    /// What to put in `?audio=` to select it.
    pub index: usize,
    pub label: String,
    pub language: Option<String>,
    pub channels: Option<u32>,
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
    Query(choice): Query<AudioChoice>,
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
    let mut tracks = sidecar_tracks(&torrent, file, &hash);

    // The cheap path, and the common one.
    if classified.playback == Playback::Native {
        let subtitles_pending =
            add_fetched_track(&state, &info_hash, file, &hash, &mut tracks).await;
        return Ok(Json(PlayInfo::Direct {
            url: stream_url,
            resume_at: state.history.get(&hash, file).and_then(|at| at.resume_at()),
            length: entry.length,
            tracks,
            subtitles_pending,
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
                     Install ffmpeg and restart balerion, or download the file and play it in VLC."
                .into(),
            download,
        }));
    };

    let probe = match state.probe(&tools, &info_hash, file).await {
        Ok(probe) => probe,
        Err(err) => {
            /* A probe that fails on a torrent still filling up has almost
             * certainly failed for want of bytes rather than because the file is
             * unplayable. Saying which costs one look at the piece map, and gets
             * it right in the case that matters: a fresh torrent, where every
             * probe fails until the head arrives. */
            let stats = torrent.handle.stats();
            if !stats.is_complete() {
                return Ok(Json(PlayInfo::NotReady {
                    reason: format!(
                        "still waiting for the start of this file, which has to come off \
                         the swarm before anything can be said about it ({err})"
                    ),
                    pieces_have: stats.pieces_have,
                    pieces_total: stats.pieces_total,
                }));
            }
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

    // The plan depends on which audio track was chosen, because the declared
    // codec string changes with it: one track may be AAC and passed through
    // where another is AC-3 and re-encoded. Getting that wrong is a silent
    // stall rather than an error.
    let track = choice
        .track()
        .min(probe.audio_tracks.len().saturating_sub(1));
    let plan = ffmpeg::plan(&probe.with_audio(track));
    let suffix = if track == 0 {
        String::new()
    } else {
        format!("?audio={track}")
    };
    tracks.extend(embedded_tracks(&probe, &hash, file));
    let subtitles_pending = add_fetched_track(&state, &info_hash, file, &hash, &mut tracks).await;

    Ok(Json(PlayInfo::Transcode {
        mime: plan.mime.clone(),
        resume_at: state.history.get(&hash, file).and_then(|at| at.resume_at()),
        duration: probe.duration,
        length: entry.length,
        segments: ffmpeg::segment_count(probe.duration),
        segment_seconds: ffmpeg::SEGMENT_SECONDS,
        init: format!("/api/play/{hash}/{file}/init.mp4{suffix}"),
        segment_prefix: format!("/api/play/{hash}/{file}/seg/"),
        segment_suffix: suffix.clone(),
        playlist: format!("/api/play/{hash}/{file}/index.m3u8{suffix}"),
        audio: track,
        tracks,
        audio_tracks: audio_tracks(&probe),
        subtitles_pending,
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
                // The video index comes along because aligning the
                // subtitles to the speech means knowing which speech.
                url: format!("/api/subtitles/{hash}/{index}?video={file}"),
                label: name.to_string(),
                language: subtitles::language_of(&candidate.path),
            }
        })
        .collect()
}

/// The audio tracks worth offering a choice between.
///
/// Empty when there is only one, because a menu with a single entry is a menu
/// that should not be there.
fn audio_tracks(probe: &Probe) -> Vec<AudioTrack> {
    if probe.audio_tracks.len() < 2 {
        return Vec::new();
    }
    probe
        .audio_tracks
        .iter()
        .map(|track| AudioTrack {
            index: track.index,
            label: track.label(),
            language: track.language.clone(),
            channels: track.channels,
        })
        .collect()
}

/// Offer a track from OpenSubtitles, when the release came with none.
///
/// Only when there is nothing already, and deliberately so. A release that
/// carries its own subtitles has ones that match it, and spending a request on
/// a second opinion would be work for its own sake against an allowance of five
/// a day.
///
/// The search happens here and the download does not: the URL points at an
/// endpoint the browser only fetches when a viewer turns the track on.
async fn add_fetched_track(
    state: &Arc<AppState>,
    info_hash: &balerion_bt::InfoHash,
    file: usize,
    hash: &str,
    tracks: &mut Vec<Track>,
) -> bool {
    if !tracks.is_empty() {
        return false;
    }

    // Something is already on disk: fetched last time, or transcribed. Offered
    // without asking anybody anything.
    if crate::fetched::is_cached(state, info_hash, file).await {
        tracks.push(Track {
            url: format!("/api/subtitles/{hash}/{file}/fetched"),
            label: "English (found for you)".to_string(),
            language: Some("en".into()),
        });
        return false;
    }

    let offer = match crate::fetched::look(state, info_hash, file).await {
        Some(offer) => offer,
        // Nobody has any. Make some, if this machine can, and say that a job
        // is running so the page can tell the difference between "there are no
        // subtitles" and "there are none yet".
        None => return crate::fetched::transcribe(state, info_hash, file),
    };

    // The label says where these came from and how well they fit, because
    // "English" alone would make a title match look like a promise.
    let label = match (offer.exact, offer.best.machine_translated) {
        (true, _) => "English (matched to this file)".to_string(),
        (false, true) => "English (fetched, machine translated)".to_string(),
        (false, false) => "English (fetched)".to_string(),
    };
    tracks.push(Track {
        url: format!("/api/subtitles/{hash}/{file}/fetched"),
        label,
        language: offer.best.language.clone().or_else(|| Some("en".into())),
    });
    false
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

/// Which audio track to use, when a file has more than one.
#[derive(Debug, Default, Deserialize)]
pub struct AudioChoice {
    /// Index among the audio streams. Absent means the first, which is what
    /// the muxer intended and what every player does.
    pub audio: Option<usize>,
}

impl AudioChoice {
    fn track(&self) -> usize {
        self.audio.unwrap_or(0)
    }
}

/// The MSE initialisation segment: `ftyp` and `moov`.
pub async fn init(
    State(state): State<Arc<AppState>>,
    Path((hash, file)): Path<(String, usize)>,
    Query(choice): Query<AudioChoice>,
) -> Result<Response, ApiError> {
    let data = generate(&state, &hash, file, 0, choice.track()).await?;
    let init = fmp4::init_segment(&data)
        .ok_or_else(|| ApiError::server("ffmpeg produced no initialisation segment"))?;
    Ok(mp4_response(init.to_vec()))
}

/// One media segment: `moof` and `mdat` onwards.
pub async fn segment(
    State(state): State<Arc<AppState>>,
    Path((hash, file, index)): Path<(String, usize, u32)>,
    Query(choice): Query<AudioChoice>,
) -> Result<Response, ApiError> {
    let data = generate(&state, &hash, file, index, choice.track()).await?;
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
    audio: usize,
) -> Result<Arc<Vec<u8>>, ApiError> {
    let info_hash = parse(hash)?;
    let torrent = torrent(state, &info_hash)?;
    if torrent.meta.files.get(file).is_none() {
        return Err(ApiError::not_found("no such file in this torrent"));
    }
    torrent.touch();

    // Keyed by the audio track as well, or switching tracks would be answered
    // with segments carrying the old one.
    if let Some(cached) = state.cached_segment(&info_hash, file, index, audio) {
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

    let plan: Plan = ffmpeg::plan(&probe.with_audio(audio));

    // Bounded so a viewer scrubbing wildly cannot spawn an unbounded number of
    // encoders and starve the download that feeds them.
    let _permit = state
        .transcodes
        .acquire()
        .await
        .map_err(|_| ApiError::server("the transcoder is shutting down"))?;

    // Check again: another request may have produced it while we queued.
    if let Some(cached) = state.cached_segment(&info_hash, file, index, audio) {
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

    let began = std::time::Instant::now();
    let produced = tools.segment(&url, index, &plan).await;
    // Timed whether it worked or not: a failure that took ninety seconds is
    // itself the answer to "why did playback stop".
    state.encoder.record(began.elapsed());

    match produced {
        // ffmpeg can exit happily having produced only a header, which would
        // reach the browser as a valid-looking segment containing no frames.
        Ok(data) if fmp4::first_moof(&data).is_none() => Err(blame(format!(
            "ffmpeg produced no fragment for segment {index}"
        ))),
        Ok(data) => {
            let data = Arc::new(data);
            state.cache_segment(&info_hash, file, index, audio, Arc::clone(&data));
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
    entry: &balerion_bt::TorrentFile,
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
#[derive(Debug, Deserialize)]
pub struct SidecarQuery {
    /// Which file in the torrent these subtitles belong to.
    ///
    /// Optional so an old bookmark still works, and when it is missing the
    /// track is served exactly as it was written, with no attempt to align it.
    pub video: Option<usize>,
}

pub async fn sidecar(
    State(state): State<Arc<AppState>>,
    Path((hash, file)): Path<(String, usize)>,
    Query(query): Query<SidecarQuery>,
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
    let cues = subtitles::parse_cues(&text);

    /* A subtitle file that came in the same torrent is not thereby in step with
     * the video. Releases get rebuilt, leaders differ, and PAL transfers run
     * 4.3% fast, so a track that is perfect at the opening titles can be four
     * minutes out by the end. The old behaviour was to trust it completely. */
    let Some(video) = query.video else {
        return Ok(vtt_response(subtitles::cues_to_vtt(&cues)));
    };
    match align_sidecar(&state, &info_hash, video, file, &cues).await {
        Some(alignment) if alignment.is_worth_applying() => {
            let moved = subsync::apply(&cues, alignment);
            tracing::info!(
                subtitle = entry.path,
                offset_ms = alignment.offset_ms,
                scale = alignment.scale,
                confidence = alignment.confidence,
                "subtitles moved into step with the speech"
            );
            Ok(vtt_response(subtitles::cues_to_vtt_noted(
                &moved,
                Some(&note(&alignment)),
            )))
        }
        // Either it was already right, or we could not tell. Both mean the
        // file is served as its author wrote it.
        _ => Ok(vtt_response(subtitles::cues_to_vtt(&cues))),
    }
}

/// Subtitles nobody put in the torrent.
///
/// Separate from [`sidecar`] because this is the call that spends the daily
/// OpenSubtitles allowance, and because it is the one that can honestly fail:
/// there may be nothing out there for what you are watching.
pub async fn fetched(
    State(state): State<Arc<AppState>>,
    Path((hash, file)): Path<(String, usize)>,
) -> Result<Response, ApiError> {
    let info_hash = parse(&hash)?;
    let torrent = torrent(&state, &info_hash)?;
    torrent.touch();

    match crate::fetched::fetch(&state, &info_hash, file).await {
        Ok(vtt) => Ok(vtt_response(vtt)),
        Err(err) => Err(ApiError::not_found(format!(
            "could not get subtitles for this: {err}"
        ))),
    }
}

/// What was done to the timings, for anyone who goes looking.
fn note(alignment: &subsync::Alignment) -> String {
    format!(
        "balerion moved these by {}ms at {:.4}x (confidence {:.2})",
        alignment.offset_ms, alignment.scale, alignment.confidence
    )
}

/// Work out where a sidecar track belongs, remembering the answer.
///
/// Returns `None` when there is nothing to compare against, when ffmpeg is
/// absent, or when the comparison was not convincing. All three mean the same
/// thing to the caller and are worth distinguishing only in the log.
async fn align_sidecar(
    state: &Arc<AppState>,
    info_hash: &balerion_bt::InfoHash,
    video: usize,
    subtitle: usize,
    cues: &[subtitles::Cue],
) -> Option<subsync::Alignment> {
    if cues.is_empty() {
        return None;
    }
    if let Some(remembered) = state.cached_alignment(info_hash, video, subtitle) {
        return remembered;
    }

    let tools = state.tools.clone()?;
    let probe = state.probe(&tools, info_hash, video).await.ok()?;
    let url = state.stream_url(&info_hash.to_hex(), video);

    let found = match subsync::align_to_audio(&tools, &url, cues, probe.duration).await {
        Ok(alignment) if alignment.is_trustworthy() => Some(alignment),
        Ok(alignment) => {
            // Said out loud rather than silently ignored: "we looked and we are
            // not sure" is a different thing from "we did not look", and only
            // one of them is worth investigating.
            tracing::info!(
                confidence = alignment.confidence,
                offset_ms = alignment.offset_ms,
                "not confident enough about these subtitles to move them"
            );
            None
        }
        Err(err) => {
            tracing::debug!(%err, "could not align these subtitles");
            None
        }
    };

    state.remember_alignment(info_hash, video, subtitle, found);
    found
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

/// An HLS playlist for a transcoded file.
///
/// This is what makes casting possible at all. A television is a separate box
/// on the network: it is handed a URL and fetches the media itself, so anything
/// it plays has to exist as a resource. The MediaSource path hands the browser
/// a blob with no URL behind it, which no Apple TV or Chromecast can reach.
///
/// The segments themselves already exist, so this is a text file pointing at
/// them. `EXT-X-VERSION:7` and `EXT-X-MAP` are what allow fragmented MP4 rather
/// than the MPEG-TS of older HLS, which means the same segments serve the
/// browser and the television.
///
/// Durations are declared as measured rather than as requested. ffmpeg returns
/// a little over the six seconds asked for, and a playlist that claims a round
/// number while the media says otherwise makes a player's seeking drift further
/// the longer the film runs.
pub async fn playlist(
    State(state): State<Arc<AppState>>,
    Path((hash, file)): Path<(String, usize)>,
) -> Result<Response, ApiError> {
    let info_hash = parse(&hash)?;
    let torrent = torrent(&state, &info_hash)?;
    torrent.touch();

    let Some(tools) = state.tools.clone() else {
        return Err(ApiError::bad_request(
            "this file needs converting and ffmpeg was not found",
        ));
    };
    let probe = state
        .probe(&tools, &info_hash, file)
        .await
        .map_err(|err| ApiError::bad_request(format!("could not read this file: {err}")))?;

    if probe.duration <= 0.0 {
        return Err(ApiError::bad_request(
            "this file has no duration, so it cannot be served as a playlist",
        ));
    }

    let segment_seconds = ffmpeg::SEGMENT_SECONDS;
    let count = (probe.duration / segment_seconds).ceil() as u32;

    let mut playlist = String::with_capacity(64 + count as usize * 32);
    playlist.push_str("#EXTM3U\n");
    // 7 is the first version that understands EXT-X-MAP, and so fragmented MP4.
    playlist.push_str("#EXT-X-VERSION:7\n");
    // Rounded up, and never below the longest segment, or players reject it.
    playlist.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        segment_seconds.ceil() as u32
    ));
    playlist.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    // The whole file exists and is not going to grow, which lets a player seek
    // anywhere immediately instead of treating it as a live stream.
    playlist.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    playlist.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    playlist.push_str("#EXT-X-MAP:URI=\"init.mp4\"\n");

    for index in 0..count {
        // The last one is short, and saying so keeps the declared total equal to
        // the real one.
        let start = f64::from(index) * segment_seconds;
        let length = (probe.duration - start).min(segment_seconds);
        playlist.push_str(&format!("#EXTINF:{length:.3},\nseg/{index}\n"));
    }
    playlist.push_str("#EXT-X-ENDLIST\n");

    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/vnd.apple.mpegurl",
            ),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        playlist,
    )
        .into_response())
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
