//! Turning something you kept into something that plays anywhere.
//!
//! The transcoder in [`crate::ffmpeg`] is a good answer to "play this now": it
//! encodes six seconds at a time, on demand, and the first of them is ready in
//! about a second. It is a poor answer to everything else. Every seek is an
//! encode, so scrubbing costs four seconds a throw. Nothing can be handed to a
//! television, because there is no file to hand it. And it has to be done again
//! from scratch every time anybody watches.
//!
//! The trick that makes Netflix feel like Netflix is not a clever one: the file
//! was finished before you pressed play. So a download that was asked to be
//! kept gets converted once, up front, into a single MP4 that browsers, phones,
//! Apple TVs and Chromecasts all open natively. On a machine with a hardware
//! encoder that is roughly twelve times faster than watching it, so an episode
//! is a few minutes of work while you make tea.
//!
//! After that it stops being a media problem and becomes a file problem: served
//! with byte ranges, seeking is exact and instant, and the address of it is a
//! thing a receiver can simply fetch.
//!
//! Kept in a directory beside the torrent's own data rather than mixed in with
//! it, because the torrent's files are the torrent's: their names and sizes are
//! what the swarm agreed on, and writing a file of our own into the middle of
//! that would make the next hash check disagree with us.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::ffmpeg::{Plan, Tools};

/// Where converted files live, under the torrent's own directory.
const READY_DIR: &str = ".balerion-ready";

/// Where this file's converted copy would be.
pub fn ready_path(root: &Path, file: usize) -> PathBuf {
    root.join(READY_DIR).join(format!("{file}.mp4"))
}

/// The converted copy, if there is a finished one.
///
/// Finished is the word doing the work. Conversion writes to a neighbouring
/// temporary name and renames when ffmpeg exits happily, so a file that exists
/// here is one that was completed rather than one that was interrupted. A
/// half-written MP4 has no index at all and would play as nothing.
pub fn ready(root: &Path, file: usize) -> Option<PathBuf> {
    let path = ready_path(root, file);
    let complete = std::fs::metadata(&path).is_ok_and(|meta| meta.is_file() && meta.len() > 0);
    complete.then_some(path)
}

/// How far along a conversion is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Progress {
    /// Seconds of the film converted so far.
    pub done: f64,
    /// Seconds in the film altogether.
    pub total: f64,
}

impl Progress {
    pub fn fraction(&self) -> f64 {
        if self.total <= 0.0 {
            return 0.0;
        }
        (self.done / self.total).clamp(0.0, 1.0)
    }
}

/// Parse one `key=value` line of ffmpeg's `-progress` output.
///
/// It writes a block of them every so often, ending with `progress=continue`
/// or `progress=end`. The only one worth reading is how far into the film it
/// has got, which it gives in microseconds.
pub fn out_time_seconds(line: &str) -> Option<f64> {
    let (key, value) = line.split_once('=')?;
    match key.trim() {
        "out_time_us" | "out_time_ms" => {
            // `out_time_ms` is a misnomer in ffmpeg and carries microseconds
            // too, which is the sort of thing that costs an afternoon.
            let micros: f64 = value.trim().parse().ok()?;
            (micros >= 0.0).then_some(micros / 1_000_000.0)
        }
        _ => None,
    }
}

/// The arguments for converting `source` into `out`.
///
/// Split out so the shape of the command can be asserted without running
/// anything: the flags here are the difference between a file that plays
/// everywhere and one that plays in exactly the browser it was tested in.
pub fn arguments(source: &str, out: &Path, plan: &Plan, encoder: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-v".into(),
        "error".into(),
        "-i".into(),
        source.into(),
        // Chapters become a third track with a text handler that nothing asked
        // for, and some receivers refuse a file they do not understand.
        "-map_chapters".into(),
        "-1".into(),
    ];

    if plan.has_video {
        args.extend(["-map".into(), "0:v:0".into()]);
    }
    if plan.has_audio {
        args.extend(["-map".into(), format!("0:a:{}", plan.audio_track)]);
    }

    if plan.has_video {
        if plan.copy_video {
            // Already something everything can decode: no reason to spend an
            // hour of a GPU making it worse.
            args.extend(["-c:v".into(), "copy".into()]);
        } else {
            args.extend([
                "-c:v".into(),
                encoder.into(),
                "-profile:v".into(),
                "high".into(),
                "-pix_fmt".into(),
                "yuv420p".into(),
                "-b:v".into(),
                plan.video_bitrate.to_string(),
            ]);
        }
    }

    if plan.has_audio {
        if plan.copy_audio {
            args.extend(["-c:a".into(), "copy".into()]);
        } else {
            args.extend([
                "-c:a".into(),
                "aac".into(),
                "-b:a".into(),
                "128k".into(),
                // Stereo everywhere. A phone handed 5.1 AAC is a lottery, and
                // an Apple TV handed it plays silence often enough to matter.
                "-ac".into(),
                "2".into(),
            ]);
        }
    }

    args.extend([
        // Named rather than guessed from the extension. The file being written
        // is `0.mp4.part`, so ffmpeg reads the extension as `part`, finds no
        // muxer for it, and refuses the whole job before encoding a frame.
        "-f".into(),
        "mp4".into(),
        // The index at the front. Without this a player has to fetch the end of
        // the file before it can start, which over a tunnel is the difference
        // between playing at once and thinking for five seconds, and some
        // receivers simply refuse.
        "-movflags".into(),
        "+faststart".into(),
        // Said in blocks on stdout, so the shelf can show a number rather than
        // a spinner for four minutes.
        "-progress".into(),
        "pipe:1".into(),
        "-nostats".into(),
        out.to_string_lossy().into_owned(),
    ]);
    args
}

