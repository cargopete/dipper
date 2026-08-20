//! Talking to ffmpeg.
//!
//! ffmpeg is optional. When it is missing everything here reports as much and
//! the player falls back to offering a download, which is what balerion did
//! before this module existed. The single binary promise survives: transcoding
//! is an enhancement, not a requirement.
//!
//! The pleasant part of this design is that ffmpeg reads balerion's own range
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

/// Decoding a whole film's audio means fetching the whole film, so this is
/// generous. It is bounded at all only so a stalled swarm cannot leave an
/// ffmpeg running for the life of the process.
const AUDIO_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What the page is told a transcode will produce: H.264 High, at a level
/// ceiling rather than an exact level. See the note in [`plan`].
pub const TRANSCODE_CODEC: &str = "avc1.640033";

/// Always available, and tolerant of inputs hardware encoders refuse.
const FALLBACK_ENCODER: &str = "libx264";

/// Hardware encoders worth using, in the order they are worth using.
///
/// All three take the same arguments as libx264 for what balerion asks of them,
/// which is why they are a list and not three code paths. VAAPI is deliberately
/// absent: it needs a render device named on the command line and a hwupload
/// filter chain, so it is not a drop-in name, and it fails in ways that would
/// have to be tested on hardware this was not written on.
const HARDWARE_ENCODERS: &[&str] = &[
    // macOS. Costs almost nothing and leaves the CPU free for the torrent.
    "h264_videotoolbox",
    // NVIDIA.
    "h264_nvenc",
    // Intel QuickSync.
    "h264_qsv",
];

/// Choose an encoder from what this ffmpeg says it has.
fn pick_encoder(listing: &str) -> &'static str {
    HARDWARE_ENCODERS
        .iter()
        .find(|name| listing.contains(*name))
        .copied()
        .unwrap_or(FALLBACK_ENCODER)
}

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

        let listing = Command::new(&ffmpeg)
            .args(["-hide_banner", "-encoders"])
            .output()
            .await
            .ok()?;
        let video_encoder = pick_encoder(&String::from_utf8_lossy(&listing.stdout));

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
    /// Bits per second across the whole file, when ffprobe says.
    pub bit_rate: Option<u64>,
    pub video: Option<VideoStream>,
    /// The chosen audio track, which is the first one unless a caller says
    /// otherwise. Kept for every existing reader of `probe.audio`.
    pub audio: Option<AudioStream>,
    /// Every audio track in the file.
    ///
    /// A film with a commentary or a second language has more than one, and
    /// taking whichever the muxer happened to put first is how a viewer ends up
    /// listening to a director talk over the picture with no way to stop it.
    pub audio_tracks: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
}

impl Probe {
    /// The same probe, as though `track` were the only audio stream.
    ///
    /// Used to build a plan for a chosen track without threading the choice
    /// through every function that already reads `probe.audio`.
    pub fn with_audio(&self, track: usize) -> Self {
        let mut chosen = self.clone();
        chosen.audio = self
            .audio_tracks
            .get(track)
            .cloned()
            .or_else(|| self.audio.clone());
        chosen
    }
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
    /// Index among audio streams, which is what `-map 0:a:N` wants.
    pub index: usize,
    pub language: Option<String>,
    pub title: Option<String>,
    pub channels: Option<u32>,
}

impl AudioStream {
    /// What to call this track in a menu.
    ///
    /// The muxer's own title first, since whoever made the file usually said
    /// something useful ("Commentary", "English 5.1"). Otherwise the language,
    /// otherwise its number, which is at least honest.
    pub fn label(&self) -> String {
        if let Some(title) = self.title.as_ref().filter(|title| !title.trim().is_empty()) {
            return title.clone();
        }
        match (&self.language, self.channels) {
            (Some(language), Some(channels)) => format!("{language} ({channels}ch)"),
            (Some(language), None) => language.clone(),
            (None, _) => format!("Track {}", self.index + 1),
        }
    }
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
    /// Bits per second across the whole file. A string, like everything else
    /// ffprobe reports as a number.
    #[serde(default)]
    bit_rate: Option<String>,
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
    channels: Option<u32>,
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
}

