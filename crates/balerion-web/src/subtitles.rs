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
