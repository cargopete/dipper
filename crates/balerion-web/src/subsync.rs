//! Putting subtitles back in step with the speech.
//!
//! Three different faults hide under "the subtitles are wrong", and only one of
//! them is that there are none.
//!
//! **A constant offset.** A file timed against a different release of the same
//! film starts at a different point, because that release had different
//! leaders, a different logo, or a different cut. Usually a few seconds,
//! occasionally thirty.
//!
//! **Framerate drift.** The one that ruins an evening. A file timed against a
//! 25 fps PAL transfer, played against a 23.976 fps source, drifts by 4.3%.
//! Over ninety minutes that is nearly four minutes adrift by the end, having
//! been perfectly correct at the beginning. No constant offset fixes it, and a
//! viewer who nudges it right at minute ten has to nudge it again at minute
//! twenty.
//!
//! The method is the one ffsubsync uses, and it is worth stating because it is
//! not the obvious one: nothing here reads a word of either the audio or the
//! subtitles. Both sides are reduced to "was somebody speaking during this ten
//! millisecond window", giving two long strings of yes and no, and the question
//! becomes how far to slide one along the other for the best agreement. That is
//! a cross-correlation, which an FFT does in a moment, and it is entirely
//! indifferent to what language anybody is speaking.
//!
//! The parts that decide anything are pure functions over those masks, so the
//! whole judgement is testable without ffmpeg, without audio, and without a
//! film.

use std::f32::consts::PI;
use std::sync::Arc;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex32;

use crate::subtitles::Cue;

/// The resolution everything is quantised to.
///
/// Ten milliseconds is far finer than anyone can perceive a subtitle being out
/// by, and coarse enough that a three hour film is a few hundred thousand
/// samples rather than tens of millions.
pub const FRAME_MS: i64 = 10;

/// Audio is decoded at this rate, mono.
///
/// Speech detection wants nothing above a few kilohertz, and every extra
/// kilohertz is bytes fetched from the swarm for no benefit.
pub const SAMPLE_RATE: u32 = 16_000;

/// How far out of step we are willing to believe a subtitle file is.
///
/// Beyond a couple of minutes the alignment is almost certainly matching the
/// wrong stretch of dialogue, and a confident wrong answer is worse than an
/// admission that we do not know.
pub const MAX_OFFSET_MS: i64 = 120_000;

/// Framerate ratios worth trying, as (numerator, denominator) pairs.
///
/// These are the transfers that actually exist. Film at 24, NTSC's 23.976 and
/// 29.97, PAL's 25, and the speed-up between them, which is where the drift
/// comes from.
const FRAMERATE_RATIOS: &[(f64, f64)] = &[
    (1.0, 1.0),
    (25.0, 23.976),
    (23.976, 25.0),
    (25.0, 24.0),
    (24.0, 25.0),
    (24.0, 23.976),
    (23.976, 24.0),
    (30.0, 29.97),
    (29.97, 30.0),
];

/// A scale factor further from 1.0 than this is not a framerate difference.
///
/// ffsubsync's own default, and it rejects nothing legitimate: the widest real
/// ratio here is 25/23.976, which is 4.3%.
const MAX_SCALE_DEVIATION: f64 = 0.1;

/// Below this agreement, we do not claim to know where the subtitles go.
///
/// Deliberately a refusal rather than a best guess. A track that is slightly
/// out is annoying; one that has been confidently moved somewhere worse is how
/// a viewer learns to distrust the feature and turn it off for ever.
pub const MIN_CONFIDENCE: f64 = 0.25;

/// What the alignment came to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Alignment {
    /// Milliseconds to add to every cue.
    pub offset_ms: i64,
    /// Multiplier applied to every timestamp before the offset. 1.0 when no
    /// framerate difference was found, which is the common case.
    pub scale: f64,
    /// How well the two speech patterns agreed, from 0 to 1.
    pub confidence: f64,
}

impl Alignment {
    /// Is this worth acting on?
    pub fn is_trustworthy(&self) -> bool {
        self.confidence >= MIN_CONFIDENCE
            && self.offset_ms.abs() <= MAX_OFFSET_MS
            && (self.scale - 1.0).abs() <= MAX_SCALE_DEVIATION
    }

    /// Does applying this actually change anything a viewer would notice?
    ///
    /// A hundred milliseconds is below the threshold at which anybody can tell,
    /// so rewriting a whole track for less than that is work for its own sake.
    pub fn is_worth_applying(&self) -> bool {
        self.is_trustworthy() && (self.offset_ms.abs() >= 100 || (self.scale - 1.0).abs() > 1e-6)
    }
}

