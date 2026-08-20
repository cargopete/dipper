//! Turning subtitle files into something a `<track>` element will accept.
//!
//! Browsers take WebVTT and nothing else. SubRip is nearly the same format,
//! which makes the conversion short, but "nearly" hides two traps that bite
//! every implementation: SubRip uses a comma before the milliseconds where
//! WebVTT uses a full stop, and SubRip files in the wild are frequently not
//! UTF-8. Getting the second one wrong produces mojibake rather than an error,
//! so it is handled explicitly.

/// Decode subtitle bytes, falling back when they are not UTF-8.
///
/// SubRip predates the general adoption of UTF-8 and a great many files are
/// Windows-1252 or Latin-1. Those decode as invalid UTF-8, and lossy decoding
/// would litter the text with replacement characters. Windows-1252 is a
/// superset of Latin-1 that happens to fill in the range Latin-1 leaves as
/// control codes, so it is the better guess of the two.
pub fn decode(bytes: &[u8]) -> String {
    // Strip a UTF-8 byte order mark, which otherwise becomes a stray
    // character at the front of the first cue.
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);

    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|byte| windows_1252(*byte)).collect(),
    }
}

/// Map one Windows-1252 byte to its character.
///
/// Only 0x80 to 0x9F differ from Latin-1, where Latin-1 has unused control
/// codes and Windows-1252 has the punctuation that actually turns up in
/// subtitles: curly quotes, dashes and ellipses.
fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}',
        '\u{017D}', '\u{FFFD}', '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}',
        '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    ];
    match byte {
        0x80..=0x9F => HIGH[(byte - 0x80) as usize],
        // Everything else in Latin-1 maps to the codepoint of the same value.
        other => char::from(other),
    }
}

/// One subtitle line, with its timings as numbers.
///
/// The line-by-line conversion below is enough to *show* a subtitle file and
/// useless for doing anything to it. Moving a track that is out of step with
/// the speech means arithmetic on the timings, which means parsing them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cue {
    pub start_ms: i64,
    pub end_ms: i64,
    /// Everything between the timing line and the blank line, newlines and all.
    pub text: String,
}

/// Read the cues out of a SubRip or WebVTT file.
///
/// Deliberately forgiving, and for the same reason the converter is: these
/// files are hand-edited more than anything else here, and one malformed cue
/// should cost that cue rather than the track. Anything without a parsable
/// timing line is skipped, including the `WEBVTT` header, sequence numbers,
/// `NOTE` blocks and `STYLE` blocks, none of which carry dialogue.
pub fn parse_cues(input: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut pending: Option<(i64, i64)> = None;
    let mut text = String::new();

    let finish = |cues: &mut Vec<Cue>, pending: &mut Option<(i64, i64)>, text: &mut String| {
        if let Some((start_ms, end_ms)) = pending.take() {
            cues.push(Cue {
                start_ms,
                end_ms,
                text: text.trim_end().to_string(),
            });
        }
        text.clear();
    };

    for line in input.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(timing) = parse_timing(line) {
            // A new timing line ends whatever came before it, even when the
            // file forgot its blank line.
            finish(&mut cues, &mut pending, &mut text);
            pending = Some(timing);
            continue;
        }
        if line.trim().is_empty() {
            finish(&mut cues, &mut pending, &mut text);
            continue;
        }
        if pending.is_some() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(line);
        }
    }
    finish(&mut cues, &mut pending, &mut text);
    cues
}

/// Parse `00:00:01,000 --> 00:00:04,000`, in either dialect.
fn parse_timing(line: &str) -> Option<(i64, i64)> {
    let (start, rest) = line.split_once("-->")?;
    // WebVTT allows positioning settings after the end time, separated by
    // whitespace. They are not part of the timestamp.
    let end = rest.split_whitespace().next()?;
    Some((parse_timestamp(start.trim())?, parse_timestamp(end)?))
}

