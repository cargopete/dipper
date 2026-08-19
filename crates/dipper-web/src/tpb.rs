//! Finding something to watch, the other way.
//!
//! The same job as [`crate::search`] against a different index: a query goes
//! to apibay, magnets come back. What arrives here is already a magnet, so the
//! result needs no resolving step of its own. It goes to `/api/resolve` like
//! anything a viewer might have pasted, and the engine below never learns
//! where it came from.
//!
//! Two filters apply on the way out, both for the same reason the archive
//! handler insists on a derived torrent: an entry the player cannot open is
//! worse than no entry at all, because the failure arrives thirty seconds
//! later and looks like a bug.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use dipper_tpb::{Hit, category, search};
use serde::{Deserialize, Serialize};

use crate::routes::ApiError;
use crate::state::AppState;

/// Below this, do not offer it.
///
/// A magnet with no seeders is not a slow download, it is a download that
/// never starts, and it is indistinguishable in the interface from one still
/// looking for peers. The DHT occasionally turns up someone the tracker did
/// not know about, which is why the survivors are counted and reported rather
/// than quietly dropped.
const MIN_SEEDERS: u64 = 1;

/// Said once, plainly, wherever this source appears.
///
/// The archive shelves carry notes because a collection label is not a rights
/// clearance. The same applies here with rather less ambiguity, so it is
/// stated in the interface rather than left in a comment nobody reads.
pub const SOURCE_NOTE: &str = "A public index of whatever strangers uploaded. Most of it is \
                               copyrighted, none of it is cleared, and the category is not a \
                               licence. Your connection, your jurisdiction, your problem.";

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    /// Drop anything a thin line could not stream. See
    /// [`dipper_tpb::category::Category::thin_cap`].
    #[serde(default)]
    pub thin: bool,
}

#[derive(Debug, Serialize)]
pub struct Results {
    pub hits: Vec<Hit>,
    /// How many usable rows the search actually produced, before the limit.
    pub total: usize,
    /// Dropped for having no seeders. Reported rather than hidden: "24 results,
    /// 31 unseeded" is a useful thing to know about a search, and silently
    /// showing 24 reads as though 24 was all there was.
    pub unseeded: usize,
    /// Dropped for being larger than a thin line could stream, when `thin` was
    /// asked for. Same reasoning as `unseeded`, and rather more important: a
    /// filter that quietly hides most of the swarm has to say so.
    pub oversize: usize,
    /// The cap that was applied, so the page can label the control with a real
    /// number instead of calling it "small".
    pub cap: Option<u64>,
    /// Repeated back so the page can show which category produced these.
    pub category: String,
    pub note: &'static str,
}

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Results>, ApiError> {
    let key = params.category.as_deref().unwrap_or("video");
    let category = category::find(key).unwrap_or(&category::CATEGORIES[0]);
    let limit = params.limit.unwrap_or(24).clamp(1, 100) as usize;

    // Unlike the archive, apibay has no browse: an empty query returns its
    // no-results sentinel, which would surface as "nothing matches that",
    // which is not what happened.
    let terms = params.q.trim();
    if terms.is_empty() {
        return Err(ApiError::bad_request("type something to search for"));
    }

    let found = search::search(&state.tpb, terms, category.code)
        .await
        .map_err(|err| ApiError::bad_request(format!("apibay search failed: {err}")))?;

    let usable: Vec<Hit> = found
        .into_iter()
        // Belt and braces: the category code should have done this, and a
        // parent code returns children, so a change at their end could widen
        // it to things dipper cannot play.
        .filter(|hit| category::is_video(hit.category))
        .collect();

    // Size before seeders, so the two counts do not overlap and each reports
    // only what it alone excluded.
    let cap = params.thin.then(|| category.thin_cap());
    let affordable: Vec<Hit> = match cap {
        Some(cap) => usable
            .iter()
            .filter(|hit| hit.size_bytes <= cap)
            .cloned()
            .collect(),
        None => usable.clone(),
    };
    let oversize = usable.len() - affordable.len();

    let mut seeded: Vec<Hit> = affordable
        .iter()
        .filter(|hit| hit.seeders >= MIN_SEEDERS)
        .cloned()
        .collect();
    let unseeded = affordable.len() - seeded.len();

    // The endpoint appears to sort by seeders already, and appears is not a
    // promise. Playback depends on this order being right, so sort it here.
    seeded.sort_by_key(|hit| std::cmp::Reverse(hit.seeders));
    let total = seeded.len();
    seeded.truncate(limit);

    Ok(Json(Results {
        hits: seeded,
        total,
        unseeded,
        oversize,
        cap,
        category: category.key.to_string(),
        note: category.note,
    }))
}

#[derive(Debug, Serialize)]
pub struct CategoryInfo {
    pub key: &'static str,
    pub label: &'static str,
    pub note: &'static str,
    /// What "fits a thin line" means for this category, so the control can be
    /// labelled with the number before anyone searches.
    pub thin_cap: u64,
}

#[derive(Debug, Serialize)]
pub struct Catalogue {
    pub categories: Vec<CategoryInfo>,
    pub note: &'static str,
}

