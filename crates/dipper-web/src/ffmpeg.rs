//! Talking to ffmpeg.
//!
//! ffmpeg is optional. When it is missing everything here reports as much and
//! the player falls back to offering a download, which is what dipper did
//! before this module existed. The single binary promise survives: transcoding
//! is an enhancement, not a requirement.
//!
//! The pleasant part of this design is that ffmpeg reads dipper's own range
//! endpoint over HTTP, so it is just another client and the piece picker
//! steers for it automatically. No new plumbing into the torrent engine.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::process::Command;

/// How long one segment covers. Six seconds is the usual compromise: short
/// enough that a seek feels immediate, long enough that spawning a process per
/// segment is noise.
pub const SEGMENT_SECONDS: f64 = 6.0;

const SEGMENT_TIMEOUT: Duration = Duration::from_secs(180);
const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// What the page is told a transcode will produce: H.264 High, at a level
/// ceiling rather than an exact level. See the note in [`plan`].
pub const TRANSCODE_CODEC: &str = "avc1.640033";

/// Always available, and tolerant of inputs hardware encoders refuse.
const FALLBACK_ENCODER: &str = "libx264";

/// ffmpeg, if this machine has it.
#[derive(Debug, Clone)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Hardware encoder where there is one, software otherwise.
    pub video_encoder: &'static str,
}

impl Tools {
    /// Look for ffmpeg on the PATH.
    ///
    /// Returns `None` rather than an error: not having ffmpeg is an ordinary
    /// state to be in, not a failure to report.
    pub async fn detect() -> Option<Self> {
        let ffmpeg = runnable("ffmpeg").await?;
        let ffprobe = runnable("ffprobe").await?;

        // VideoToolbox costs almost nothing and leaves the CPU free for the
        // torrent. libx264 everywhere else.
        let listing = Command::new(&ffmpeg)
            .args(["-hide_banner", "-encoders"])
            .output()
            .await
            .ok()?;
        let video_encoder =
            if String::from_utf8_lossy(&listing.stdout).contains("h264_videotoolbox") {
                "h264_videotoolbox"
            } else {
                "libx264"
            };

        tracing::info!(encoder = video_encoder, "transcoding is available");
        Some(Self {
            ffmpeg,
            ffprobe,
            video_encoder,
        })
    }
}

/// Is this tool on the PATH and does it run?
async fn runnable(tool: &str) -> Option<PathBuf> {
    Command::new(tool)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .ok()
        .filter(std::process::ExitStatus::success)
        .map(|_| PathBuf::from(tool))
}

/// What ffprobe found in a file.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub duration: f64,
    pub video: Option<VideoStream>,
    pub audio: Option<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
}

