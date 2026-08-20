//! One search bar, several indexes.
//!
//! There were two indexes behind one dropdown, with a handler each, and adding
//! a third by copying one would have made three things that must agree about
//! what a search means. This is the seam instead: one result shape, one
//! endpoint, and sources that differ only in how they are asked.
//!
//! Two things fall out of having it that were not possible before.
//!
//! **Fanning out.** Several indexes can be asked at once and the answers merged,
//! which is the only way a Torznab setup with four indexers behind it is worth
//! having. One that is slow or down does not hold up the rest.
//!
//! **Deduplication.** Not optional once there is more than one index. The same
//! release comes back from every one of them, and four copies of the same
//! episode destroys exactly the pick-and-play behaviour the player depends on.
//! Keyed on the infohash, because that is the only identifier two indexes will
//! agree on: names differ by punctuation, sizes by a byte.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use balerion_ia::{AdvancedQuery, advanced};
use balerion_torznab as torznab;
use serde::{Deserialize, Serialize};

use crate::routes::ApiError;
use crate::state::AppState;

/// One index that can be asked a question.
#[derive(Debug, Clone, Serialize)]
pub struct Source {
    /// What to put in `?sources=`.
    pub key: String,
    pub label: String,
    /// What is on the other end, said plainly. A collection label is not a
    /// rights clearance and neither is an indexer's category.
    pub note: String,
    /// True when this one is on unless a viewer says otherwise.
    pub default: bool,
}

/// Everything this balerion can ask.
pub fn sources(state: &AppState) -> Vec<Source> {
    let mut sources = vec![Source {
        key: "ia".into(),
        label: "Internet Archive".into(),
        note: "Public domain and Creative Commons film, and whatever else the \
               public has uploaded. Only items with a derived torrent."
            .into(),
        default: true,
    }];

    sources.push(Source {
        key: "tpb".into(),
        label: "apibay".into(),
        note: crate::tpb::SOURCE_NOTE.into(),
        default: false,
    });

    if let Some(client) = &state.torznab {
        for indexer in client.indexers() {
            sources.push(Source {
                key: format!("torznab:{}", indexer.name),
                label: indexer.name.clone(),
                note: "Your own indexer. What it returns is whatever it is \
                       configured to index, and its categories are not a licence."
                    .into(),
                default: false,
            });
        }
    }
    sources
}

/// One result, whichever index produced it.
#[derive(Debug, Clone, Serialize)]
pub struct Found {
    /// Which index (or indexes) produced it.
    pub sources: Vec<String>,
    pub title: String,
    /// What to hand `/api/resolve`: an archive.org identifier or a magnet.
    pub open: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seeders: Option<u64>,
    /// Creator and year, or the release name. Whatever is worth showing under
    /// the title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FindParams {
    #[serde(default)]
    pub q: String,
    /// Comma-separated source keys. Empty means the defaults.
    #[serde(default)]
    pub sources: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
pub struct Results {
    pub hits: Vec<Found>,
    /// Which sources actually answered, so a page can say when one did not.
    pub answered: Vec<String>,
    pub failed: Vec<String>,
    /// How many results were the same thing arriving twice.
    pub duplicates: usize,
}

/// The sources a request asked for, or the defaults.
fn wanted(state: &AppState, params: &FindParams) -> Vec<Source> {
    let available = sources(state);
    let Some(asked) = params
        .sources
        .as_deref()
        .filter(|list| !list.trim().is_empty())
    else {
        return available
            .into_iter()
            .filter(|source| source.default)
            .collect();
    };
    let asked: Vec<&str> = asked.split(',').map(str::trim).collect();
    available
        .into_iter()
        .filter(|source| asked.contains(&source.key.as_str()))
        .collect()
}

pub async fn handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<FindParams>,
) -> Result<Json<Results>, ApiError> {
    let limit = params.limit.unwrap_or(30).clamp(1, 100);
    let chosen = wanted(&state, &params);
    if chosen.is_empty() {
        return Err(ApiError::bad_request("no such source"));
    }

    // Asked at once. One index being slow must not hold up the others, which
    // is the whole reason a fan-out is worth having.
    let asks = chosen.iter().map(|source| {
        let state = Arc::clone(&state);
        let terms = params.q.clone();
        let source = source.clone();
        async move {
            let outcome = ask(&state, &source, &terms, limit).await;
            (source.key.clone(), outcome)
        }
    });

    let mut answered = Vec::new();
    let mut failed = Vec::new();
    let mut all = Vec::new();
    for (key, outcome) in futures_util::future::join_all(asks).await {
        match outcome {
            Ok(hits) => {
                answered.push(key);
                all.extend(hits);
            }
            Err(err) => {
                tracing::warn!(source = key, %err, "source did not answer");
                failed.push(key);
            }
        }
    }

    let before = all.len();
    let mut hits = merge(all);
    let duplicates = before.saturating_sub(hits.len());

    /* Ordered by what a viewer actually wants rather than by popularity. Those
     * are different questions: a 1080p disc rip with six seeders beats a
     * camcorder recording with sixty, and sorting by seeders alone put the
     * second one first. See [`crate::release`] for the judgement, which is
     * deliberately a pure function so it can be argued with. */
    hits.sort_by_key(|hit| std::cmp::Reverse(crate::release::rank(&hit.title, hit.seeders)));
    hits.truncate(limit as usize);

    Ok(Json(Results {
        hits,
        answered,
        failed,
        duplicates,
    }))
}