/// The categories the page offers, so the markup does not duplicate them.
pub async fn categories() -> Json<Catalogue> {
    Json(Catalogue {
        categories: category::CATEGORIES
            .iter()
            .map(|category| CategoryInfo {
                key: category.key,
                label: category.label,
                note: category.note,
                thin_cap: category.thin_cap(),
            })
            .collect(),
        note: SOURCE_NOTE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(seeders: u64, category: u32) -> Hit {
        sized_hit(seeders, category, 1)
    }

    fn sized_hit(seeders: u64, category: u32, size_bytes: u64) -> Hit {
        Hit {
            id: 1,
            name: "Some.Film.1080p".to_string(),
            info_hash: "31A5EA99284B3603E94EF861311B6BB29345C6D2".to_string(),
            seeders,
            leechers: 0,
            num_files: 1,
            size_bytes,
            username: "someone".to_string(),
            status: "member".to_string(),
            added: 0,
            category,
            category_label: dipper_tpb::category::label(category),
            magnet: "magnet:?xt=urn:btih:31A5EA99284B3603E94EF861311B6BB29345C6D2".to_string(),
        }
    }

    /// The filtering and ordering, without the network. Mirrors the body of
    /// [`handler`]: if that changes, this should be made to change with it.
    fn shortlist(found: Vec<Hit>, limit: usize, cap: Option<u64>) -> (Vec<Hit>, usize, usize) {
        let usable: Vec<Hit> = found
            .into_iter()
            .filter(|hit| category::is_video(hit.category))
            .collect();
        let affordable: Vec<Hit> = match cap {
            Some(cap) => usable
                .iter()
                .filter(|hit| hit.size_bytes <= cap)
                .cloned()
                .collect(),
            None => usable.clone(),
        };
        let oversize = usable.len() - affordable.len();
        let mut seeded: Vec<Hit> = affordable
            .iter()
            .filter(|hit| hit.seeders >= MIN_SEEDERS)
            .cloned()
            .collect();
        let unseeded = affordable.len() - seeded.len();
        seeded.sort_by_key(|hit| std::cmp::Reverse(hit.seeders));
        seeded.truncate(limit);
        (seeded, unseeded, oversize)
    }

    #[test]
    fn a_dead_swarm_is_not_offered_but_is_counted() {
        let (hits, unseeded, _) = shortlist(vec![hit(0, 207), hit(5, 207), hit(0, 208)], 24, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(unseeded, 2, "the drop has to be visible somewhere");
    }

    #[test]
    fn anything_outside_video_is_dropped_and_not_counted_as_unseeded() {
        // A non-video row was never a candidate, so counting it among the
        // unseeded would misreport what the search found.
        let (hits, unseeded, _) = shortlist(vec![hit(9, 505), hit(9, 101), hit(1, 201)], 24, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(unseeded, 0);
    }

    #[test]
    fn the_healthiest_swarm_comes_first() {
        let (hits, ..) = shortlist(vec![hit(3, 207), hit(90, 207), hit(12, 207)], 24, None);
        let seeders: Vec<u64> = hits.iter().map(|hit| hit.seeders).collect();
        assert_eq!(seeders, vec![90, 12, 3]);
    }

    #[test]
    fn the_limit_applies_after_the_sort() {
        // Truncating first would leave the best result off the page.
        let (hits, ..) = shortlist(vec![hit(1, 207), hit(2, 207), hit(500, 207)], 2, None);
        assert_eq!(hits[0].seeders, 500);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn a_cap_drops_what_is_too_big_and_counts_it() {
        let cap = category::find("hd_movies").unwrap().thin_cap();
        let (hits, _, oversize) = shortlist(
            vec![
                sized_hit(9, 207, cap - 1),
                sized_hit(9, 207, cap + 1),
                sized_hit(9, 207, 40 << 30),
            ],
            24,
            Some(cap),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(oversize, 2);
    }

    #[test]
    fn a_result_exactly_on_the_cap_is_kept() {
        // The cap is what a thin line sustains, so equal to it is fine. An
        // off-by-one here silently loses the best result on the page.
        let cap = category::find("tv").unwrap().thin_cap();
        let (hits, ..) = shortlist(vec![sized_hit(9, 205, cap)], 24, Some(cap));
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn the_two_exclusions_do_not_double_count_each_other() {
        // One result that is both too big and unseeded must appear in exactly
        // one tally, or the numbers under the results add up to more than the
        // search returned.
        let cap = category::find("hd_movies").unwrap().thin_cap();
        let (hits, unseeded, oversize) = shortlist(vec![sized_hit(0, 207, cap + 1)], 24, Some(cap));
        assert!(hits.is_empty());
        assert_eq!(oversize, 1);
        assert_eq!(unseeded, 0, "already excluded by size");
    }

    #[test]
    fn without_the_cap_nothing_is_excluded_for_size() {
        let (hits, _, oversize) = shortlist(vec![sized_hit(9, 207, 40 << 30)], 24, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(oversize, 0);
    }

    #[test]
    fn the_source_note_says_what_it_is() {
        assert!(SOURCE_NOTE.contains("copyright"), "{SOURCE_NOTE}");
    }
}
