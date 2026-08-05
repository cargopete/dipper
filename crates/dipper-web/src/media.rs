//! What a browser will actually play.
//!
//! The honest answer is "it depends on the codec, and you cannot know without
//! parsing the file", so this deals only in containers. A container that no
//! browser opens is a definite no; a container browsers do open is a maybe,
//! and the page handles the disappointment when the codec turns out to be
//! exotic. Guessing optimistically and showing a clear error beats guessing
//! pessimistically and refusing to try.

/// How a file is likely to fare in a `<video>` or `<audio>` element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    /// Browsers open this container natively. Stream it.
    Native,
    /// A real media file, but not one browsers open. Needs a remux, which
    /// dipper does not do yet, so it is offered as a download instead.
    NeedsRemux,
    /// Not video or audio at all.
    NotMedia,
}

/// The kind of element the page should reach for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Media {
    pub content_type: &'static str,
    pub playback: Playback,
    pub kind: Kind,
    /// Why it will not play, when it will not. Shown to the user verbatim, so
    /// it says what to do rather than merely what went wrong.
    pub reason: Option<&'static str>,
}

const REMUX_REASON: &str =
    "this container is not one browsers can open, so it needs converting first. \
     You can still download it and play it in VLC.";

/// Classify by extension.
pub fn classify(path: &str) -> Media {
    let extension = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();

    match extension.as_str() {
        // Containers every current browser opens.
        "mp4" | "m4v" => native("video/mp4", Kind::Video),
        "webm" => native("video/webm", Kind::Video),
        "ogv" => native("video/ogg", Kind::Video),
        "mp3" => native("audio/mpeg", Kind::Audio),
        "m4a" => native("audio/mp4", Kind::Audio),
        "oga" | "ogg" => native("audio/ogg", Kind::Audio),
        "opus" => native("audio/opus", Kind::Audio),
        "flac" => native("audio/flac", Kind::Audio),
        "wav" => native("audio/wav", Kind::Audio),

        // Real video, wrong wrapper. Matroska is the painful one: it is what
        // most of the good encodes are in, and no browser will touch it.
        "mkv" => remux("video/x-matroska", Kind::Video),
        "avi" => remux("video/x-msvideo", Kind::Video),
        "mov" => remux("video/quicktime", Kind::Video),
        "wmv" => remux("video/x-ms-wmv", Kind::Video),
        "flv" => remux("video/x-flv", Kind::Video),
        "mpg" | "mpeg" => remux("video/mpeg", Kind::Video),
        "ts" | "m2ts" => remux("video/mp2t", Kind::Video),
        "3gp" => remux("video/3gpp", Kind::Video),
        "rmvb" | "rm" => remux("application/vnd.rn-realmedia", Kind::Video),
        "wma" => remux("audio/x-ms-wma", Kind::Audio),

        "srt" => other("application/x-subrip"),
        "vtt" => other("text/vtt"),
        "txt" | "nfo" => other("text/plain; charset=utf-8"),
        "jpg" | "jpeg" => other("image/jpeg"),
        "png" => other("image/png"),
        "pdf" => other("application/pdf"),
        _ => other("application/octet-stream"),
    }
}

fn native(content_type: &'static str, kind: Kind) -> Media {
    Media {
        content_type,
        playback: Playback::Native,
        kind,
        reason: None,
    }
}

fn remux(content_type: &'static str, kind: Kind) -> Media {
    Media {
        content_type,
        playback: Playback::NeedsRemux,
        kind,
        reason: Some(REMUX_REASON),
    }
}

fn other(content_type: &'static str) -> Media {
    Media {
        content_type,
        playback: Playback::NotMedia,
        kind: Kind::Other,
        reason: None,
    }
}