/// Move cues by an alignment. Scale first, then offset.
pub fn apply(cues: &[Cue], alignment: Alignment) -> Vec<Cue> {
    let shift = |ms: i64| ((ms as f64 * alignment.scale).round() as i64) + alignment.offset_ms;
    cues.iter()
        .map(|cue| Cue {
            start_ms: shift(cue.start_ms),
            end_ms: shift(cue.end_ms),
            text: cue.text.clone(),
        })
        .collect()
}

/// Reduce a subtitle track to "was a cue on screen during this frame".
///
/// The reference the audio is compared against. A cue being on screen is a
/// decent proxy for somebody speaking, which is the only thing both sides have
/// in common.
pub fn mask_from_cues(cues: &[Cue], frames: usize) -> Vec<f32> {
    let mut mask = vec![0.0; frames];
    for cue in cues {
        if cue.end_ms <= cue.start_ms {
            continue;
        }
        let from = (cue.start_ms.max(0) / FRAME_MS) as usize;
        let to = (cue.end_ms.max(0) / FRAME_MS) as usize;
        if from >= frames {
            continue;
        }
        mask[from..to.min(frames)].fill(1.0);
    }
    mask
}

/// How many frames a track of this length needs.
pub fn frames_for(duration_ms: i64) -> usize {
    (duration_ms.max(0) / FRAME_MS) as usize + 1
}

/// Reduce decoded audio to "was somebody speaking during this frame".
///
/// An energy detector rather than a trained one. It is not trying to tell
/// speech from a door slamming, and it does not need to: what it produces is
/// compared against subtitle timings, and dialogue is overwhelmingly what a
/// film is loudest about while a subtitle is on screen.
///
/// The threshold is derived from the recording rather than fixed, because the
/// same absolute level is silence in one film and shouting in another. A
/// fraction of the loudness the film actually reaches works across both.
pub fn mask_from_audio(samples: &[f32]) -> Vec<f32> {
    let per_frame = (SAMPLE_RATE as i64 * FRAME_MS / 1000) as usize;
    if per_frame == 0 || samples.is_empty() {
        return Vec::new();
    }

    // Root mean square per frame: loudness, not amplitude, so one stray sample
    // does not mark a frame as speech.
    let energies: Vec<f32> = samples
        .chunks(per_frame)
        .map(|chunk| {
            let sum: f32 = chunk.iter().map(|sample| sample * sample).sum();
            (sum / chunk.len() as f32).sqrt()
        })
        .collect();

    // A high percentile rather than the maximum, so a single clipped frame
    // cannot set the scale for the whole film and push everything else below
    // the threshold.
    let mut sorted = energies.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let loud = sorted[(sorted.len() * 95 / 100).min(sorted.len().saturating_sub(1))];
    let threshold = loud * 0.15;

    energies
        .iter()
        .map(|energy| if *energy > threshold { 1.0 } else { 0.0 })
        .collect()
}

/// Find where `subject` sits against `reference`, trying framerate ratios too.
///
/// Both are speech masks at [`FRAME_MS`] resolution. The reference is the
/// audio; the subject is the subtitle track that may need moving.
pub fn align(reference: &[f32], subject: &[f32]) -> Alignment {
    let mut best = Alignment {
        offset_ms: 0,
        scale: 1.0,
        confidence: 0.0,
    };

    for (numerator, denominator) in FRAMERATE_RATIOS {
        let scale = numerator / denominator;
        if (scale - 1.0).abs() > MAX_SCALE_DEVIATION {
            continue;
        }
        let scaled = resample(subject, scale);
        let (frames, score) = best_offset(reference, &scaled);
        if score > best.confidence {
            best = Alignment {
                offset_ms: frames * FRAME_MS,
                scale,
                confidence: score,
            };
        }
    }
    best
}

/// Stretch or squeeze a mask by `scale`, keeping its resolution.
fn resample(mask: &[f32], scale: f64) -> Vec<f32> {
    if (scale - 1.0).abs() < 1e-9 {
        return mask.to_vec();
    }
    let length = ((mask.len() as f64) * scale).round() as usize;
    (0..length)
        .map(|index| {
            let source = ((index as f64) / scale).round() as usize;
            mask.get(source).copied().unwrap_or(0.0)
        })
        .collect()
}

