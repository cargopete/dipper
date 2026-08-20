//! Subtitles that were not in the torrent.
//!
//! The player has always been able to show a `.srt` sitting beside the video and
//! a track baked into the container. When a release carries neither, which is
//! most of what apibay serves and a good deal of the Archive, there was nothing
//! to show and nothing to be done about it.
//!
//! This goes and asks. Two ways, and they are worth very different amounts:
//!
//! **By file hash.** OpenSubtitles identify a file by its size plus its first
//! and last 64 KiB, which is a scheme that suits balerion unusually well: the
//! head is the first thing the picker fetches because the player asks for it,
//! and the tail is kept warm anyway for the index box that plenty of MP4s put
//! at the end. So identifying a two gigabyte film costs 128 kilobytes we
//! probably already have. A hash match is subtitles timed against *this exact
//! release*, which is the version of this feature that simply works.
//!
//! **By title.** What is left when the hash finds nothing. These were timed
//! against some other release, so they are checked against the audio by
//! [`crate::subsync`] before anyone sees them, and moved if they need it.
//!
//! The search is free and the download is not: the free allowance is five to
//! ten files a day. So the search happens when a file is opened and the
//! download only when a viewer actually turns the track on, and anything
//! fetched is written beside the video and never fetched twice.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use balerion_bt::InfoHash;
use balerion_osdb::{Match, Query, hash as osdb_hash, search};

use crate::state::{AppState, Torrent};

/// Where a fetched track is kept, inside the torrent's own directory.
///
/// Beside the video rather than in a shared cache, so deleting a torrent takes
/// its subtitles with it and the daily allowance is never spent twice on the
/// same file.
pub fn cache_path(root: &Path, file: usize) -> PathBuf {
    root.join(format!(".balerion-subtitles-{file}.vtt"))
}

/// What we found, without having spent anything to fetch it.
#[derive(Debug, Clone)]
pub struct Offer {
    pub best: Match,
    /// How it was found, for the label. A viewer deserves to know whether these
    /// were timed against what they are watching.
    pub exact: bool,
}

/// Look for subtitles for a file, without downloading one, remembering the
/// answer.
///
/// Returns `None` when there is no client configured, when nothing matched, or
/// when the search failed. All three mean "no track to offer", and the
/// difference between them belongs in the log rather than in the interface.
///
/// Remembered because a player asks about a file more than once, and every
/// repeat would be another request against somebody else's rate limit for an
/// answer that has not changed.
pub async fn look(state: &Arc<AppState>, info_hash: &InfoHash, file: usize) -> Option<Offer> {
    if let Some(remembered) = state.cached_offer(info_hash, file) {
        return remembered;
    }
    let found = search_for(state, info_hash, file).await;
    state.remember_offer(info_hash, file, found.clone());
    found
}

async fn search_for(state: &Arc<AppState>, info_hash: &InfoHash, file: usize) -> Option<Offer> {
    let client = state.osdb.as_ref()?;
    let torrent = state.get(info_hash)?;
    let entry = torrent.meta.files.get(file)?;

    // The hash first, because a match on it needs no correcting afterwards.
    if let Some(moviehash) = file_hash(&torrent, file).await {
        match search::search(client, &Query::for_hash(&moviehash)).await {
            Ok(found) => {
                if let Some(best) = found.into_iter().find(|found| found.exact) {
                    tracing::info!(moviehash, release = ?best.release, "found subtitles for this exact file");
                    return Some(Offer { best, exact: true });
                }
            }
            Err(err) => tracing::debug!(%err, "hash search failed"),
        }
    }

    // Otherwise by name, which means whatever the release was called.
    let title = title_of(&entry.path, &torrent.meta.name);
    let mut query = Query::for_title(&title);
    if let Some((season, episode)) = crate::media::episode_of(&entry.path) {
        query = query.episode(season, episode);
    }

    match search::search(client, &query).await {
        Ok(found) => found.into_iter().next().map(|best| {
            tracing::info!(title, release = ?best.release, "found subtitles by title");
            Offer { best, exact: false }
        }),
        Err(err) => {
            tracing::debug!(%err, title, "title search failed");
            None
        }
    }
}

/// Is there already a track on disk for this file?
///
/// Either fetched earlier or transcribed, and in both cases it is offered
/// without asking anybody anything.
pub async fn is_cached(state: &Arc<AppState>, info_hash: &InfoHash, file: usize) -> bool {
    let Some(torrent) = state.get(info_hash) else {
        return false;
    };
    tokio::fs::metadata(cache_path(&torrent.root, file))
        .await
        .is_ok()
}

