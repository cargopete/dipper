//! `balerion get <query>`: search, choose, download. The whole point of the
//! tool in one command, rather than making you copy an identifier between two.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use balerion_ia::{AdvancedQuery, IaClient, advanced};

use crate::download::{EngineOptions, download_command};
use crate::fmt;
use crate::torrent_source;

/// One thing the user could have meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub identifier: String,
    pub title: Option<String>,
    pub creator: Option<String>,
    pub mediatype: Option<String>,
    pub size: u64,
    pub downloads: u64,
}

impl Candidate {
    fn label(&self, width: usize) -> String {
        let title = self
            .title
            .as_deref()
            .map(fmt::one_line)
            .unwrap_or_else(|| self.identifier.clone());
        let mut label = fmt::truncate(&title, width);
        if let Some(creator) = self.creator.as_deref().filter(|c| !c.is_empty()) {
            label.push_str(&format!(
                "  ({})",
                fmt::truncate(&fmt::one_line(creator), 28)
            ));
        }
        label
    }
}

/// How the user told us which result they wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Take the top hit without asking.
    First,
    /// A 1-based position from the printed list.
    Position(usize),
    /// Ask.
    Prompt,
    /// Show the list and stop.
    ListOnly,
}

/// Pick a candidate without any I/O, so the rules are testable.
pub fn choose(candidates: &[Candidate], choice: Choice) -> Result<&Candidate> {
    if candidates.is_empty() {
        bail!("nothing matched");
    }
    match choice {
        Choice::First => Ok(&candidates[0]),
        Choice::Position(position) => candidates
            .get(position.checked_sub(1).unwrap_or(usize::MAX))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "there is no result {position}; the list has {}",
                    candidates.len()
                )
            }),
        Choice::Prompt | Choice::ListOnly => bail!("a choice is required"),
    }
}

pub fn print_candidates(candidates: &[Candidate]) {
    let width = candidates.len().to_string().len();
    for (index, candidate) in candidates.iter().enumerate() {
        println!(
            "{:>width$}. {:>10}  {:<9}  {}",
            index + 1,
            fmt::bytes(candidate.size),
            candidate.mediatype.as_deref().unwrap_or("-"),
            candidate.label(58),
            width = width,
        );
    }
}

/// Read a choice from the terminal. Returns `None` if the user backed out.
async fn ask(count: usize) -> Result<Option<usize>> {
    if !std::io::stdin().is_terminal() {
        bail!("not a terminal: pass --pick <n> or --first to choose non-interactively");
    }
    let prompt = format!("\nwhich one? [1-{count}, enter for 1, q to quit] ");
    let line = tokio::task::spawn_blocking(move || {
        use std::io::{BufRead, Write};
        print!("{prompt}");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).map(|_| line)
    })
    .await
    .context("reading your answer")??;

    let answer = line.trim();
    if answer.is_empty() {
        return Ok(Some(1));
    }
    if answer.eq_ignore_ascii_case("q") || answer.eq_ignore_ascii_case("quit") {
        return Ok(None);
    }
    match answer.parse::<usize>() {
        Ok(position) if (1..=count).contains(&position) => Ok(Some(position)),
        _ => bail!("{answer:?} is not one of 1-{count}"),
    }
}

/// Search archive.org (or the local catalogue) for torrent-backed items.
pub async fn find(
    client: &IaClient,
    query: &str,
    limit: usize,
    local: Option<&balerion_index::Catalogue>,
) -> Result<Vec<Candidate>> {
    // Only torrent-backed items: without a derived torrent there is nothing
    // for the engine to fetch.
    if let Some(catalogue) = local {
        let hits = catalogue.search(&format!("({query}) AND has_torrent:true"), limit)?;
        if !hits.is_empty() {
            return Ok(hits
                .into_iter()
                .map(|hit| Candidate {
                    identifier: hit.record.identifier,
                    title: hit.record.title,
                    creator: hit.record.creator,
                    mediatype: hit.record.mediatype,
                    size: hit.record.item_size,
                    downloads: hit.record.downloads,
                })
                .collect());
        }
        tracing::debug!("nothing in the local catalogue; asking archive.org");
    }

    let search = AdvancedQuery::new(format!(
        "({query}) AND format:\"{}\"",
        balerion_ia::metadata::TORRENT_FORMAT
    ))
    .sort(["downloads desc"])
    .rows((limit as u32).clamp(1, 500));

    let hits = advanced::collect(client, &search, limit, |_, _| {})
        .await
        .context("searching archive.org")?;

    Ok(hits
        .into_iter()
        .map(|hit| Candidate {
            identifier: hit.identifier.clone(),
            title: hit.title().map(str::to_string),
            creator: hit.fields.creator().map(str::to_string),
            mediatype: hit.mediatype().map(str::to_string),
            size: hit.item_size().unwrap_or(0),
            downloads: hit.downloads().unwrap_or(0),
        })
        .collect())
}