/// The shift, in frames, that makes the two masks agree best.
///
/// Positive means the subject has to move later. The score is the overlap at
/// that shift divided by how much of the subject there was to overlap, so it
/// reads as a fraction: 1.0 means every subtitled frame landed on speech.
pub fn best_offset(reference: &[f32], subject: &[f32]) -> (i64, f64) {
    if reference.is_empty() || subject.is_empty() {
        return (0, 0.0);
    }

    let correlation = cross_correlate(reference, subject);
    // The correlation is circular, so an index in the top half is a positive
    // shift and one in the bottom half is a negative shift wrapped around.
    let size = correlation.len();
    let limit = (MAX_OFFSET_MS / FRAME_MS) as usize;

    let mut best_index = 0usize;
    let mut best_value = f32::MIN;
    for (index, value) in correlation.iter().enumerate() {
        let shift = if index <= size / 2 {
            index as i64
        } else {
            index as i64 - size as i64
        };
        if shift.unsigned_abs() as usize > limit {
            continue;
        }
        if *value > best_value {
            best_value = *value;
            best_index = index;
        }
    }

    let shift = if best_index <= size / 2 {
        best_index as i64
    } else {
        best_index as i64 - size as i64
    };
    /* Normalised by the geometric mean of the two, which is the detail that
     * decides whether this feature helps or harms.
     *
     * The obvious normalisation, dividing by the smaller of the two, scores a
     * perfect 1.0 for a subtitle file with three short cues placed against a
     * film full of dialogue: every one of them lands on speech wherever you put
     * it, so the answer is meaningless and the confidence is total. Dividing by
     * the geometric mean asks a better question, namely how much of *both* sides
     * agreed, and gives that case about 0.03. */
    let reference_speech = reference.iter().sum::<f32>();
    let subject_speech = subject.iter().sum::<f32>();
    let weight = (reference_speech * subject_speech).sqrt().max(1.0);
    (shift, (best_value.max(0.0) / weight).min(1.0) as f64)
}

/// Circular cross-correlation of two real signals, via the FFT.
///
/// Direct correlation would be perfectly correct and unusably slow: a two hour
/// film is 720,000 frames, and searching two minutes of offsets over that is
/// billions of multiplications. Through the frequency domain it is a few
/// transforms and a pointwise product.
fn cross_correlate(reference: &[f32], subject: &[f32]) -> Vec<f32> {
    let size = (reference.len() + subject.len()).next_power_of_two();
    let mut planner = FftPlanner::new();
    let forward = planner.plan_fft_forward(size);
    let inverse = planner.plan_fft_inverse(size);

    let mut a = to_complex(reference, size);
    let mut b = to_complex(subject, size);
    forward.process(&mut a);
    forward.process(&mut b);

    // Correlation rather than convolution: conjugate one side.
    for (left, right) in a.iter_mut().zip(b.iter()) {
        *left *= right.conj();
    }
    inverse.process(&mut a);

    let norm = 1.0 / size as f32;
    a.iter().map(|value| value.re * norm).collect()
}

fn to_complex(values: &[f32], size: usize) -> Vec<Complex32> {
    let mut out = vec![Complex32::new(0.0, 0.0); size];
    for (slot, value) in out.iter_mut().zip(values) {
        slot.re = *value;
    }
    out
}

/// Silence the unused-import warning in builds where no test uses PI, while
/// keeping the constant available to the synthetic-audio fixtures below.
#[allow(dead_code)]
const _PI: f32 = PI;

/// Decode a file's audio to mono 16 kHz samples, through ffmpeg.
///
/// The only impure part of this module, and it reads back through balerion's
/// own range endpoint like everything else, so the piece picker steers for it
/// with no extra plumbing.
pub async fn decode_audio(
    tools: &crate::ffmpeg::Tools,
    url: &str,
    seconds: f64,
) -> anyhow::Result<Vec<f32>> {
    tools.audio_samples(url, SAMPLE_RATE, seconds).await
}

/// How much of a film to listen to before deciding.
///
/// Bounded, and the bound is the whole design. Decoding a film's audio means
/// fetching the film, so listening to all of it would have a subtitle track
/// quietly pull a two gigabyte download in front of the piece the player is
/// waiting on. The first quarter of an hour is bytes the player is fetching
/// anyway, since that is where the playhead starts.
///
/// Fifteen minutes is also enough to see a framerate difference rather than
/// only an offset: 4.3% over that span is thirty-eight seconds of drift, which
/// no misplaced constant offset can imitate.
pub const WINDOW_SECONDS: f64 = 900.0;