/// Pick the file a viewer most likely meant: the largest video, or failing
/// that the largest audio.
///
/// Size is a better signal than name. Torrents are full of samples, trailers
/// and "RARBG.txt", and the feature is reliably the biggest thing in there.
///
/// Containers needing conversion are ranked below native ones but still
/// offered, because dipper can convert them when ffmpeg is present. An item
/// whose only video is an MPEG program stream, which describes a good deal of
/// the Internet Archive, should still open on something.
pub fn best_to_play(files: &[dipper_bt::TorrentFile]) -> Option<usize> {
    let pick = |kind: Kind, playback: Playback| {
        files
            .iter()
            .enumerate()
            .filter(|(_, file)| {
                let media = classify(&file.path);
                media.playback == playback && media.kind == kind
            })
            .max_by_key(|(_, file)| file.length)
            .map(|(index, _)| index)
    };
    pick(Kind::Video, Playback::Native)
        .or_else(|| pick(Kind::Video, Playback::NeedsRemux))
        .or_else(|| pick(Kind::Audio, Playback::Native))
        .or_else(|| pick(Kind::Audio, Playback::NeedsRemux))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, length: u64) -> dipper_bt::TorrentFile {
        dipper_bt::TorrentFile {
            path: path.to_string(),
            length,
            offset: 0,
        }
    }

    #[test]
    fn browser_friendly_containers_are_streamable() {
        assert_eq!(classify("film.mp4").playback, Playback::Native);
        assert_eq!(classify("film.mp4").content_type, "video/mp4");
        assert_eq!(classify("film.webm").kind, Kind::Video);
        assert_eq!(classify("song.flac").kind, Kind::Audio);
    }

    #[test]
    fn matroska_is_refused_with_something_useful_to_say() {
        let mkv = classify("film.mkv");
        assert_eq!(mkv.playback, Playback::NeedsRemux);
        assert_eq!(mkv.kind, Kind::Video);
        assert!(mkv.reason.expect("a reason").contains("VLC"));
    }

    #[test]
    fn extensions_are_case_insensitive_and_path_aware() {
        assert_eq!(classify("Some.Film.2009/VIDEO.MP4").playback, Playback::Native);
        // A dot in a directory name must not be mistaken for the extension.
        assert_eq!(classify("v1.2/README").playback, Playback::NotMedia);
        assert_eq!(classify("noextension").playback, Playback::NotMedia);
    }

    #[test]
    fn the_biggest_playable_video_wins_over_samples_and_extras() {
        let files = vec![
            file("film/RARBG.txt", 30),
            file("film/sample.mp4", 20_000_000),
            file("film/poster.png", 500_000),
            file("film/feature.mp4", 700_000_000),
            file("film/extras.mkv", 900_000_000),
        ];
        // The mkv is bigger but unplayable; the sample is playable but small.
        assert_eq!(best_to_play(&files), Some(3));
    }

    #[test]
    fn audio_is_chosen_only_when_there_is_no_video() {
        let files = vec![file("album/track.mp3", 5), file("album/cover.png", 900)];
        assert_eq!(best_to_play(&files), Some(0));
    }

    #[test]
    fn a_convertible_container_is_offered_when_nothing_native_exists() {
        // This is the shape of a great many Internet Archive items: the only
        // video is an MPEG program stream. Offering nothing would mean the
        // player refused material it can perfectly well convert.
        let files = vec![file("film/notes.txt", 1), file("film/feature.mpeg", 700)];
        assert_eq!(best_to_play(&files), Some(1));
    }

    #[test]
    fn a_native_container_still_wins_over_a_convertible_one() {
        // Converting costs CPU and quality; if there is an MP4 sitting there,
        // it should be preferred even when it is smaller.
        let files = vec![file("film/feature.mkv", 900), file("film/feature.mp4", 300)];
        assert_eq!(best_to_play(&files), Some(1));
    }

    #[test]
    fn a_torrent_with_no_media_at_all_suggests_nothing() {
        let files = vec![file("stuff/readme.txt", 10), file("stuff/cover.png", 900)];
        assert_eq!(best_to_play(&files), None);
    }
}
