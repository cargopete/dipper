//! Finding something to watch.
//!
//! A video-shaped search over archive.org: moving images only, and only items
//! for which a `.torrent` has been derived, since anything else is not
//! something dipper can fetch.
//!
//! Uses the page-based advanced endpoint rather than the scrape API, whose
//! cursor pagination does not work anonymously (see
//! `docs/archive-org-notes.md`).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use dipper_ia::{AdvancedQuery, SearchHit, advanced};
use serde::{Deserialize, Serialize};

use crate::routes::ApiError;
use crate::state::AppState;

/// archive.org marks derived torrents with this format string.
const TORRENT_FILTER: &str = "format:\"Archive BitTorrent\"";

/// A collection offered in the interface.
pub struct Shelf {
    pub key: &'static str,
    pub label: &'static str,
    /// What the collection actually is, shown to the user.
    pub note: &'static str,
    /// The archive.org collection identifier, or none for everything.
    pub collection: Option<&'static str>,
}

/// Curated shelves, most reliably free first.
///
/// archive.org is user-uploaded, and a collection label is not a rights
/// clearance. Prelinger is the safe end: Rick Prelinger's ephemeral film
/// archive is deliberately public domain. `moviesandfilms` is the whole
/// moving image library, where the rights status of any given upload is
/// whatever the uploader claimed, so it is offered last and labelled as such.
pub const SHELVES: &[Shelf] = &[
    Shelf {
        key: "prelinger",
        label: "Prelinger Archives",
        note: "Ephemeral, industrial and educational film. Deliberately public domain.",
        collection: Some("prelinger"),
    },
    Shelf {
        key: "classic_cartoons",
        label: "Classic cartoons",
        note: "Animation old enough that its copyright has lapsed.",
        collection: Some("classic_cartoons"),
    },
    Shelf {
        key: "film_noir",
        label: "Film noir",
        note: "Noir features whose copyright was not renewed.",
        collection: Some("film_noir"),
    },
    Shelf {
        key: "sci-fi_horror",
        label: "Science fiction and horror",
        note: "B-movies, mostly out of copyright.",
        collection: Some("sci-fi_horror"),
    },
    Shelf {
        key: "computerchronicles",
        label: "The Computer Chronicles",
        note: "The television series, released under a Creative Commons licence.",
        collection: Some("computerchronicles"),
    },
    Shelf {
        key: "all",
        label: "Everything",
        note: "The whole moving image library. Uploaded by the public, so the \
               rights status of any given item is whatever its uploader claimed. \
               Worth checking before you assume anything here is free to use.",
        collection: None,
    },
];

fn shelf(key: &str) -> Option<&'static Shelf> {
    SHELVES.iter().find(|shelf| shelf.key == key)
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub shelf: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub identifier: String,
    pub title: String,
    pub creator: Option<String>,
    pub year: Option<String>,
    pub size: Option<u64>,
    pub downloads: Option<u64>,
    pub details_url: String,
}

#[derive(Debug, Serialize)]
pub struct Results {
    pub hits: Vec<Hit>,
    pub total: u64,
    /// Repeated back so the page can show which shelf produced these.
    pub shelf: String,
    pub note: &'static str,
}

/// Build the Lucene query archive.org wants.
///
/// Kept separate from the request handler so it can be tested without the
/// network, which matters: a wrong filter here silently returns items dipper
/// cannot download rather than failing.
pub fn build_query(terms: &str, collection: Option<&str>) -> String {
    let mut parts = vec![
        "mediatype:(movies)".to_string(),
        TORRENT_FILTER.to_string(),
    ];
    if let Some(collection) = collection {
        parts.push(format!("collection:({collection})"));
    }

    let terms = terms.trim();
    if !terms.is_empty() {
        // Parenthesised so a multi word query cannot break out of the
        // conjunction and quietly widen the search to the whole archive.
        parts.push(format!("({terms})"));
    }
    parts.join(" AND ")
}