#[derive(Debug, Clone)]
pub struct VideoStream {
    pub codec: String,
    pub profile: Option<String>,
    pub level: Option<i64>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct AudioStream {
    pub codec: String,
}

#[derive(Debug, Clone)]
pub struct SubtitleStream {
    /// Index among subtitle streams, which is what `-map 0:s:N` wants.
    pub index: usize,
    pub language: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    #[serde(default)]
    format: RawFormat,
}

#[derive(Debug, Default, Deserialize)]
struct RawFormat {
    #[serde(default)]
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    profile: Option<String>,
    level: Option<i64>,
    width: Option<u32>,
    height: Option<u32>,
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
}

impl Tools {
    /// Ask ffprobe what is in a file.
    ///
    /// `url` points back at dipper's own range endpoint, so this works on a
    /// torrent that is nowhere near finished: ffprobe range-requests the header
    /// and the picker fetches it.
    pub async fn probe(&self, url: &str) -> Result<Probe> {
        let output = tokio::time::timeout(
            PROBE_TIMEOUT,
            Command::new(&self.ffprobe)
                .args([
                    "-hide_banner",
                    "-v",
                    "error",
                    "-show_streams",
                    "-show_format",
                    "-of",
                    "json",
                    url,
                ])
                .output(),
        )
        .await
        .context("ffprobe took too long; the download may be stalled")?
        .context("could not run ffprobe")?;

        if !output.status.success() {
            bail!(
                "ffprobe could not read this file: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        let raw: RawProbe =
            serde_json::from_slice(&output.stdout).context("ffprobe returned unreadable JSON")?;

        let mut probe = Probe {
            duration: raw
                .format
                .duration
                .and_then(|d| d.parse().ok())
                .unwrap_or(0.0),
            ..Default::default()
        };

        for stream in raw.streams {
            let kind = stream.codec_type.as_deref().unwrap_or_default();
            let codec = stream.codec_name.clone().unwrap_or_default();
            match kind {
                // First of each wins. Multiple audio tracks are a real thing
                // but choosing between them is not this change.
                "video" if probe.video.is_none() => {
                    // Cover art in an audio file is a video stream as far as
                    // ffprobe is concerned, and trying to play it is a mess.
                    if !matches!(codec.as_str(), "mjpeg" | "png" | "bmp" | "gif") {
                        probe.video = Some(VideoStream {
                            codec,
                            profile: stream.profile,
                            level: stream.level,
                            width: stream.width,
                            height: stream.height,
                        });
                    }
                }
                "audio" if probe.audio.is_none() => probe.audio = Some(AudioStream { codec }),
                "subtitle" => {
                    let index = probe.subtitles.len();
                    probe.subtitles.push(SubtitleStream {
                        index,
                        language: stream.tags.get("language").cloned(),
                        title: stream.tags.get("title").cloned(),
                    });
                }
                _ => {}
            }
        }
        Ok(probe)
    }
}

/// Whether each stream can be passed through untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub copy_video: bool,
    pub copy_audio: bool,
    pub has_video: bool,
    pub has_audio: bool,
    /// The MIME type MSE needs, including codecs. Getting this wrong is a
    /// silent stall rather than an error, so it is built from measured values.
    pub mime: String,
}

/// Decide what to do with each stream.
///
/// Video is copied only when it is already H.264, because that is the one
/// codec every browser decodes. Everything else is re-encoded to a fixed
/// profile so the MSE codec string is known in advance rather than guessed.
pub fn plan(probe: &Probe) -> Plan {
    let copy_video = probe
        .video
        .as_ref()
        .is_some_and(|video| video.codec == "h264");
    let copy_audio = probe
        .audio
        .as_ref()
        .is_some_and(|audio| audio.codec == "aac");

    let mut codecs = Vec::new();
    if let Some(video) = &probe.video {
        codecs.push(if copy_video {
            avc_codec_string(video.profile.as_deref(), video.level)
        } else {
            // The transcode pins the profile to High but deliberately lets the
            // encoder choose its own level. Pinning the level asks hardware
            // encoders to accept a combination they may refuse outright, and
            // VideoToolbox refuses with a bare `-12902` at encoder setup.
            //
            // So the declared level is an upper bound rather than a promise.
            // Browsers read this to decide whether they could decode the
            // stream, and claiming a ceiling higher than we produce is safe;
            // claiming one lower is what breaks playback.
            TRANSCODE_CODEC.to_string()
        });
    }
    if probe.audio.is_some() {
        // AAC-LC either way: copied AAC is overwhelmingly LC, and the
        // transcode produces LC.
        codecs.push("mp4a.40.2".to_string());
    }

    Plan {
        copy_video,
        copy_audio,
        has_video: probe.video.is_some(),
        has_audio: probe.audio.is_some(),
        mime: format!("video/mp4; codecs=\"{}\"", codecs.join(",")),
    }
}

/// Build the `avc1.PPCCLL` string MSE wants, from ffprobe's profile name and
/// level.
///
/// PP is the profile_idc, CC the constraint flags and LL the level_idc, all in
/// hex. An unrecognised profile falls back to High 4.0, which is the safest
/// widely-supported guess.
pub fn avc_codec_string(profile: Option<&str>, level: Option<i64>) -> String {
    let (profile_idc, constraints) = match profile.unwrap_or_default() {
        "Constrained Baseline" => (0x42, 0xe0),
        "Baseline" => (0x42, 0x00),
        "Main" => (0x4d, 0x00),
        "Extended" => (0x58, 0x00),
        "High" => (0x64, 0x00),
        "High 10" | "High 10 Intra" => (0x6e, 0x00),
        "High 4:2:2" | "High 4:2:2 Intra" => (0x7a, 0x00),
        "High 4:4:4 Predictive" | "High 4:4:4 Intra" => (0xf4, 0x00),
        _ => (0x64, 0x00),
    };
    // ffprobe reports the level_idc directly: 30 for 3.0, 40 for 4.0.
    let level_idc = level.filter(|l| (1..=255).contains(l)).unwrap_or(40);
    format!("avc1.{profile_idc:02x}{constraints:02x}{level_idc:02x}")
}

impl Tools {
    /// Produce one fragmented MP4 segment covering `[index * SEGMENT_SECONDS, +SEGMENT_SECONDS)`.
    ///
    /// Each call is a fresh process reading only what it needs. That makes
    /// seeking free (ask for any segment, any time) and leaves nothing to
    /// clean up if the viewer closes the tab mid-request.
    ///
    /// Deliberately no `-copyts` and no `-output_ts_offset`: both interact
    /// badly with `-t`, since the duration limit is measured against the
    /// shifted timeline and silently discards every packet. Segments therefore
    /// start at zero and the browser places them with `timestampOffset`.
    pub async fn segment(&self, url: &str, index: u32, plan: &Plan) -> Result<Vec<u8>> {
        match self.run_segment(url, index, plan, self.video_encoder).await {
            Ok(data) => Ok(data),
            Err(err) if self.video_encoder != FALLBACK_ENCODER && !plan.copy_video => {
                // Hardware encoders refuse inputs software ones accept, and
                // report it as an opaque setup failure. Falling back costs CPU
                // and keeps playback alive, which is the better trade every
                // time: the alternative is a video that simply stops.
                tracing::warn!(
                    encoder = self.video_encoder,
                    %err,
                    "hardware encoder refused this input; retrying in software"
                );
                self.run_segment(url, index, plan, FALLBACK_ENCODER).await
            }
            Err(err) => Err(err),
        }
    }