/// Fold results that are the same torrent into one.
///
/// Keyed on the infohash and nothing else. Two indexes will not agree on a
/// title (punctuation, group tags, capitalisation) and may not agree on a size,
/// but a torrent is its infohash and that is the end of it. Anything without
/// one, which means every archive.org item, is left alone.
pub fn merge(hits: Vec<Found>) -> Vec<Found> {
    let mut by_hash: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Found> = Vec::new();

    for hit in hits {
        let Some(hash) = hit.info_hash.clone() else {
            out.push(hit);
            continue;
        };
        match by_hash.get(&hash) {
            Some(&at) => {
                let held = &mut out[at];
                // The union: the best seeder count anyone reported, and every
                // index that had it. A viewer choosing between two rows wants
                // the higher figure, and one index under-reporting should not
                // make a healthy swarm look dead.
                held.seeders = held.seeders.max(hit.seeders);
                for source in hit.sources {
                    if !held.sources.contains(&source) {
                        held.sources.push(source);
                    }
                }
                held.size = held.size.or(hit.size);
            }
            None => {
                by_hash.insert(hash, out.len());
                out.push(hit);
            }
        }
    }
    out
}

/// Ask one source.
async fn ask(
    state: &Arc<AppState>,
    source: &Source,
    terms: &str,
    limit: u32,
) -> anyhow::Result<Vec<Found>> {
    if let Some(name) = source.key.strip_prefix("torznab:") {
        return ask_torznab(state, name, terms, limit).await;
    }
    match source.key.as_str() {
        "ia" => ask_archive(state, terms, limit).await,
        "tpb" => ask_apibay(state, terms, limit).await,
        other => anyhow::bail!("no such source {other}"),
    }
}

async fn ask_archive(state: &Arc<AppState>, terms: &str, limit: u32) -> anyhow::Result<Vec<Found>> {
    let query = AdvancedQuery::new(crate::search::build_query(terms, None))
        .fields([
            "identifier",
            "title",
            "creator",
            "year",
            "downloads",
            "item_size",
            "format",
        ])
        .sort(["downloads desc"])
        .rows(limit);

    let page = advanced::page(&state.ia, &query, 1).await?;
    Ok(page
        .hits
        .iter()
        .filter(|hit| advanced::has_torrent(hit))
        .map(|hit| Found {
            sources: vec!["ia".into()],
            title: hit.title().unwrap_or(&hit.identifier).trim().to_string(),
            // An identifier rather than a magnet, because archive.org's
            // trackers refuse third-party seeding and a magnet for one of its
            // items never resolves.
            open: hit.identifier.clone(),
            info_hash: None,
            size: hit.item_size(),
            seeders: None,
            detail: hit.fields.get_str("creator").map(str::to_string),
        })
        .collect())
}