/// The name written to while the work is going on.
fn working_name(out: &Path) -> PathBuf {
    let mut name = out.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

/// Convert one file, reporting progress as it goes.
///
/// `on_progress` is called with seconds converted, often. It is given the
/// number rather than a fraction so the caller decides what to do with a
/// duration it may know better than we do.
pub async fn run<F>(
    tools: &Tools,
    source: &str,
    out: &Path,
    plan: &Plan,
    mut on_progress: F,
) -> Result<PathBuf>
where
    F: FnMut(f64) + Send,
{
    if let Some(parent) = out.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("could not make {}", parent.display()))?;
    }

    let working = working_name(out);
    let args = arguments(source, &working, plan, tools.video_encoder);

    let mut child = Command::new(&tools.ffmpeg)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("could not run ffmpeg")?;

    if let Some(stdout) = child.stdout.take() {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(seconds) = out_time_seconds(&line) {
                on_progress(seconds);
            }
        }
    }

    let finished = child.wait_with_output().await.context("ffmpeg vanished")?;
    if !finished.status.success() {
        let said = String::from_utf8_lossy(&finished.stderr);
        // Tidy up rather than leaving a part file to confuse the next run.
        let _ = tokio::fs::remove_file(&working).await;
        anyhow::bail!("ffmpeg refused this file: {}", said.trim());
    }

    // Only now does it become the thing others may read.
    tokio::fs::rename(&working, out)
        .await
        .with_context(|| format!("could not put {} in place", out.display()))?;
    Ok(out.to_path_buf())
}

/// Start converting every video file in a kept torrent that needs it.
///
/// Spawned rather than awaited: an episode is minutes of work, and the caller
/// is a request handler. Silent when there is nothing to do, which is the
/// common case: a torrent that is already H.264 in an MP4 is left alone,
/// because converting it would spend an hour of a GPU making it worse.
pub fn prepare(state: &std::sync::Arc<crate::state::AppState>, hash: balerion_bt::InfoHash) {
    let state = std::sync::Arc::clone(state);
    tokio::spawn(async move {
        let Some(torrent) = state.get(&hash) else {
            return;
        };
        if !torrent.handle.stats().is_complete() {
            return;
        }
        let Some(tools) = state.tools.clone() else {
            return;
        };

        for (index, entry) in torrent.meta.files.iter().enumerate() {
            let classified = crate::media::classify(&entry.path);
            if classified.kind != crate::media::Kind::Video {
                continue;
            }
            // Already something a browser opens: nothing to prepare.
            if classified.playback == crate::media::Playback::Native {
                continue;
            }
            if ready(&torrent.root, index).is_some() {
                continue;
            }

            let Ok(probe) = state.probe(&tools, &hash, index).await else {
                continue;
            };
            if !state.begin_converting(&hash, index, probe.duration) {
                continue;
            }

            // Queued behind any other conversion. Segments for somebody
            // actually watching are not queued behind anything.
            let Ok(_turn) = state.conversions.acquire().await else {
                state.finished_converting(&hash, index);
                return;
            };

            let source = state.stream_url(&hash.to_hex(), index);
            let out = ready_path(&torrent.root, index);
            let plan = crate::ffmpeg::plan(&probe);
            tracing::info!(name = entry.path, "preparing a kept file to play anywhere");

            let outcome = run(&tools, &source, &out, &plan, |seconds| {
                state.converting_reached(&hash, index, seconds);
            })
            .await;
            state.finished_converting(&hash, index);

            match outcome {
                Ok(path) => tracing::info!(path = %path.display(), "ready to play anywhere"),
                Err(err) => tracing::warn!(%err, name = entry.path, "could not prepare this one"),
            }
        }
    });
}