fn year_of(hit: &SearchHit) -> Option<String> {
    hit.fields
        .get_str("year")
        .or_else(|| hit.fields.get_str("publicdate"))
        .map(|value| value.chars().take(4).collect())
}

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Results>, ApiError> {
    let key = params.shelf.as_deref().unwrap_or("prelinger");
    let shelf = shelf(key).unwrap_or(&SHELVES[0]);
    let limit = params.limit.unwrap_or(24).clamp(1, 100);

    let query = AdvancedQuery::new(build_query(&params.q, shelf.collection))
        .fields([
            "identifier",
            "title",
            "creator",
            "year",
            "publicdate",
            "downloads",
            "item_size",
            "format",
        ])
        // Popularity is a decent stand-in for relevance, and far better than
        // identifier order, which sorts by whatever the uploader typed.
        .sort(["downloads desc"])
        .rows(limit);

    let page = advanced::page(&state.ia, &query, 1)
        .await
        .map_err(|err| ApiError::bad_request(format!("archive.org search failed: {err}")))?;

    let hits = page
        .hits
        .iter()
        // Belt and braces: the format filter should have done this, but an
        // item without a torrent is one the player cannot open.
        .filter(|hit| advanced::has_torrent(hit))
        .map(|hit| Hit {
            identifier: hit.identifier.clone(),
            title: hit
                .title()
                .unwrap_or(&hit.identifier)
                .trim()
                .to_string(),
            creator: hit.fields.get_str("creator").map(str::to_string),
            year: year_of(hit),
            size: hit.item_size(),
            downloads: hit.downloads(),
            details_url: hit.details_url(),
        })
        .collect();

    Ok(Json(Results {
        hits,
        total: page.num_found,
        shelf: shelf.key.to_string(),
        note: shelf.note,
    }))
}

#[derive(Debug, Serialize)]
pub struct ShelfInfo {
    pub key: &'static str,
    pub label: &'static str,
    pub note: &'static str,
}

/// The shelves the page offers, so the markup does not have to duplicate them.
pub async fn shelves() -> Json<Vec<ShelfInfo>> {
    Json(
        SHELVES
            .iter()
            .map(|shelf| ShelfInfo {
                key: shelf.key,
                label: shelf.label,
                note: shelf.note,
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_query_is_restricted_to_downloadable_video() {
        // Without both of these the interface offers things it cannot play
        // and things it cannot fetch.
        let query = build_query("", None);
        assert!(query.contains("mediatype:(movies)"), "{query}");
        assert!(query.contains("Archive BitTorrent"), "{query}");
    }

    #[test]
    fn a_shelf_adds_its_collection() {
        let query = build_query("", Some("prelinger"));
        assert!(query.contains("collection:(prelinger)"), "{query}");
    }

    #[test]
    fn everything_shelf_adds_no_collection_filter() {
        assert!(!build_query("bread", None).contains("collection:"));
    }

    #[test]
    fn user_terms_are_parenthesised_so_they_cannot_widen_the_search() {
        // `a OR b` spliced in bare would apply the OR across the whole
        // conjunction and return the entire archive.
        let query = build_query("bread OR cake", Some("prelinger"));
        assert!(query.contains("(bread OR cake)"), "{query}");
        assert!(query.starts_with("mediatype:(movies)"), "{query}");
    }

    #[test]
    fn an_empty_query_is_a_browse_rather_than_an_error() {
        let query = build_query("   ", Some("film_noir"));
        assert!(!query.contains("()"), "no empty group: {query}");
        assert!(query.ends_with("collection:(film_noir)"), "{query}");
    }

    #[test]
    fn shelves_are_addressable_and_the_default_exists() {
        assert!(shelf("prelinger").is_some());
        assert!(shelf("all").is_some());
        assert!(shelf("not-a-shelf").is_none());
        // The page defaults to the first shelf, so it must be a safe one.
        assert_eq!(SHELVES[0].key, "prelinger");
        assert!(SHELVES[0].collection.is_some());
    }

    #[test]
    fn the_everything_shelf_says_what_it_is() {
        let all = shelf("all").unwrap();
        assert!(all.collection.is_none());
        assert!(
            all.note.contains("rights"),
            "the mixed shelf must say so: {}",
            all.note
        );
    }
}