impl Tools {
    /// Ask ffprobe what is in a file.
    ///
    /// `url` points back at balerion's own range endpoint, so this works on a
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
            bit_rate: raw
                .format
                .bit_rate
                .and_then(|rate| rate.parse::<u64>().ok())
                .filter(|rate| *rate > 0),
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
                "audio" => {
                    let stream = AudioStream {
                        codec,
                        index: probe.audio_tracks.len(),
                        language: stream.tags.get("language").cloned(),
                        title: stream.tags.get("title").cloned(),
                        channels: stream.channels,
                    };
                    // The first is the default, which is what every player
                    // does and what the muxer intended.
                    if probe.audio.is_none() {
                        probe.audio = Some(stream.clone());
                    }
                    probe.audio_tracks.push(stream);
                }
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
    /// Which audio stream to take, as `-map 0:a:N` counts them.
    pub audio_track: usize,
    /// What to encode the video at, in bits per second.
    pub video_bitrate: u64,
    /// The MIME type MSE needs, including codecs. Getting this wrong is a
    /// silent stall rather than an error, so it is built from measured values.
    pub mime: String,
}

/// What a given height is worth spending on, in bits per second.
///
/// Everything here used to get three megabits regardless, which is wrong at
/// both ends. A 480p Prelinger short was handed three megabits it could not
/// use, and every one of them came off somebody's swarm; a 1080p feature was
/// handed three when it wanted eight, which is why a good release could come
/// out looking soft.
fn ceiling_for(height: u32) -> u64 {
    match height {
        0..=480 => 1_500_000,
        481..=576 => 2_000_000,
        577..=720 => 3_500_000,
        721..=1080 => 6_000_000,
        1081..=1440 => 10_000_000,
        _ => 16_000_000,
    }
}

/// The bitrate to encode at, given what the source actually is.
///
/// Two rules, and the second matters more than the first. Never spend more than
/// the resolution is worth, and **never spend more than the source has**:
/// re-encoding a two megabit source at six adds no detail whatsoever, it only
/// adds bytes, and on a thin line those bytes are the difference between
/// watching something and waiting for it.
pub fn target_bitrate(probe: &Probe) -> u64 {
    let height = probe.video.as_ref().and_then(|video| video.height);
    let ceiling = ceiling_for(height.unwrap_or(720));

    // The file's rate includes its audio, so a little is taken off before
    // treating it as an upper bound for the video alone.
    let source = probe
        .bit_rate
        .map(|rate| rate.saturating_sub(128_000).max(200_000));

    match source {
        Some(source) => ceiling.min(source),
        // Nothing measured: the resolution's own figure is the best guess
        // available, and guessing low would make every unlabelled file look
        // worse than it is.
        None => ceiling,
    }
}