/// Read `len` bytes out of an open file, a chunk at a time.
///
/// In the same shape as the torrent stream's body rather than a new
/// dependency, and chunked rather than read whole because a film is larger
/// than anybody wants in memory at once.
fn chunks(
    handle: tokio::fs::File,
    len: u64,
) -> impl futures_util::Stream<Item = std::io::Result<Vec<u8>>> {
    const CHUNK: usize = 256 * 1024;
    futures_util::stream::unfold((handle, len), |(mut handle, left)| async move {
        if left == 0 {
            return None;
        }
        let want = CHUNK.min(usize::try_from(left).unwrap_or(CHUNK));
        let mut buffer = vec![0u8; want];
        match tokio::io::AsyncReadExt::read(&mut handle, &mut buffer).await {
            // A file that ends early is a file somebody deleted underneath us.
            Ok(0) => None,
            Ok(read) => {
                buffer.truncate(read);
                Some((Ok(buffer), (handle, left - read as u64)))
            }
            Err(err) => Some((Err(err), (handle, 0))),
        }
    })
}

/// Serve a converted file, with byte ranges.
///
/// Deliberately not the torrent stream endpoint. That one exists to serve
/// bytes that may not have arrived yet, and everything about it — waiting for
/// pieces, prioritising the picker, refusing to be cached — is in service of
/// that. This file is finished and on disk, so it can be what a player most
/// wants a video to be: an ordinary file, seekable to the byte, cacheable, and
/// with an address a television can fetch.
pub async fn serve(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<crate::state::AppState>>,
    axum::extract::Path((hash, file)): axum::extract::Path<(String, usize)>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    let Ok(info_hash) = balerion_bt::InfoHash::parse(&hash) else {
        return (StatusCode::BAD_REQUEST, "that is not an infohash").into_response();
    };
    let Some(torrent) = state.get(&info_hash) else {
        return (StatusCode::NOT_FOUND, "no such torrent").into_response();
    };
    let Some(path) = ready(&torrent.root, file) else {
        return (
            StatusCode::NOT_FOUND,
            "this one has not been prepared to play anywhere",
        )
            .into_response();
    };
    torrent.touch();

    let Ok(handle) = tokio::fs::File::open(&path).await else {
        return (StatusCode::NOT_FOUND, "the prepared file has gone").into_response();
    };
    let Ok(meta) = handle.metadata().await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not measure it").into_response();
    };
    let length = meta.len();

    let (start, end, status) = match crate::range::parse(
        headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok()),
        length,
    ) {
        crate::range::Requested::Unsatisfiable => {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{length}"))],
            )
                .into_response();
        }
        crate::range::Requested::Whole => (0, length, StatusCode::OK),
        crate::range::Requested::Partial { start, end } => (start, end, StatusCode::PARTIAL_CONTENT),
    };

    let mut handle = handle;
    if start > 0
        && tokio::io::AsyncSeekExt::seek(&mut handle, std::io::SeekFrom::Start(start))
            .await
            .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "could not seek it").into_response();
    }

    let served = end - start;
    let body = axum::body::Body::from_stream(chunks(handle, served));

    let mut response = axum::http::HeaderMap::new();
    response.insert(header::ACCEPT_RANGES, "bytes".parse().expect("static"));
    response.insert(header::CONTENT_TYPE, "video/mp4".parse().expect("static"));
    response.insert(
        header::CONTENT_LENGTH,
        served.to_string().parse().expect("a number"),
    );
    // Safe to cache, unlike the torrent stream: this file is finished and will
    // not change under anybody. It is what makes a second viewing instant.
    response.insert(
        header::CACHE_CONTROL,
        "private, max-age=86400".parse().expect("static"),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        response.insert(
            header::CONTENT_RANGE,
            format!("bytes {start}-{}/{length}", end - 1)
                .parse()
                .expect("a range"),
        );
    }
    (status, response, body).into_response()
}


#[cfg(test)]
mod tests {
    use super::*;

    fn a_plan() -> Plan {
        Plan {
            copy_video: false,
            copy_audio: false,
            has_video: true,
            has_audio: true,
            audio_track: 0,
            video_bitrate: 2_000_000,
            mime: "video/mp4".into(),
        }
    }

    #[test]
    fn the_converted_copy_sits_beside_the_data_rather_than_in_it() {
        // Writing our own file among the torrent's would make the next hash
        // check disagree with us about what is on disk.
        let path = ready_path(Path::new("/data/abc"), 3);
        assert_eq!(path, Path::new("/data/abc/.balerion-ready/3.mp4"));
    }

    #[test]
    fn the_working_name_is_not_the_finished_one() {
        let out = Path::new("/data/abc/.balerion-ready/0.mp4");
        assert_eq!(
            working_name(out),
            Path::new("/data/abc/.balerion-ready/0.mp4.part")
        );
    }