/// Start transcribing a file in the background, if nothing else can help.
///
/// Returns true when a job is now running, whether this call started it or
/// found one already going. The result lands in the same cache file a fetched
/// track uses, so the next time the player asks about this file the track is
/// simply there.
pub fn transcribe(state: &Arc<AppState>, info_hash: &InfoHash, file: usize) -> bool {
    if state.is_transcribing(info_hash, file) {
        return true;
    }
    let (Some(whisper), Some(tools)) = (state.whisper.clone(), state.tools.clone()) else {
        return false;
    };
    if !state.begin_transcribing(info_hash, file) {
        return true;
    }

    let state = Arc::clone(state);
    let info_hash = *info_hash;
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let url = state.stream_url(&info_hash.to_hex(), file);
        // Translate rather than transcribe: the request was for English
        // subtitles, and for a film that is not in English a transcript in its
        // own language is not what anybody asked for. whisper leaves English
        // audio alone.
        let outcome = whisper.transcribe(&tools, &url, true).await;
        state.finished_transcribing(&info_hash, file);

        match outcome {
            Ok(cues) => {
                let vtt = crate::subtitles::cues_to_vtt_noted(
                    &cues,
                    Some(
                        "balerion transcribed these from the audio; expect the punctuation of a machine",
                    ),
                );
                let Some(torrent) = state.get(&info_hash) else {
                    return;
                };
                if let Err(err) = tokio::fs::write(cache_path(&torrent.root, file), vtt).await {
                    tracing::warn!(%err, "could not keep the transcription");
                    return;
                }
                tracing::info!(
                    file,
                    cues = cues.len(),
                    seconds = started.elapsed().as_secs(),
                    "transcribed a file that had no subtitles"
                );
            }
            Err(err) => tracing::warn!(%err, file, "could not transcribe this"),
        }
    });
    true
}

/// Download, align if it needs it, and hand back WebVTT.
///
/// **This is the call that spends the daily allowance.** Anything already in
/// the cache is served from there without touching the network.
pub async fn fetch(
    state: &Arc<AppState>,
    info_hash: &InfoHash,
    file: usize,
) -> anyhow::Result<String> {
    let torrent = state
        .get(info_hash)
        .ok_or_else(|| anyhow::anyhow!("no such torrent"))?;
    let cached = cache_path(&torrent.root, file);
    if let Ok(text) = tokio::fs::read_to_string(&cached).await {
        return Ok(text);
    }

    let client = state
        .osdb
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no OpenSubtitles API key is configured"))?;
    let offer = look(state, info_hash, file)
        .await
        .ok_or_else(|| anyhow::anyhow!("nobody has subtitles for this"))?;

    let raw = search::download(client, offer.best.file_id).await?;
    let text = crate::subtitles::decode(&raw);
    let cues = crate::subtitles::parse_cues(&text);
    if cues.is_empty() {
        anyhow::bail!("the subtitle file that came back has no cues in it");
    }

    // A hash match was timed against this exact file, so moving it could only
    // make it worse. Anything else is checked against the speech.
    let vtt = if offer.exact {
        crate::subtitles::cues_to_vtt_noted(
            &cues,
            Some("balerion: matched to this exact file, timings untouched"),
        )
    } else {
        align_fetched(state, info_hash, file, &cues).await
    };

    // Written beside the video, so this costs the allowance once ever.
    if let Err(err) = tokio::fs::write(&cached, &vtt).await {
        tracing::warn!(%err, "could not keep the fetched subtitles");
    }
    Ok(vtt)
}

/// Put a title-matched track in step with the audio, if we can tell.
async fn align_fetched(
    state: &Arc<AppState>,
    info_hash: &InfoHash,
    file: usize,
    cues: &[crate::subtitles::Cue],
) -> String {
    let Some(tools) = state.tools.clone() else {
        return crate::subtitles::cues_to_vtt(cues);
    };
    let Ok(probe) = state.probe(&tools, info_hash, file).await else {
        return crate::subtitles::cues_to_vtt(cues);
    };
    let url = state.stream_url(&info_hash.to_hex(), file);

    match crate::subsync::align_to_audio(&tools, &url, cues, probe.duration).await {
        Ok(alignment) if alignment.is_worth_applying() => {
            let moved = crate::subsync::apply(cues, alignment);
            crate::subtitles::cues_to_vtt_noted(
                &moved,
                Some(&format!(
                    "balerion moved these by {}ms at {:.4}x (confidence {:.2})",
                    alignment.offset_ms, alignment.scale, alignment.confidence
                )),
            )
        }
        Ok(alignment) if alignment.is_trustworthy() => crate::subtitles::cues_to_vtt_noted(
            cues,
            Some("balerion: already in step with the speech"),
        ),
        Ok(_) => crate::subtitles::cues_to_vtt_noted(
            cues,
            Some("balerion: could not tell whether these are in step; shown as written"),
        ),
        Err(err) => {
            tracing::debug!(%err, "could not align the fetched subtitles");
            crate::subtitles::cues_to_vtt(cues)
        }
    }
}