/// `hh:mm:ss,mmm`, `hh:mm:ss.mmm` or WebVTT's `mm:ss.mmm`.
fn parse_timestamp(value: &str) -> Option<i64> {
    // A comma is SubRip's decimal separator; everything else is the same.
    let value = value.replace(',', ".");
    let (clock, fraction) = match value.split_once('.') {
        Some((clock, fraction)) => (clock, fraction),
        None => (value.as_str(), "0"),
    };

    let mut seconds = 0i64;
    let mut parts = 0;
    for part in clock.split(':') {
        let part: i64 = part.trim().parse().ok()?;
        if part < 0 {
            return None;
        }
        seconds = seconds.checked_mul(60)?.checked_add(part)?;
        parts += 1;
    }
    if !(2..=3).contains(&parts) {
        return None;
    }

    // Pad or truncate to milliseconds: `.5` is half a second, not five.
    let digits: String = fraction.chars().filter(char::is_ascii_digit).collect();
    let millis: i64 = format!("{digits:0<3}")[..3].parse().ok()?;
    seconds.checked_mul(1000)?.checked_add(millis)
}

/// Write cues back out as WebVTT.
pub fn cues_to_vtt(cues: &[Cue]) -> String {
    cues_to_vtt_noted(cues, None)
}

/// The same, with a `NOTE` block recording what was done to the timings.
///
/// A `NOTE` is part of WebVTT and no player renders one, so this puts the
/// decision where it stays attached to the track without putting anything on
/// screen. The header goes in exactly once, which sounds too obvious to be
/// worth saying and was not.
pub fn cues_to_vtt_noted(cues: &[Cue], note: Option<&str>) -> String {
    let mut out = String::from("WEBVTT\n\n");
    if let Some(note) = note {
        // Blank lines would end the block early, so they are folded out.
        out.push_str("NOTE ");
        out.push_str(&note.replace("\n\n", "\n").replace('\r', ""));
        out.push_str("\n\n");
    }
    for cue in cues {
        // A cue that has been shifted off the front of the film is clamped
        // rather than dropped: negative timestamps are not legal WebVTT, and
        // losing the first line of dialogue is worse than showing it early.
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            timestamp(cue.start_ms.max(0)),
            timestamp(cue.end_ms.max(0)),
            cue.text
        ));
    }
    out
}

fn timestamp(ms: i64) -> String {
    let ms = ms.max(0);
    let (hours, rest) = (ms / 3_600_000, ms % 3_600_000);
    let (minutes, rest) = (rest / 60_000, rest % 60_000);
    format!(
        "{hours:02}:{minutes:02}:{:02}.{:03}",
        rest / 1000,
        rest % 1000
    )
}

