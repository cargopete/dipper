//! Reading a release name, and deciding which of two is better.
//!
//! Results used to be ordered by seeders alone, which answers "will this
//! download" and not "is this the one you want". They are different questions
//! and the second is the one somebody choosing a film is asking: a 1080p
//! BluRay with six seeders beats a camcorder recording with sixty, and no
//! amount of swarm health changes that.
//!
//! Release naming is not a standard and never was, but it is a convention
//! everybody follows: the title first, then the year, then the provenance, then
//! the group. That is enough to read a resolution, a source and a codec out of
//! it, and those three account for nearly all of the difference between two
//! copies of the same programme.
//!
//! Deliberately a pure function over a string. It is opinionated, and opinions
//! should be arguable, which means testable.

/// How the picture was obtained. The single biggest thing about a release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// A camcorder in a cinema, or a telesync. Unwatchable, and the one thing
    /// worth actively pushing to the bottom rather than merely ranking low.
    Cam,
    /// Recorded off the air.
    Broadcast,
    /// From a DVD.
    Dvd,
    /// Re-encoded from a streaming service.
    WebRip,
    /// Taken from a streaming service without re-encoding.
    WebDl,
    /// From a disc.
    BluRay,
    /// From a disc, untouched. The best there is and the largest by a distance.
    Remux,
    /// Nothing in the name said.
    Unknown,
}

/// What a release name says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Vertical resolution in lines, when the name says.
    pub height: Option<u32>,
    pub source: Source,
    /// True for HEVC, which is meaningfully smaller for the same picture and
    /// meaningfully harder for an old device to decode.
    pub hevc: bool,
    pub hdr: bool,
}

/// Read what a release name claims.
pub fn parse(name: &str) -> Release {
    // Separators vary; words are what matter.
    let lower = name.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let has = |needle: &str| words.contains(&needle);
    // Some markers are written joined to their neighbours ("web-dl", "h.265").
    let contains = |needle: &str| lower.contains(needle);

    let height = words.iter().find_map(|word| match *word {
        "2160p" | "4k" | "uhd" => Some(2160),
        "1440p" => Some(1440),
        "1080p" | "1080i" => Some(1080),
        "720p" => Some(720),
        "576p" | "576i" => Some(576),
        "480p" | "480i" => Some(480),
        "360p" => Some(360),
        _ => None,
    });

    // Ordered worst-first so the strongest claim wins: a name saying both
    // "bluray" and "cam" is a camcorder recording of a disc release, and the
    // camcorder is the part that decides what it looks like.
    let source = if has("cam") || has("camrip") || has("hdcam") || has("ts") || has("telesync") {
        Source::Cam
    } else if has("remux") {
        Source::Remux
    } else if has("bluray") || has("blueray") || has("bdrip") || has("brrip") || has("bdremux") {
        Source::BluRay
    } else if contains("web-dl") || has("webdl") || has("amzn") || has("nf") {
        Source::WebDl
    } else if has("webrip") || has("web") {
        Source::WebRip
    } else if has("hdtv") || has("pdtv") || has("dsr") {
        Source::Broadcast
    } else if has("dvdrip") || has("dvd") || has("dvdscr") {
        Source::Dvd
    } else {
        Source::Unknown
    };

    Release {
        height,
        source,
        hevc: has("x265") || has("h265") || has("hevc") || contains("h.265"),
        hdr: has("hdr") || has("hdr10") || has("dv") || has("dolby") && has("vision"),
    }
}

/// What a release is worth, before anything about its swarm.
///
/// The numbers are a judgement and are meant to be argued with. The shape of
/// the judgement: resolution matters most, provenance next, and a camcorder
/// recording is not a lower-quality option but a different thing altogether,
/// which is why it is pushed below everything rather than ranked among it.
pub fn quality(release: &Release) -> i32 {
    let resolution = match release.height {
        Some(height) if height >= 2160 => 42,
        Some(height) if height >= 1440 => 38,
        Some(height) if height >= 1080 => 35,
        Some(height) if height >= 720 => 26,
        Some(height) if height >= 576 => 16,
        Some(height) if height >= 480 => 12,
        Some(_) => 6,
        // Nothing said, which is most of the Archive and a good deal else.
        // Placed between 720 and 576: assuming the worst would bury a great
        // many perfectly good files whose uploader simply did not label them.
        None => 20,
    };

    let provenance = match release.source {
        Source::Remux => 16,
        Source::BluRay => 14,
        Source::WebDl => 11,
        Source::WebRip => 8,
        Source::Dvd => 6,
        Source::Broadcast => 5,
        Source::Unknown => 7,
        // Not a low score, a disqualification. A camcorder recording is worse
        // than not watching, and burying it is the whole reason a viewer would
        // rather this list were not sorted by popularity.
        Source::Cam => -60,
    };

    // Small adjustments, deliberately small: they change which of two similar
    // releases wins and never which tier does.
    let codec = if release.hevc { 2 } else { 0 };
    let range = if release.hdr { 2 } else { 0 };

    resolution + provenance + codec + range
}