/// The whole command: search, show, choose, download.
#[allow(clippy::too_many_arguments)]
pub async fn get_command(
    client: &IaClient,
    catalogue: Option<&balerion_index::Catalogue>,
    query: &str,
    limit: usize,
    choice: Choice,
    output: Option<PathBuf>,
    options: &EngineOptions,
) -> Result<()> {
    let candidates = find(client, query, limit, catalogue).await?;
    if candidates.is_empty() {
        bail!("nothing on archive.org matched {query:?} with a downloadable torrent");
    }

    if choice == Choice::ListOnly {
        print_candidates(&candidates);
        println!(
            "\n{} results. Re-run with --pick <n> to fetch one.",
            candidates.len()
        );
        return Ok(());
    }

    let chosen = match choice {
        Choice::Prompt => {
            print_candidates(&candidates);
            match ask(candidates.len()).await? {
                Some(position) => &candidates[position - 1],
                None => {
                    println!("nothing fetched");
                    return Ok(());
                }
            }
        }
        other => {
            // Still show the list, so a scripted --first is not a mystery.
            print_candidates(&candidates);
            println!();
            choose(&candidates, other)?
        }
    };

    println!("fetching {}", chosen.identifier);
    let source = torrent_source::resolve(&chosen.identifier, client).await?;
    download_command(&source, output, options).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<Candidate> {
        vec![
            Candidate {
                identifier: "first-item".into(),
                title: Some("The First Thing".into()),
                creator: Some("Someone".into()),
                mediatype: Some("audio".into()),
                size: 1024,
                downloads: 90,
            },
            Candidate {
                identifier: "second-item".into(),
                title: Some("The Second Thing".into()),
                creator: None,
                mediatype: Some("texts".into()),
                size: 2048,
                downloads: 10,
            },
        ]
    }

    #[test]
    fn first_takes_the_top_hit() {
        let list = candidates();
        assert_eq!(
            choose(&list, Choice::First).unwrap().identifier,
            "first-item"
        );
    }

    #[test]
    fn positions_are_one_based() {
        let list = candidates();
        assert_eq!(
            choose(&list, Choice::Position(2)).unwrap().identifier,
            "second-item"
        );
    }

    #[test]
    fn out_of_range_positions_say_how_many_there_were() {
        let list = candidates();
        let err = choose(&list, Choice::Position(7)).unwrap_err();
        assert!(format!("{err}").contains("the list has 2"), "{err}");
        // Zero is a common off-by-one from someone reading the list as 0-based.
        assert!(choose(&list, Choice::Position(0)).is_err());
    }

    #[test]
    fn an_empty_result_set_is_an_error_not_a_panic() {
        assert!(choose(&[], Choice::First).is_err());
        assert!(choose(&[], Choice::Position(1)).is_err());
    }

    #[test]
    fn labels_fold_whitespace_and_stay_within_width() {
        let candidate = Candidate {
            identifier: "x".into(),
            title: Some("a title\n  with awkward\twhitespace and rather a lot of length".into()),
            creator: Some("A Creator".into()),
            mediatype: None,
            size: 0,
            downloads: 0,
        };
        let label = candidate.label(20);
        assert!(!label.contains('\n') && !label.contains('\t'), "{label}");
        assert!(label.starts_with("a title with"), "{label}");
        assert!(label.contains("(A Creator)"), "{label}");
    }

    #[test]
    fn an_untitled_item_falls_back_to_its_identifier() {
        let candidate = Candidate {
            identifier: "some-identifier".into(),
            title: None,
            creator: None,
            mediatype: None,
            size: 0,
            downloads: 0,
        };
        assert_eq!(candidate.label(40), "some-identifier");
    }
}