/// Decide what to do with each stream.
///
/// Audio is copied when it is already AAC. Video never is, even when it is
/// already H.264, and that is the whole of this comment.
///
/// A copied stream cannot be cut anywhere except a keyframe. Ask ffmpeg for the
/// six seconds from 30.0 with `-c:v copy` and it hands back the eight and a bit
/// seconds from the keyframe at 27.83, because it has no way to produce the
/// frames in between. Measured on a 23.976fps BluRay rip: 196 frames where 144
/// were asked for. The audio alongside it is re-encoded and so *is* cut where
/// asked. The player then places that segment at exactly 30.0, and everything in
/// it is two seconds adrift of where the timeline says it is, which looks like
/// sound out of step with picture and behaves like a buffer that never fills.
///
/// Re-encoding costs a hardware encoder very little and makes every segment
/// exactly the length it claims to be. The alternative, cutting segments on
/// keyframe boundaries instead of fixed six second ones, means variable-length
/// segments and a keyframe index for the whole file before playback can start.
/// That is the right answer for a mature player and far too much machinery for
/// this one.
pub fn plan(probe: &Probe) -> Plan {
    // Deliberately not `video.codec == "h264"`. See above.
    let copy_video = false;
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
        audio_track: probe.audio.as_ref().map_or(0, |audio| audio.index),
        video_bitrate: target_bitrate(probe),
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
            // MKVs carry chapters, and the MP4 muxer writes them as a third
            // track with a text handler, which nothing asked for. `-map` does
            // not exclude it because it is not a stream being mapped.
            //
            // It matters because iOS expects a fragmented MP4's init to
            // describe exactly the tracks the segments carry. With the extra
            // track Safari accepts the playlist, fetches the segments, and sits
            // at 0:00 without an error. Measured: three trak boxes without this,
            // two with.
            "-map_chapters".into(),
            "-1".into(),
            "-t".into(),
            format!("{SEGMENT_SECONDS}"),
        ];

        if plan.has_video {
            args.extend(["-map".into(), "0:v:0".into()]);
        }
        if plan.has_audio {
            // The chosen track, not simply the first: a film with a commentary
            // or a second language has more than one, and the muxer's order is
            // not a preference.
            args.extend(["-map".into(), format!("0:a:{}", plan.audio_track)]);
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
                    plan.video_bitrate.to_string(),
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

    /// Decode a file's audio to mono `f32` samples at `rate`.
    ///
    /// For subtitle alignment, which needs to know when somebody was speaking
    /// and nothing else, so this throws away the video, the channels above one
    /// and everything above a few kilohertz before any of it reaches memory. A
    /// two hour film at 16 kHz mono is about 115 MB of samples, which is why
    /// `seconds` exists: pass the duration you actually intend to look at.
    ///
    /// Reads through balerion's own range endpoint like everything else here,
    /// so the piece picker steers for it with no extra plumbing. That does mean
    /// it pulls the whole file off the swarm, which is the honest cost of
    /// listening to all of it.
    pub async fn audio_samples(&self, url: &str, rate: u32, seconds: f64) -> Result<Vec<f32>> {
        let output = tokio::time::timeout(
            AUDIO_TIMEOUT,
            Command::new(&self.ffmpeg)
                .args([
                    "-hide_banner",
                    "-v",
                    "error",
                    "-i",
                    url,
                    // First audio stream only. Which one is the right one is a
                    // separate question; for detecting speech, any of them
                    // will do.
                    "-map",
                    "0:a:0",
                    "-ac",
                    "1",
                    "-ar",
                    &rate.to_string(),
                    "-t",
                    &format!("{seconds}"),
                    // Raw little-endian floats, so there is no container to
                    // parse on this side.
                    "-f",
                    "f32le",
                    "pipe:1",
                ])
                .stdin(Stdio::null())
                .output(),
        )
        .await
        .context("ffmpeg took too long decoding the audio")?
        .context("could not run ffmpeg")?;

        if !output.status.success() {
            bail!(
                "ffmpeg could not decode the audio: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        Ok(output
            .stdout
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
            .collect())
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

    /// One audio track, for fixtures that only care about its codec.
    fn audio(codec: &str) -> AudioStream {
        AudioStream {
            codec: codec.to_string(),
            index: 0,
            language: None,
            title: None,
            channels: Some(2),
        }
    }

    fn probe_of(height: u32, bit_rate: Option<u64>) -> Probe {
        Probe {
            duration: 3600.0,
            bit_rate,
            video: Some(VideoStream {
                codec: "h264".into(),
                profile: Some("High".into()),
                level: Some(40),
                width: Some(height * 16 / 9),
                height: Some(height),
            }),
            audio: Some(audio("aac")),
            audio_tracks: vec![audio("aac")],
            subtitles: Vec::new(),
        }
    }

    #[test]
    fn a_small_source_is_not_handed_a_bitrate_it_cannot_use() {
        // The Prelinger case: 480p at 900 kbit. Encoding it at three megabits
        // spends somebody's swarm on nothing anybody can see.
        let target = target_bitrate(&probe_of(480, Some(900_000)));
        assert!(target < 1_000_000, "{target}");
    }

    #[test]
    fn a_large_source_gets_more_than_the_old_fixed_figure() {
        // 1080p at 8 megabits used to be re-encoded at three, which is why a
        // good release could come out looking soft.
        let target = target_bitrate(&probe_of(1080, Some(8_000_000)));
        assert!(target > 3_000_000, "{target}");
        assert!(target <= 6_000_000, "still capped: {target}");
    }

    #[test]
    fn we_never_spend_more_than_the_source_has() {
        // Re-encoding a two megabit source at six adds no detail, only bytes.
        let target = target_bitrate(&probe_of(1080, Some(2_000_000)));
        assert!(target < 2_000_000, "{target}");
    }

    #[test]
    fn an_unmeasured_source_falls_back_to_what_its_resolution_is_worth() {
        // Guessing low would make every file ffprobe cannot rate look worse
        // than it is.
        assert_eq!(target_bitrate(&probe_of(1080, None)), 6_000_000);
        assert_eq!(target_bitrate(&probe_of(480, None)), 1_500_000);
    }

    #[test]
    fn a_source_of_almost_nothing_still_gets_a_floor() {
        // A 150 kbit file minus the audio allowance would otherwise come out
        // negative, or near enough zero to produce an unwatchable encode.
        let target = target_bitrate(&probe_of(480, Some(150_000)));
        assert!(target >= 200_000, "{target}");
    }

    fn track(
        index: usize,
        language: Option<&str>,
        title: Option<&str>,
        channels: u32,
    ) -> AudioStream {
        AudioStream {
            codec: "ac3".into(),
            index,
            language: language.map(str::to_string),
            title: title.map(str::to_string),
            channels: Some(channels),
        }
    }

    #[test]
    fn a_track_is_labelled_with_whatever_the_muxer_actually_said() {
        // Whoever made the file usually wrote something useful, and it beats
        // anything we could work out.
        assert_eq!(
            track(1, Some("eng"), Some("Commentary"), 2).label(),
            "Commentary"
        );
        assert_eq!(track(0, Some("eng"), None, 6).label(), "eng (6ch)");
        assert_eq!(track(0, Some("fra"), None, 0).label(), "fra (0ch)");
    }

    #[test]
    fn a_nameless_track_is_at_least_numbered_honestly() {
        let anonymous = AudioStream {
            codec: "aac".into(),
            index: 2,
            language: None,
            title: None,
            channels: None,
        };
        assert_eq!(
            anonymous.label(),
            "Track 3",
            "counted from one for a person"
        );
    }

    #[test]
    fn a_blank_title_falls_through_rather_than_showing_an_empty_menu_entry() {
        assert_eq!(track(0, Some("eng"), Some("   "), 2).label(), "eng (2ch)");
    }

    #[test]
    fn choosing_a_track_changes_what_the_plan_maps_and_copies() {
        // The fault this prevents: a viewer picks the second track and gets
        // segments carrying the first, because the plan never heard about it.
        let mut probe = probe_with(Some("h264"), Some("ac3"));
        probe.audio_tracks = vec![
            audio("ac3"),
            AudioStream {
                codec: "aac".into(),
                index: 1,
                language: Some("eng".into()),
                title: None,
                channels: Some(2),
            },
        ];

        let first = plan(&probe.with_audio(0));
        assert_eq!(first.audio_track, 0);
        assert!(!first.copy_audio, "ac3 has to be re-encoded");

        let second = plan(&probe.with_audio(1));
        assert_eq!(second.audio_track, 1);
        assert!(second.copy_audio, "aac can be passed through");
    }

    #[test]
    fn asking_for_a_track_that_is_not_there_falls_back_rather_than_failing() {
        let probe = probe_with(Some("h264"), Some("aac"));
        assert_eq!(plan(&probe.with_audio(9)).audio_track, 0);
    }

    #[test]
    fn a_hardware_encoder_is_preferred_when_ffmpeg_has_one() {
        assert_eq!(
            pick_encoder("V..... h264_videotoolbox  VideoToolbox H.264"),
            "h264_videotoolbox"
        );
        assert_eq!(pick_encoder("V..... h264_nvenc  NVIDIA"), "h264_nvenc");
        assert_eq!(pick_encoder("V..... h264_qsv  QuickSync"), "h264_qsv");
    }

    #[test]
    fn software_encoding_is_the_answer_when_there_is_nothing_else() {
        assert_eq!(pick_encoder("V..... libx264  H.264"), FALLBACK_ENCODER);
        assert_eq!(pick_encoder(""), FALLBACK_ENCODER);
    }

    #[test]
    fn vaapi_is_not_picked_up_by_accident() {
        // It is not a drop-in name: it needs a render device and a hwupload
        // filter chain, and choosing it here would produce a player that fails
        // on every segment.
        assert_eq!(
            pick_encoder("V..... h264_vaapi  VAAPI H.264"),
            FALLBACK_ENCODER
        );
    }

    #[test]
    fn a_file_with_no_video_still_yields_a_plan() {
        let probe = Probe {
            duration: 100.0,
            bit_rate: Some(128_000),
            video: None,
            audio: Some(audio("mp3")),
            audio_tracks: vec![audio("mp3")],
            subtitles: Vec::new(),
        };
        let plan = plan(&probe);
        assert!(!plan.has_video);
        assert!(plan.video_bitrate > 0, "unused, but never nonsense");
    }

    fn probe_with(video: Option<&str>, sound: Option<&str>) -> Probe {
        Probe {
            duration: 60.0,
            bit_rate: None,
            video: video.map(|codec| VideoStream {
                codec: codec.to_string(),
                profile: Some("High".into()),
                level: Some(40),
                width: Some(720),
                height: Some(480),
            }),
            audio: sound.map(audio),
            audio_tracks: sound.map(audio).into_iter().collect(),
            subtitles: Vec::new(),
        }
    }

    #[test]
    fn aac_is_passed_through_and_h264_is_not() {
        // The tempting case: an MKV whose streams are both fine and only the
        // container is wrong. The audio is indeed free. The video is not, and
        // copying it is what put sound out of step with picture.
        //
        // A copied stream can only be cut at a keyframe, so a six second
        // segment comes back eight seconds long, while the re-encoded audio
        // beside it is cut exactly where asked. Measured at 196 frames against
        // 144 on a real 23.976fps rip.
        let plan = plan(&probe_with(Some("h264"), Some("aac")));
        assert!(
            !plan.copy_video,
            "copied video cannot be cut on a segment boundary"
        );
        assert!(plan.copy_audio, "aac is already what we would produce");
        // And so the declared codec is the transcode's ceiling, not the
        // stream's own profile, because the stream is being re-encoded.
        assert_eq!(plan.mime, "video/mp4; codecs=\"avc1.640033,mp4a.40.2\"");
    }

    #[test]
    fn no_input_is_ever_copied_as_video() {
        // The invariant the segmenting depends on: every segment is exactly as
        // long as it claims. Nothing here may quietly reintroduce a copy.
        for codec in ["h264", "hevc", "mpeg2video", "vp9", "av1", "mpeg4"] {
            let plan = plan(&probe_with(Some(codec), Some("aac")));
            assert!(!plan.copy_video, "{codec} was passed through");
        }
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
            bit_rate: None,
            video: None,
            audio: Some(audio("mp3")),
            audio_tracks: vec![audio("mp3")],
            subtitles: Vec::new(),
        };
        let plan = plan(&art);
        assert!(!plan.has_video);
        assert!(!plan.copy_audio, "mp3 is not aac");
    }
}