/// Convert SubRip to WebVTT.
///
/// Deliberately forgiving: subtitle files are edited by hand more often than
/// any other format here, and a malformed cue should cost the viewer that one
/// line rather than the whole track.
pub fn srt_to_vtt(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    out.push_str("WEBVTT\n\n");

    for line in input.lines() {
        // Lone carriage returns survive `lines()` when the file uses CRLF and
        // would end up inside cue text.
        let line = line.trim_end_matches('\r');

        if line.contains("-->") {
            out.push_str(&convert_timing(line));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Rewrite a timing line, which is the only line whose syntax differs.
///
/// SubRip writes `00:00:01,000 --> 00:00:04,000`, WebVTT wants full stops.
/// Some files carry trailing positioning data, which WebVTT also understands,
/// so it is left alone.
fn convert_timing(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for character in line.chars() {
        // Only commas sitting between digits are decimal separators. A comma
        // in positioning metadata must survive untouched.
        out.push(if character == ',' { '.' } else { character });
    }
    out
}

/// Is this a subtitle file we can convert?
pub fn is_subtitle(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".srt") || lower.ends_with(".vtt")
}

/// Does `candidate` look like the subtitles for `video`?
///
/// Matched on the filename stem, so `film.mkv` finds `film.srt` and
/// `film.en.srt`. Torrents also park subtitles in a `Subs/` directory, so the
/// directory part is ignored.
pub fn belongs_to(video_path: &str, candidate_path: &str) -> bool {
    if !is_subtitle(candidate_path) {
        return false;
    }
    let stem = |path: &str| {
        path.rsplit('/')
            .next()
            .unwrap_or(path)
            .rsplit_once('.')
            .map(|(head, _)| head.to_ascii_lowercase())
            .unwrap_or_default()
    };

    let video = stem(video_path);
    let subtitle = stem(candidate_path);
    if video.is_empty() || subtitle.is_empty() {
        return false;
    }
    // `film.en` should match `film`, and an exact match obviously counts.
    subtitle == video || subtitle.starts_with(&format!("{video}."))
}

/// A language tag for the `<track>` element, guessed from the filename.
///
/// Only recognises the two letter code some files put before the extension.
/// Wrong guesses are cosmetic: the track still plays, it is just labelled
/// unhelpfully.
pub fn language_of(path: &str) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let without_extension = name.rsplit_once('.')?.0;
    let tag = without_extension.rsplit_once('.')?.1;
    (tag.len() == 2 && tag.chars().all(|c| c.is_ascii_alphabetic())).then(|| tag.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cues_are_read_out_of_subrip() {
        let srt = "1\n00:00:01,000 --> 00:00:04,500\nFirst line\nsecond row\n\n\
                   2\n00:01:02,250 --> 00:01:03,000\nLater\n";
        let cues = parse_cues(srt);
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert_eq!(cues[0].start_ms, 1_000);
        assert_eq!(cues[0].end_ms, 4_500);
        assert_eq!(cues[0].text, "First line\nsecond row");
        assert_eq!(cues[1].start_ms, 62_250);
        assert_eq!(cues[1].text, "Later");
    }

    #[test]
    fn cues_are_read_out_of_webvtt_including_its_short_timestamps() {
        let vtt = "WEBVTT\n\nNOTE something\n\n00:01.000 --> 00:04.000 line:0\nHello\n";
        let cues = parse_cues(vtt);
        assert_eq!(cues.len(), 1, "{cues:?}");
        assert_eq!(cues[0].start_ms, 1_000);
        assert_eq!(cues[0].end_ms, 4_000, "settings are not part of the time");
        assert_eq!(cues[0].text, "Hello");
    }

    #[test]
    fn a_missing_blank_line_does_not_swallow_the_next_cue() {
        // Hand-edited files do this constantly.
        let srt = "00:00:01,000 --> 00:00:02,000\nOne\n00:00:03,000 --> 00:00:04,000\nTwo\n";
        let cues = parse_cues(srt);
        assert_eq!(cues.len(), 2, "{cues:?}");
        assert_eq!(cues[0].text, "One");
        assert_eq!(cues[1].text, "Two");
    }

    #[test]
    fn a_file_of_headers_and_nothing_else_yields_no_cues() {
        assert!(parse_cues("WEBVTT\n\n").is_empty());
        assert!(parse_cues("").is_empty());
        assert!(parse_cues("1\n2\n3\n").is_empty());
    }

    #[test]
    fn an_annotated_track_has_exactly_one_header() {
        // Two WEBVTT lines is not a WebVTT file, and a browser answers that
        // with an empty subtitle track and no error whatsoever.
        let vtt = cues_to_vtt_noted(
            &[Cue {
                start_ms: 0,
                end_ms: 1_000,
                text: "x".into(),
            }],
            Some("moved by -4200ms"),
        );
        assert_eq!(vtt.matches("WEBVTT").count(), 1, "{vtt}");
        assert!(vtt.contains("NOTE moved by -4200ms"), "{vtt}");
        // And the cues still parse back out, note and all.
        assert_eq!(parse_cues(&vtt).len(), 1);
    }

    #[test]
    fn cues_survive_a_round_trip_through_webvtt() {
        let original = vec![
            Cue {
                start_ms: 1_000,
                end_ms: 4_500,
                text: "First\nsecond".into(),
            },
            Cue {
                start_ms: 3_723_004,
                end_ms: 3_724_000,
                text: "An hour in".into(),
            },
        ];
        let vtt = cues_to_vtt(&original);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("01:02:03.004 --> 01:02:04.000"), "{vtt}");
        assert_eq!(parse_cues(&vtt), original);
    }

    #[test]
    fn a_cue_pushed_before_the_start_is_clamped_rather_than_written_negative() {
        // A negative timestamp is not legal WebVTT, and dropping the line
        // would lose dialogue over an arithmetic detail.
        let vtt = cues_to_vtt(&[Cue {
            start_ms: -2_000,
            end_ms: 500,
            text: "Early".into(),
        }]);
        assert!(!vtt.contains("-00"), "no negative timestamp: {vtt}");
        assert!(vtt.contains("00:00:00.000 --> 00:00:00.500"), "{vtt}");
    }

    #[test]
    fn a_short_fraction_is_read_as_a_fraction_not_as_milliseconds() {
        // `.5` is half a second. Reading it as five milliseconds puts every
        // cue in the file a fraction of a second early.
        assert_eq!(parse_timestamp("00:00:01.5"), Some(1_500));
        assert_eq!(parse_timestamp("00:00:01.05"), Some(1_050));
        assert_eq!(parse_timestamp("00:00:01.005"), Some(1_005));
    }

    #[test]
    fn nonsense_timestamps_are_refused() {
        assert_eq!(parse_timestamp("banana"), None);
        assert_eq!(parse_timestamp("61"), None, "one part is not a clock");
        assert_eq!(parse_timestamp("1:2:3:4"), None);
    }

    #[test]
    fn converts_the_timestamp_separator() {
        let srt = "1\n00:00:01,000 --> 00:00:04,000\nFirst line\n\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.starts_with("WEBVTT\n\n"), "{vtt}");
        assert!(vtt.contains("00:00:01.000 --> 00:00:04.000"), "{vtt}");
        assert!(!vtt.contains(','), "no comma should survive in a timing");
        assert!(vtt.contains("First line"));
    }

    #[test]
    fn survives_crlf_line_endings() {
        // Almost every subtitle file from a Windows machine looks like this,
        // and a stray carriage return lands inside the cue text.
        let srt = "1\r\n00:00:01,000 --> 00:00:04,000\r\nA line\r\n\r\n";
        let vtt = srt_to_vtt(srt);
        assert!(!vtt.contains('\r'), "carriage returns must not survive");
        assert!(vtt.contains("00:00:01.000 --> 00:00:04.000"));
    }

    #[test]
    fn strips_a_byte_order_mark() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"1\n00:00:01,000 --> 00:00:02,000\nHello\n");
        let text = decode(&bytes);
        assert!(text.starts_with('1'), "BOM leaked into the text: {text:?}");
    }

    #[test]
    fn windows_1252_decodes_rather_than_producing_mojibake() {
        // 0x93 and 0x94 are curly quotes in Windows-1252 and invalid UTF-8.
        let bytes = [b'S', b'a', b'y', b' ', 0x93, b'h', b'i', 0x94];
        let text = decode(&bytes);
        assert_eq!(text, "Say \u{201C}hi\u{201D}");
        assert!(!text.contains('\u{FFFD}'), "nothing should be replaced");
    }

    #[test]
    fn latin1_accents_decode_to_the_right_letters() {
        // 0xE9 is e-acute in both Latin-1 and Windows-1252.
        assert_eq!(decode(&[b'c', b'a', b'f', 0xE9]), "caf\u{E9}");
    }

    #[test]
    fn valid_utf8_is_left_exactly_as_it_is() {
        let text = "sous-titres en fran\u{E7}ais \u{2014} d\u{E9}but";
        assert_eq!(decode(text.as_bytes()), text);
    }

    #[test]
    fn subtitle_files_are_recognised_by_extension() {
        assert!(is_subtitle("film.srt"));
        assert!(is_subtitle("FILM.SRT"));
        assert!(is_subtitle("film.vtt"));
        assert!(!is_subtitle("film.mkv"));
        assert!(!is_subtitle("srt"));
    }

    #[test]
    fn sidecars_are_matched_to_their_video() {
        assert!(belongs_to("film.mkv", "film.srt"));
        assert!(belongs_to("dir/film.mkv", "dir/film.srt"));
        // Language suffixes are the usual convention.
        assert!(belongs_to("film.mkv", "film.en.srt"));
        // And torrents habitually put them in their own directory.
        assert!(belongs_to(
            "Some.Film/film.mkv",
            "Some.Film/Subs/film.en.srt"
        ));
        // A different film's subtitles must not attach themselves.
        assert!(!belongs_to("film.mkv", "other.srt"));
        assert!(!belongs_to("film.mkv", "film.mkv"));
    }

    #[test]
    fn language_tags_are_read_from_the_filename_when_present() {
        assert_eq!(language_of("film.en.srt").as_deref(), Some("en"));
        assert_eq!(language_of("dir/film.fr.srt").as_deref(), Some("fr"));
        assert_eq!(language_of("film.srt"), None);
        // "forced" is not a language, and must not be offered as one.
        assert_eq!(language_of("film.forced.srt"), None);
    }

    #[test]
    fn a_file_with_no_cues_still_produces_a_valid_track() {
        // An empty track is legal WebVTT and beats a 500.
        let vtt = srt_to_vtt("");
        assert_eq!(vtt.trim(), "WEBVTT");
    }
}