/// How much a swarm of this size is worth.
///
/// The interesting part is the shape. Nothing is the difference between
/// watching something and not, so the first few seeders are worth a great deal
/// and the difference between fifty and five hundred is worth almost nothing:
/// both will saturate the line. A linear term would let a popular bad release
/// beat a good one with a healthy swarm, which is precisely the ordering this
/// was written to stop.
pub fn availability(seeders: Option<u64>) -> i32 {
    match seeders {
        // Nothing to download from, whatever it looks like.
        Some(0) => -50,
        Some(seeders) => {
            // Roughly logarithmic: 1 -> 10, 10 -> 23, 100 -> 36, 1000 -> 50.
            let scaled = (seeders as f64).log10() * 13.0 + 10.0;
            scaled.round() as i32
        }
        // Not reported, which is every archive.org item. They are served by a
        // webseed that is always up, so an unknown swarm here is closer to a
        // healthy one than to a dead one.
        None => 24,
    }
}

/// The score a result is ordered by.
pub fn rank(name: &str, seeders: Option<u64>) -> i32 {
    quality(&parse(name)) + availability(seeders)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_name_gives_up_its_resolution_and_source() {
        let release = parse("Some.Film.2019.1080p.BluRay.x264-GROUP");
        assert_eq!(release.height, Some(1080));
        assert_eq!(release.source, Source::BluRay);
        assert!(!release.hevc);
    }

    #[test]
    fn the_separators_do_not_matter() {
        for name in [
            "Some Film 2019 1080p BluRay x264",
            "Some.Film.2019.1080p.BluRay.x264",
            "Some_Film_2019_1080p_BluRay_x264",
            "Some-Film-2019-1080p-BluRay-x264",
        ] {
            let release = parse(name);
            assert_eq!(release.height, Some(1080), "{name}");
            assert_eq!(release.source, Source::BluRay, "{name}");
        }
    }

    #[test]
    fn web_dl_is_told_apart_from_a_webrip() {
        // One is the stream as it was served; the other has been through an
        // encoder again. They are not the same thing.
        assert_eq!(
            parse("Show.S01E01.1080p.WEB-DL.DD5.1").source,
            Source::WebDl
        );
        assert_eq!(
            parse("Show.S01E01.1080p.WEBRip.x264").source,
            Source::WebRip
        );
    }

    #[test]
    fn hevc_is_recognised_however_it_is_spelled() {
        assert!(parse("Film.2160p.x265").hevc);
        assert!(parse("Film.2160p.HEVC").hevc);
        assert!(parse("Film.2160p.h265").hevc);
        assert!(parse("Film.2160p.H.265").hevc);
        assert!(!parse("Film.1080p.x264").hevc);
    }

    #[test]
    fn a_camcorder_recording_of_a_disc_release_is_still_a_camcorder_recording() {
        // The classic misleading name: it says BluRay because it is a
        // camcorder recording *of* one.
        let release = parse("New.Film.2026.HDCAM.BluRay.x264");
        assert_eq!(release.source, Source::Cam);
        assert!(
            quality(&release) < 0,
            "a cam must go below everything, not merely near the bottom"
        );
    }

    #[test]
    fn a_good_release_with_a_small_swarm_beats_a_bad_one_with_a_large_one() {
        // The whole reason this exists. Sorting by popularity puts the wrong
        // thing first, and popularity is a poor proxy for what you want to
        // watch.
        let good = rank("Film.2019.1080p.BluRay.x264-GRP", Some(6));
        let bad = rank("Film.2019.480p.HDCAM.XviD", Some(600));
        assert!(good > bad, "good {good} should beat bad {bad}");
    }

    #[test]
    fn between_two_similar_releases_the_healthier_swarm_wins() {
        let busy = rank("Film.1080p.BluRay.x264", Some(400));
        let quiet = rank("Film.1080p.BluRay.x264", Some(3));
        assert!(busy > quiet);
    }

    #[test]
    fn nothing_seeding_it_loses_to_almost_anything() {
        // A magnet with no seeders is not a slow download, it is one that
        // never starts.
        let dead = rank("Film.2160p.Remux.HDR", Some(0));
        let alive = rank("Film.480p.DVDRip.XviD", Some(2));
        assert!(alive > dead, "dead {dead} should lose to alive {alive}");
    }

    #[test]
    fn an_unlabelled_file_is_not_assumed_to_be_awful() {
        // Most of the Archive is named after the film and nothing else, and
        // burying all of it would be a poor way to treat the one source whose
        // rights are actually clear.
        let plain = rank("A Trip to the Moon", None);
        let bad = rank("A.Trip.to.the.Moon.360p.HDTV.XviD", Some(30));
        assert!(plain > bad, "plain {plain} should beat {bad}");
    }

    #[test]
    fn resolution_outranks_provenance() {
        // A 1080p web release beats a 480p disc rip, which is the way round
        // most people actually want it.
        let web = rank("Film.1080p.WEB-DL", Some(10));
        let disc = rank("Film.480p.BluRay", Some(10));
        assert!(web > disc);
    }

    #[test]
    fn the_swarm_term_flattens_out_rather_than_running_away() {
        // Ten times the seeders must not be worth ten times the score, or a
        // popular bad release beats a good one again.
        let ten = availability(Some(10));
        let hundred = availability(Some(100));
        let thousand = availability(Some(1_000));
        assert!(hundred - ten < 20, "{ten} -> {hundred}");
        assert!(thousand - hundred < 20, "{hundred} -> {thousand}");
    }

    #[test]
    fn an_empty_name_does_not_panic_and_scores_as_unknown() {
        let release = parse("");
        assert_eq!(release.height, None);
        assert_eq!(release.source, Source::Unknown);
    }
}