/// Align a subtitle track against a file's audio.
///
/// Returns the alignment whether or not it is worth acting on; deciding that is
/// [`Alignment::is_trustworthy`]'s job and the caller's, because "we looked and
/// we are not sure" is a useful thing to be able to say.
pub async fn align_to_audio(
    tools: &crate::ffmpeg::Tools,
    url: &str,
    cues: &[Cue],
    duration: f64,
) -> anyhow::Result<Alignment> {
    let window = if duration > 0.0 {
        duration.min(WINDOW_SECONDS)
    } else {
        WINDOW_SECONDS
    };
    let samples = decode_audio(tools, url, window).await?;
    let reference = mask_from_audio(&samples);
    if reference.is_empty() {
        anyhow::bail!("there is no audio in this file to compare against");
    }
    // The subject is cut to the same span, or a whole film's worth of cues
    // would be compared against a quarter of an hour of audio and the
    // agreement would read as poor when it was merely lopsided.
    let subject = mask_from_cues(cues, reference.len());
    Ok(align(&reference, &subject))
}

/// Shared handle to a computed alignment, so a second request for the same
/// track does not decode the audio again.
pub type Cached = Arc<Alignment>;

#[cfg(test)]
mod tests {
    use super::*;

    /// A subtitle track: cues every `gap` ms, each `length` ms long.
    fn cues(count: usize, gap: i64, length: i64) -> Vec<Cue> {
        (0..count as i64)
            .map(|index| Cue {
                start_ms: index * gap,
                end_ms: index * gap + length,
                text: format!("line {index}"),
            })
            .collect()
    }

    #[test]
    fn a_track_already_in_step_is_left_where_it_is() {
        let reference = mask_from_cues(&cues(40, 3_000, 1_500), 15_000);
        let alignment = align(&reference, &reference);
        assert_eq!(alignment.offset_ms, 0);
        assert!(alignment.confidence > 0.9, "{alignment:?}");
        assert!(
            !alignment.is_worth_applying(),
            "nothing to do: {alignment:?}"
        );
    }

    #[test]
    fn a_track_running_late_is_pulled_back() {
        // The common case: subtitles made for a release with a longer leader.
        let truth = cues(40, 3_000, 1_500);
        let late: Vec<Cue> = truth
            .iter()
            .map(|cue| Cue {
                start_ms: cue.start_ms + 4_200,
                end_ms: cue.end_ms + 4_200,
                text: cue.text.clone(),
            })
            .collect();

        let reference = mask_from_cues(&truth, 15_000);
        let subject = mask_from_cues(&late, 15_000);
        let alignment = align(&reference, &subject);

        assert!(alignment.is_trustworthy(), "{alignment:?}");
        // Within one frame of the truth, and the sign says "move earlier".
        assert!(
            (alignment.offset_ms + 4_200).abs() <= FRAME_MS,
            "{alignment:?}"
        );
    }

    #[test]
    fn a_track_running_early_is_pushed_later() {
        let truth = cues(40, 3_000, 1_500);
        let early: Vec<Cue> = truth
            .iter()
            .map(|cue| Cue {
                start_ms: cue.start_ms - 2_500,
                end_ms: cue.end_ms - 2_500,
                text: cue.text.clone(),
            })
            .collect();

        let reference = mask_from_cues(&truth, 15_000);
        let subject = mask_from_cues(&early, 15_000);
        let alignment = align(&reference, &subject);

        assert!(alignment.is_trustworthy(), "{alignment:?}");
        assert!(
            (alignment.offset_ms - 2_500).abs() <= FRAME_MS,
            "{alignment:?}"
        );
    }

    #[test]
    fn pal_speed_up_is_recognised_as_a_framerate_and_not_as_an_offset() {
        // This is the one worth having. A file timed against 25 fps played at
        // 23.976 drifts 4.3%: correct at the start, four minutes out by the end
        // of a feature. No constant offset can fix it.
        // Three quarters of an hour of dialogue: long enough that 4.3% is two
        // minutes of drift by the end, short enough not to make the suite crawl.
        let truth = cues(45, 60_000, 2_000);
        let ratio = 25.0 / 23.976;
        let drifting: Vec<Cue> = truth
            .iter()
            .map(|cue| Cue {
                start_ms: (cue.start_ms as f64 / ratio).round() as i64,
                end_ms: (cue.end_ms as f64 / ratio).round() as i64,
                text: cue.text.clone(),
            })
            .collect();

        let frames = frames_for(45 * 60 * 1000);
        let reference = mask_from_cues(&truth, frames);
        let subject = mask_from_cues(&drifting, frames);
        let alignment = align(&reference, &subject);

        assert!(alignment.is_trustworthy(), "{alignment:?}");
        assert!(
            (alignment.scale - ratio).abs() < 0.005,
            "expected roughly {ratio}, got {alignment:?}"
        );

        // And applying it should put the last cue back where it belongs, which
        // is the whole point: the first one was never wrong.
        let fixed = apply(&drifting, alignment);
        let last_error = (fixed.last().unwrap().start_ms - truth.last().unwrap().start_ms).abs();
        assert!(last_error < 1_000, "{last_error}ms out at the end");
    }