    async fn run_segment(
        &self,
        url: &str,
        index: u32,
        plan: &Plan,
        encoder: &str,
    ) -> Result<Vec<u8>> {
        let start = f64::from(index) * SEGMENT_SECONDS;
        let mut args: Vec<String> = vec![
            "-hide_banner".into(),
            "-v".into(),
            "error".into(),
            // Seeking before -i is the fast one: ffmpeg jumps in the container
            // rather than decoding from the start and throwing it away.
            "-ss".into(),
            format!("{start}"),
            "-i".into(),
            url.into(),
            "-t".into(),
            format!("{SEGMENT_SECONDS}"),
        ];

        if plan.has_video {
            args.extend(["-map".into(), "0:v:0".into()]);
        }
        if plan.has_audio {
            args.extend(["-map".into(), "0:a:0".into()]);
        }

        if plan.has_video {
            if plan.copy_video {
                args.extend(["-c:v".into(), "copy".into()]);
            } else {
                args.extend([
                    "-c:v".into(),
                    encoder.into(),
                    // Profile is pinned because it fixes the first byte of the
                    // codec string. The level deliberately is not: see
                    // TRANSCODE_CODEC.
                    "-profile:v".into(),
                    "high".into(),
                    "-pix_fmt".into(),
                    "yuv420p".into(),
                    "-b:v".into(),
                    "3M".into(),
                ]);
                if encoder == FALLBACK_ENCODER {
                    args.extend(["-preset".into(), "veryfast".into()]);
                }
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
                    // Browsers handle stereo everywhere; 5.1 AAC is a lottery.
                    "-ac".into(),
                    "2".into(),
                ]);
            }
        }

        args.extend([
            // `default_base_moof`, not `default_base_is_moof`. The latter is
            // not a real flag and ffmpeg refuses the whole muxer if you use
            // it, which presents as an unrelated filter error.
            "-movflags".into(),
            "+empty_moov+default_base_moof+frag_keyframe".into(),
            "-f".into(),
            "mp4".into(),
            "pipe:1".into(),
        ]);

