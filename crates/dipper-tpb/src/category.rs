//! apibay's category codes, and the handful dipper offers.
//!
//! The parent code broadens the search to its children: `cat=200` returns
//! everything in 2xx, which is how one request covers all of video. That
//! matters more for what it excludes. `cat=0` searches the lot, adult
//! categories included, and will hand back 5xx rows for an innocent query,
//! so dipper never sends it.

/// What a thin line sustains, in bits per second.
///
/// Poor ADSL, a rural line, a bad mobile signal. Below this you are not
/// streaming anything and the question does not arise.
pub const THIN_LINE_BPS: u64 = 1_500_000;

/// A category offered in the interface.
pub struct Category {
    pub key: &'static str,
    pub label: &'static str,
    /// What the category actually contains, shown to the user.
    pub note: &'static str,
    pub code: u32,
    /// How long one item in this category typically runs, in seconds.
    ///
    /// A guess, and unavoidably so: apibay reports a size and never a
    /// duration. It is only ever used to turn [`THIN_LINE_BPS`] into a size
    /// cap, so being out by a third moves the cap by a third and changes
    /// nothing else. Nothing derived from it is shown as though it were
    /// measured.
    pub typical_runtime_secs: u64,
}

impl Category {
    /// The largest item in this category a thin line could stream.
    ///
    /// Size is the right thing to filter on even though bitrate is what
    /// actually matters, because within one search every result is the same
    /// programme at a different bitrate. Two releases of the same episode have
    /// the same runtime, so their sizes rank exactly as their bitrates do.
    pub const fn thin_cap(&self) -> u64 {
        THIN_LINE_BPS * self.typical_runtime_secs / 8
    }
}

/// What the search box offers, broadest first.
///
/// Video only, and deliberately so: dipper is a player, and offering a
/// category whose contents it cannot open is the same mistake as offering an
/// archive.org item with no derived torrent.
pub const CATEGORIES: &[Category] = &[
    Category {
        key: "video",
        label: "All video",
        note: "Everything filed under video. One request covers the lot.",
        code: 200,
        // A mixed bag of films and episodes, so the middle of the two.
        typical_runtime_secs: 3600,
    },
    Category {
        key: "hd_movies",
        label: "HD movies",
        note: "1080p and thereabouts. The best trade between size and picture for streaming.",
        code: 207,
        // A feature, near enough.
        typical_runtime_secs: 6600,
    },
    Category {
        key: "movies",
        label: "Movies",
        note: "Standard definition. Small, quick to start, and it shows.",
        code: 201,
        // A feature, near enough.
        typical_runtime_secs: 6600,
    },
    Category {
        key: "hd_tv",
        label: "HD TV shows",
        note: "Episodes and season packs at 1080p. A pack is one torrent of many files.",
        code: 208,
        // An episode, or one of many in a season pack.
        typical_runtime_secs: 2700,
    },
    Category {
        key: "tv",
        label: "TV shows",
        note: "Standard definition episodes and packs.",
        code: 205,
        // An episode, or one of many in a season pack.
        typical_runtime_secs: 2700,
    },
    Category {
        key: "uhd_movies",
        label: "UHD / 4K movies",
        note: "Tens of gigabytes each. Fine to download, and a domestic line will \
               not usually stream one without stopping to think.",
        code: 211,
        // A feature, near enough.
        typical_runtime_secs: 6600,
    },
];

pub fn find(key: &str) -> Option<&'static Category> {
    CATEGORIES.iter().find(|category| category.key == key)
}

/// The label for any code the API might return, including ones not offered.
///
/// A result carries its own category, and a search of all video returns codes
/// no menu entry corresponds to, so this covers more ground than [`CATEGORIES`].
pub fn label(code: u32) -> &'static str {
    match code {
        101 => "Music",
        102 => "Audio books",
        104 => "FLAC",
        100..=199 => "Audio",
        201 => "Movies",
        202 => "Movies DVDR",
        203 => "Music videos",
        204 => "Movie clips",
        205 => "TV shows",
        206 => "Handheld",
        207 => "HD movies",
        208 => "HD TV shows",
        209 => "3D",
        210 => "Cam / telesync",
        211 => "UHD / 4K movies",
        212 => "UHD / 4K TV shows",
        200..=299 => "Video",
        300..=399 => "Applications",
        400..=499 => "Games",
        500..=599 => "Adult",
        601 => "E-books",
        602 => "Comics",
        600..=699 => "Other",
        _ => "Unknown",
    }
}

/// Whether a returned row is video at all.
///
/// Belt and braces against a category filter that did not hold: a row outside
/// 2xx is something dipper cannot play, and a 5xx row is something nobody
/// asked for.
pub fn is_video(code: u32) -> bool {
    (200..=299).contains(&code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offered_category_is_video() {
        for category in CATEGORIES {
            assert!(
                is_video(category.code),
                "{} is code {}, outside video",
                category.key,
                category.code
            );
        }
    }

    #[test]
    fn nothing_offered_searches_everything() {
        // cat=0 reaches the adult categories, whatever the query was.
        assert!(CATEGORIES.iter().all(|category| category.code != 0));
    }

    #[test]
    fn categories_are_addressable_and_the_default_exists() {
        assert!(find("video").is_some());
        assert!(find("hd_movies").is_some());
        assert!(find("not-a-category").is_none());
        // The page defaults to the first entry, so it must be the broad one.
        assert_eq!(CATEGORIES[0].code, 200);
    }

    #[test]
    fn labels_cover_the_codes_a_video_search_returns() {
        // Observed in a single search of cat=200: the menu has no entry for
        // most of these, and they still have to render as something.
        for code in [205, 207, 208, 210, 211, 212] {
            assert_ne!(label(code), "Unknown", "code {code}");
        }
    }

    #[test]
    fn the_thin_cap_is_a_plausible_size_for_each_category() {
        for category in CATEGORIES {
            let cap = category.thin_cap();
            // Nothing streams under 100 MiB and nothing thin streams over 2 GiB.
            assert!(
                (100 << 20..2 << 30).contains(&cap),
                "{} caps at {cap} bytes",
                category.key
            );
        }
    }

    #[test]
    fn an_episode_caps_lower_than_a_feature() {
        // The whole reason the cap is per category rather than one flat number:
        // 1.2 GiB is a thin two-hour film and a fat forty-minute episode.
        let episode = find("hd_tv").unwrap().thin_cap();
        let feature = find("hd_movies").unwrap().thin_cap();
        assert!(episode < feature, "{episode} vs {feature}");
    }

    #[test]
    fn the_cap_is_the_line_rate_times_the_runtime() {
        // Derived, not picked. If someone edits one of the two inputs the cap
        // should move with it rather than staying at a number nobody can explain.
        let tv = find("tv").unwrap();
        assert_eq!(tv.thin_cap(), THIN_LINE_BPS * tv.typical_runtime_secs / 8);
    }

    #[test]
    fn an_unfiled_code_is_not_claimed_as_video() {
        assert!(!is_video(505));
        assert!(!is_video(102));
        assert_eq!(label(9999), "Unknown");
    }
}