async fn ask_apibay(state: &Arc<AppState>, terms: &str, limit: u32) -> anyhow::Result<Vec<Found>> {
    if terms.trim().is_empty() {
        // apibay has no browse: an empty query gets its no-results sentinel,
        // which would surface as "nothing matches that".
        return Ok(Vec::new());
    }
    let category = balerion_tpb::category::find("video")
        .map(|category| category.code)
        .unwrap_or(0);
    let found = balerion_tpb::search::search(&state.tpb, terms.trim(), category).await?;
    let page = crate::tpb::shortlist(found, None, limit as usize);

    Ok(page
        .hits
        .into_iter()
        .map(|hit| Found {
            sources: vec!["tpb".into()],
            title: hit.name.clone(),
            open: hit.magnet.clone(),
            info_hash: Some(hit.info_hash.to_ascii_lowercase()),
            size: Some(hit.size_bytes),
            seeders: Some(hit.seeders),
            detail: Some(hit.category_label.to_string()),
        })
        .collect())
}

async fn ask_torznab(
    state: &Arc<AppState>,
    name: &str,
    terms: &str,
    limit: u32,
) -> anyhow::Result<Vec<Found>> {
    let client = state
        .torznab
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no Torznab indexer is configured"))?;
    let indexer = client
        .indexers()
        .iter()
        .find(|indexer| indexer.name == name)
        .ok_or_else(|| anyhow::anyhow!("no indexer called {name}"))?;

    let query = torznab::Query::new(terms).limit(limit);
    let answer = client.ask(indexer, &query).await?;
    if answer.without_magnet > 0 {
        tracing::debug!(
            indexer = name,
            dropped = answer.without_magnet,
            "results offering only a .torrent URL, which balerion cannot take"
        );
    }

    Ok(answer
        .hits
        .into_iter()
        .map(|hit| Found {
            sources: vec![format!("torznab:{name}")],
            title: hit.title,
            open: hit.magnet,
            info_hash: (!hit.info_hash.is_empty()).then_some(hit.info_hash),
            size: (hit.size_bytes > 0).then_some(hit.size_bytes),
            seeders: Some(hit.seeders),
            detail: None,
        })
        .collect())
}

/// The sources this balerion can ask, for the page's menu.
pub async fn list(State(state): State<Arc<AppState>>) -> Json<Vec<Source>> {
    Json(sources(&state))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(source: &str, hash: Option<&str>, seeders: Option<u64>) -> Found {
        Found {
            sources: vec![source.to_string()],
            title: format!("A release from {source}"),
            open: "magnet:?xt=urn:btih:x".into(),
            info_hash: hash.map(str::to_string),
            size: Some(1024),
            seeders,
            detail: None,
        }
    }

    #[test]
    fn the_same_torrent_from_two_indexes_becomes_one_row() {
        // The whole reason deduplication is not optional: four copies of the
        // same episode is exactly what breaks picking one and playing it.
        let merged = merge(vec![
            found("tpb", Some("aa"), Some(10)),
            found("torznab:mine", Some("aa"), Some(4)),
        ]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].sources, vec!["tpb", "torznab:mine"]);
    }

    #[test]
    fn the_best_seeder_count_anybody_reported_is_the_one_kept() {
        // One index under-reporting must not make a healthy swarm look dead.
        let merged = merge(vec![
            found("a", Some("aa"), Some(3)),
            found("b", Some("aa"), Some(97)),
        ]);
        assert_eq!(merged[0].seeders, Some(97));
    }

    #[test]
    fn different_torrents_are_left_alone() {
        let merged = merge(vec![
            found("a", Some("aa"), Some(1)),
            found("b", Some("bb"), Some(1)),
        ]);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn results_with_no_infohash_are_never_folded_together() {
        // Every archive.org item looks like this, and they are not the same
        // thing merely because none of them has a hash.
        let merged = merge(vec![
            found("ia", None, None),
            found("ia", None, None),
            found("ia", None, None),
        ]);
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn merging_keeps_the_order_the_first_copy_arrived_in() {
        let merged = merge(vec![
            found("a", Some("aa"), Some(1)),
            found("b", Some("bb"), Some(1)),
            found("c", Some("aa"), Some(1)),
        ]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].info_hash.as_deref(), Some("aa"));
        assert_eq!(merged[1].info_hash.as_deref(), Some("bb"));
    }

    #[test]
    fn a_size_from_whichever_index_knew_it_survives() {
        let mut without = found("a", Some("aa"), Some(1));
        without.size = None;
        let merged = merge(vec![without, found("b", Some("aa"), Some(1))]);
        assert_eq!(merged[0].size, Some(1024));
    }

    #[test]
    fn merging_nothing_is_not_a_panic() {
        assert!(merge(Vec::new()).is_empty());
    }
}
