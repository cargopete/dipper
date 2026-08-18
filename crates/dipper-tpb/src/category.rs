//! apibay's category codes, and the handful dipper offers.
//!
//! The parent code broadens the search to its children: `cat=200` returns
//! everything in 2xx, which is how one request covers all of video. That
//! matters more for what it excludes. `cat=0` searches the lot, adult
//! categories included, and will hand back 5xx rows for an innocent query,
//! so dipper never sends it.

/// A category offered in the interface.
pub struct Category {
    pub key: &'static str,
    pub label: &'static str,
    /// What the category actually contains, shown to the user.
    pub note: &'static str,
    pub code: u32,
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
    },
    Category {
        key: "hd_movies",
        label: "HD movies",
        note: "1080p and thereabouts. The best trade between size and picture for streaming.",
        code: 207,
    },
    Category {
        key: "movies",
        label: "Movies",
        note: "Standard definition. Small, quick to start, and it shows.",
        code: 201,
    },
    Category {
        key: "hd_tv",
        label: "HD TV shows",
        note: "Episodes and season packs at 1080p. A pack is one torrent of many files.",
        code: 208,
    },
    Category {
        key: "tv",
        label: "TV shows",
        note: "Standard definition episodes and packs.",
        code: 205,
    },
    Category {
        key: "uhd_movies",
        label: "UHD / 4K movies",
        note: "Tens of gigabytes each. Fine to download, and a domestic line will \
               not usually stream one without stopping to think.",
        code: 211,
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
    fn an_unfiled_code_is_not_claimed_as_video() {
        assert!(!is_video(505));
        assert!(!is_video(102));
        assert_eq!(label(9999), "Unknown");
    }
}