    #[test]
    fn progress_is_read_out_of_ffmpeg_s_blocks() {
        assert_eq!(out_time_seconds("out_time_us=1500000"), Some(1.5));
        // Named milliseconds, carries microseconds. Not our doing.
        assert_eq!(out_time_seconds("out_time_ms=2000000"), Some(2.0));
        assert_eq!(out_time_seconds("  out_time_us = 3000000 "), Some(3.0));
        assert_eq!(out_time_seconds("frame=125"), None);
        assert_eq!(out_time_seconds("progress=continue"), None);
        assert_eq!(out_time_seconds("out_time_us=N/A"), None);
        assert_eq!(out_time_seconds("nonsense"), None);
        // ffmpeg emits -1 before it has started on some inputs.
        assert_eq!(out_time_seconds("out_time_us=-1"), None);
    }

    #[test]
    fn a_fraction_is_never_outside_itself() {
        assert_eq!(Progress { done: 0.0, total: 100.0 }.fraction(), 0.0);
        assert_eq!(Progress { done: 50.0, total: 100.0 }.fraction(), 0.5);
        // ffmpeg overshoots the probed duration on some files.
        assert_eq!(Progress { done: 120.0, total: 100.0 }.fraction(), 1.0);
        // And a file whose duration nobody knows must not divide by it.
        assert_eq!(Progress { done: 10.0, total: 0.0 }.fraction(), 0.0);
    }

    #[test]
    fn the_index_goes_at_the_front() {
        // Without faststart a player fetches the end of the file before it can
        // start, and some receivers refuse the file outright.
        let args = arguments("/in.mkv", Path::new("/out.mp4"), &a_plan(), "libx264");
        let at = args.iter().position(|a| a == "-movflags").expect("faststart");
        assert_eq!(args[at + 1], "+faststart");
    }

    #[test]
    fn audio_is_brought_down_to_stereo() {
        let args = arguments("/in.mkv", Path::new("/out.mp4"), &a_plan(), "libx264");
        let at = args.iter().position(|a| a == "-ac").expect("channels");
        assert_eq!(args[at + 1], "2");
    }

    #[test]
    fn something_already_playable_is_not_re_encoded() {
        let plan = Plan {
            copy_video: true,
            copy_audio: true,
            ..a_plan()
        };
        let args = arguments("/in.mp4", Path::new("/out.mp4"), &plan, "libx264");
        assert!(args.windows(2).any(|w| w[0] == "-c:v" && w[1] == "copy"));
        assert!(args.windows(2).any(|w| w[0] == "-c:a" && w[1] == "copy"));
        assert!(
            !args.iter().any(|a| a == "-b:v"),
            "a bitrate means it is being re-encoded"
        );
    }

    #[test]
    fn a_file_with_no_sound_is_not_asked_for_any() {
        let plan = Plan {
            has_audio: false,
            ..a_plan()
        };
        let args = arguments("/in.mkv", Path::new("/out.mp4"), &plan, "libx264");
        assert!(!args.iter().any(|a| a.starts_with("0:a:")));
        assert!(!args.iter().any(|a| a == "-c:a"));
    }

    #[test]
    fn the_chosen_audio_track_is_the_one_taken() {
        let plan = Plan {
            audio_track: 2,
            ..a_plan()
        };
        let args = arguments("/in.mkv", Path::new("/out.mp4"), &plan, "libx264");
        assert!(args.iter().any(|a| a == "0:a:2"));
    }

    #[test]
    fn progress_is_asked_for_so_the_shelf_can_show_a_number() {
        let args = arguments("/in.mkv", Path::new("/out.mp4"), &a_plan(), "libx264");
        let at = args.iter().position(|a| a == "-progress").expect("progress");
        assert_eq!(args[at + 1], "pipe:1");
        assert!(args.iter().any(|a| a == "-nostats"));
    }

    #[test]
    fn the_format_is_named_rather_than_guessed() {
        // The working file is `0.mp4.part`; ffmpeg reads that extension as
        // `part`, finds no muxer, and refuses before encoding a frame.
        let args = arguments("/in.mkv", Path::new("/out.mp4.part"), &a_plan(), "libx264");
        let at = args.iter().position(|a| a == "-f").expect("a named format");
        assert_eq!(args[at + 1], "mp4");
    }

    #[test]
    fn the_encoder_asked_for_is_the_one_used() {
        // The machine may have a hardware one, and using the software encoder
        // instead is the difference between four minutes and forty.
        let args = arguments("/in.mkv", Path::new("/out.mp4"), &a_plan(), "h264_nvenc");
        let at = args.iter().position(|a| a == "-c:v").expect("an encoder");
        assert_eq!(args[at + 1], "h264_nvenc");
    }

    #[test]
    fn the_output_is_the_last_argument() {
        // ffmpeg reads it positionally; anything after it is an option for a
        // file that does not exist.
        let args = arguments("/in.mkv", Path::new("/out.mp4"), &a_plan(), "libx264");
        assert_eq!(args.last().unwrap(), "/out.mp4");
    }
}