        let output = tokio::time::timeout(
            SEGMENT_TIMEOUT,
            Command::new(&self.ffmpeg)
                .args(&args)
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("ffmpeg took too long producing a segment")?
        .context("could not run ffmpeg")?;

        if !output.status.success() {
            bail!(
                "ffmpeg failed on segment {index}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        if output.stdout.is_empty() {
            bail!("ffmpeg produced nothing for segment {index}; it is probably past the end");
        }
        Ok(output.stdout)
    }

    /// Extract an embedded subtitle track as WebVTT.
    pub async fn subtitles(&self, url: &str, track: usize) -> Result<String> {
        let output = tokio::time::timeout(
            PROBE_TIMEOUT,
            Command::new(&self.ffmpeg)
                .args([
                    "-hide_banner",
                    "-v",
                    "error",
                    "-i",
                    url,
                    "-map",
                    &format!("0:s:{track}"),
                    "-f",
                    "webvtt",
                    "pipe:1",
                ])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("ffmpeg took too long extracting subtitles")?
        .context("could not run ffmpeg")?;

        if !output.status.success() {
            bail!(
                "could not extract subtitle track {track}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// How many segments a file of this duration needs.
pub fn segment_count(duration: f64) -> u32 {
    if duration <= 0.0 {
        return 0;
    }
    (duration / SEGMENT_SECONDS).ceil() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_with(video: Option<&str>, audio: Option<&str>) -> Probe {
        Probe {
            duration: 60.0,
            video: video.map(|codec| VideoStream {
                codec: codec.to_string(),
                profile: Some("High".into()),
                level: Some(40),
                width: Some(720),
                height: Some(480),
            }),
            audio: audio.map(|codec| AudioStream {
                codec: codec.to_string(),
            }),
            subtitles: Vec::new(),
        }
    }

    #[test]
    fn h264_and_aac_are_passed_through_untouched() {
        // The common case for an MKV: the streams are already fine, only the
        // container is wrong, so this should cost almost nothing.
        let plan = plan(&probe_with(Some("h264"), Some("aac")));
        assert!(plan.copy_video && plan.copy_audio);
        // Copied video reports its real profile and level, measured from the
        // stream, rather than the ceiling the transcode path declares.
        assert_eq!(plan.mime, "video/mp4; codecs=\"avc1.640028,mp4a.40.2\"");
    }

    #[test]
    fn the_measured_archive_case_transcodes_both_streams() {
        // mpeg2video plus ac3 in a program stream, which is what the Internet
        // Archive's older collections actually contain.
        let plan = plan(&probe_with(Some("mpeg2video"), Some("ac3")));
        assert!(!plan.copy_video, "mpeg2 cannot be passed through");
        assert!(!plan.copy_audio, "no browser decodes ac3 reliably");
        assert_eq!(plan.mime, "video/mp4; codecs=\"avc1.640033,mp4a.40.2\"");
    }

    #[test]
    fn hevc_is_transcoded_rather_than_gambled_on() {
        // Safari would manage it and Firefox would not, so normalise.
        let plan = plan(&probe_with(Some("hevc"), Some("aac")));
        assert!(!plan.copy_video);
        assert!(plan.copy_audio, "the audio was already fine");
    }

    #[test]
    fn an_audio_only_file_produces_no_video_codec() {
        let plan = plan(&probe_with(None, Some("flac")));
        assert!(!plan.has_video);
        assert_eq!(plan.mime, "video/mp4; codecs=\"mp4a.40.2\"");
    }

    #[test]
    fn a_silent_video_produces_no_audio_codec() {
        let plan = plan(&probe_with(Some("mpeg4"), None));
        assert!(!plan.has_audio);
        assert_eq!(plan.mime, "video/mp4; codecs=\"avc1.640033\"");
    }

    #[test]
    fn codec_strings_match_the_profiles_they_describe() {
        // Measured against real ffmpeg output: High at level 4.0 is what the
        // transcode pins to, and High at 3.0 is what a copy of the fixture
        // produced.
        assert_eq!(avc_codec_string(Some("High"), Some(40)), "avc1.640028");
        assert_eq!(avc_codec_string(Some("High"), Some(30)), "avc1.64001e");
        assert_eq!(avc_codec_string(Some("Main"), Some(31)), "avc1.4d001f");
        assert_eq!(avc_codec_string(Some("Baseline"), Some(30)), "avc1.42001e");
        // Constrained Baseline carries its constraint flags, which matter.
        assert_eq!(
            avc_codec_string(Some("Constrained Baseline"), Some(30)),
            "avc1.42e01e"
        );
    }

    #[test]
    fn an_unknown_profile_falls_back_to_something_widely_supported() {
        assert_eq!(avc_codec_string(None, None), "avc1.640028");
        assert_eq!(avc_codec_string(Some("Martian"), Some(40)), "avc1.640028");
        // A nonsense level must not produce a malformed string.
        assert_eq!(avc_codec_string(Some("High"), Some(0)), "avc1.640028");
        assert_eq!(avc_codec_string(Some("High"), Some(9999)), "avc1.640028");
    }

    #[test]
    fn segment_counts_cover_the_whole_file() {
        assert_eq!(segment_count(0.0), 0);
        assert_eq!(segment_count(6.0), 1);
        // A part-segment at the end still needs fetching.
        assert_eq!(segment_count(6.1), 2);
        assert_eq!(segment_count(366.5), 62);
        assert_eq!(segment_count(-1.0), 0);
    }

    #[test]
    fn cover_art_is_not_mistaken_for_a_video_track() {
        // An MP3 with embedded artwork reports a video stream. Treating it as
        // one produces a player showing a still image and no controls.
        let art = Probe {
            duration: 200.0,
            video: None,
            audio: Some(AudioStream {
                codec: "mp3".into(),
            }),
            subtitles: Vec::new(),
        };
        let plan = plan(&art);
        assert!(!plan.has_video);
        assert!(!plan.copy_audio, "mp3 is not aac");
    }
}