/// The OpenSubtitles hash of one file in a torrent, if its ends are on disk.
///
/// Deliberately gives up rather than waiting. The head and tail are usually
/// here already, and blocking a file listing on a swarm delivering the last
/// piece of a film would be a poor trade for a subtitle track.
async fn file_hash(torrent: &Arc<Torrent>, file: usize) -> Option<String> {
    let entry = torrent.meta.files.get(file)?;
    let (head, tail) = osdb_hash::ranges(entry.length)?;

    for span in [&head, &tail] {
        let pieces = torrent
            .meta
            .pieces_for_span(entry.offset + span.start, span.end - span.start);
        for piece in pieces {
            if !torrent.handle.has_piece(piece) {
                tracing::debug!(
                    file = entry.path,
                    piece,
                    "cannot identify this file yet; its ends are still arriving"
                );
                return None;
            }
        }
    }

    let head_bytes = torrent
        .handle
        .read_range(entry.offset + head.start, head.end - head.start)
        .await
        .ok()?;
    let tail_bytes = torrent
        .handle
        .read_range(entry.offset + tail.start, tail.end - tail.start)
        .await
        .ok()?;
    osdb_hash::compute(entry.length, &head_bytes, &tail_bytes)
}

/// A searchable title, from the filename or failing that the torrent name.
///
/// Release names are a wall of resolution, source, codec and group, none of
/// which OpenSubtitles wants. Everything from the first of those markers
/// onwards is dropped, which is crude and works: release naming puts the title
/// first and the technical details after it, always.
pub fn title_of(path: &str, fallback: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.rsplit_once('.').map(|(head, _)| head).unwrap_or(name);
    let cleaned = clean(stem);
    if cleaned.is_empty() {
        clean(fallback)
    } else {
        cleaned
    }
}

/// Markers that mean the title has ended and the provenance has begun.
const NOISE: &[&str] = &[
    "1080p", "2160p", "720p", "480p", "4k", "uhd", "bluray", "blu-ray", "brrip", "bdrip", "dvdrip",
    "webrip", "web-dl", "webdl", "hdtv", "hdrip", "x264", "x265", "h264", "h265", "hevc", "xvid",
    "divx", "aac", "ac3", "dts", "dd5", "remux", "proper", "repack", "extended", "internal",
];

fn clean(name: &str) -> String {
    let words: Vec<String> = name
        .split(['.', '_', ' ', '-'])
        .map(|word| word.trim().to_string())
        .filter(|word| !word.is_empty())
        .collect();

    let mut title: Vec<String> = Vec::new();
    for word in words {
        let lower = word.to_ascii_lowercase();
        // A four-digit year is where the title ends just as reliably as a
        // codec is, and rather more often.
        let is_year = lower.len() == 4
            && lower.chars().all(|c| c.is_ascii_digit())
            && (1880..=2100).contains(&lower.parse::<u32>().unwrap_or(0));
        if NOISE.contains(&lower.as_str()) || is_year || crate::media::episode_of(&word).is_some() {
            break;
        }
        title.push(word);
    }
    title.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_name_is_cut_down_to_a_title() {
        assert_eq!(
            title_of("Some.Film.2019.1080p.BluRay.x264-GROUP.mkv", "fallback"),
            "Some Film"
        );
        assert_eq!(
            title_of("The.Computer.Chronicles.S03E07.WEBRip.mp4", "fallback"),
            "The Computer Chronicles"
        );
    }

    #[test]
    fn a_plain_name_survives_intact() {
        assert_eq!(title_of("Nosferatu.mp4", "x"), "Nosferatu");
        assert_eq!(
            title_of("dir/A Trip to the Moon.mkv", "x"),
            "A Trip to the Moon"
        );
    }

    #[test]
    fn a_name_that_is_all_provenance_falls_back_to_the_torrent() {
        // Cutting at the first marker can leave nothing, and searching for an
        // empty string returns the whole catalogue.
        assert_eq!(
            title_of("1080p.x264.mkv", "Some Torrent Name"),
            "Some Torrent Name"
        );
    }

    #[test]
    fn the_cache_lives_inside_the_torrents_own_directory() {
        // So that deleting a torrent takes its subtitles with it, rather than
        // leaving them in a shared cache nothing ever tidies.
        let path = cache_path(Path::new("/data/abc"), 3);
        assert!(path.starts_with("/data/abc"));
        assert!(path.to_string_lossy().ends_with("-3.vtt"), "{path:?}");
    }

    #[test]
    fn two_files_in_one_torrent_do_not_share_a_cache_entry() {
        assert_ne!(
            cache_path(Path::new("/x"), 0),
            cache_path(Path::new("/x"), 1)
        );
    }
}