    #[test]
    fn two_unrelated_tracks_produce_an_answer_we_refuse_to_trust() {
        // Dialogue at wildly different rhythms: there is no right answer here,
        // and inventing one is how a viewer ends up worse off than before.
        let reference = mask_from_cues(&cues(200, 1_000, 900), 60_000);
        let subject = mask_from_cues(&cues(3, 40_000, 200), 60_000);
        let alignment = align(&reference, &subject);
        assert!(
            !alignment.is_trustworthy() || alignment.confidence < 0.9,
            "far too confident about nonsense: {alignment:?}"
        );
    }

    #[test]
    fn an_offset_beyond_what_we_believe_is_refused() {
        let alignment = Alignment {
            offset_ms: MAX_OFFSET_MS + 1,
            scale: 1.0,
            confidence: 1.0,
        };
        assert!(!alignment.is_trustworthy());
    }

    #[test]
    fn a_scale_that_is_not_a_framerate_difference_is_refused() {
        let alignment = Alignment {
            offset_ms: 0,
            scale: 1.5,
            confidence: 1.0,
        };
        assert!(!alignment.is_trustworthy());
    }

    #[test]
    fn empty_input_is_an_answer_of_no_confidence_rather_than_a_panic() {
        assert_eq!(best_offset(&[], &[1.0]), (0, 0.0));
        assert_eq!(best_offset(&[1.0], &[]), (0, 0.0));
        assert!(mask_from_audio(&[]).is_empty());
        assert!(mask_from_cues(&[], 0).is_empty());
    }

    #[test]
    fn applying_an_alignment_scales_before_it_shifts() {
        let moved = apply(
            &[Cue {
                start_ms: 1_000,
                end_ms: 2_000,
                text: "x".into(),
            }],
            Alignment {
                offset_ms: 500,
                scale: 2.0,
                confidence: 1.0,
            },
        );
        assert_eq!(moved[0].start_ms, 2_500);
        assert_eq!(moved[0].end_ms, 4_500);
    }

    #[test]
    fn loud_stretches_of_audio_are_marked_as_speech_and_quiet_ones_are_not() {
        let per_frame = (SAMPLE_RATE as i64 * FRAME_MS / 1000) as usize;
        let mut samples = vec![0.0f32; per_frame * 100];
        // Frames 30 to 60 are loud; a sine rather than a constant, so the RMS
        // is a realistic fraction of the peak.
        for frame in 30..60 {
            for index in 0..per_frame {
                let phase = (index as f32) / (per_frame as f32) * PI * 20.0;
                samples[frame * per_frame + index] = phase.sin() * 0.8;
            }
        }

        let mask = mask_from_audio(&samples);
        assert_eq!(mask.len(), 100);
        assert!(mask[..30].iter().all(|frame| *frame == 0.0), "silence");
        assert!(mask[30..60].iter().all(|frame| *frame == 1.0), "speech");
        assert!(mask[60..].iter().all(|frame| *frame == 0.0), "silence");
    }

    #[test]
    fn a_subtitle_track_is_aligned_against_real_audio_energy() {
        // The whole pipeline, minus ffmpeg: build audio that is loud exactly
        // where dialogue happens, offset the subtitles, and see them pulled
        // back.
        let per_frame = (SAMPLE_RATE as i64 * FRAME_MS / 1000) as usize;
        let truth = cues(30, 4_000, 2_000);
        let total_frames = frames_for(30 * 4_000);
        let mut samples = vec![0.0f32; per_frame * total_frames];
        for cue in &truth {
            let from = (cue.start_ms / FRAME_MS) as usize;
            let to = (cue.end_ms / FRAME_MS) as usize;
            for frame in from..to.min(total_frames) {
                for index in 0..per_frame {
                    let phase = (index as f32) / (per_frame as f32) * PI * 20.0;
                    samples[frame * per_frame + index] = phase.sin() * 0.6;
                }
            }
        }

        let late: Vec<Cue> = truth
            .iter()
            .map(|cue| Cue {
                start_ms: cue.start_ms + 3_000,
                end_ms: cue.end_ms + 3_000,
                text: cue.text.clone(),
            })
            .collect();

        let reference = mask_from_audio(&samples);
        let subject = mask_from_cues(&late, reference.len());
        let alignment = align(&reference, &subject);

        assert!(alignment.is_trustworthy(), "{alignment:?}");
        assert!(
            (alignment.offset_ms + 3_000).abs() <= 2 * FRAME_MS,
            "{alignment:?}"
        );
    }
}
