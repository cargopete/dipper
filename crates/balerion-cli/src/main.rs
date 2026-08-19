//! balerion: a BitTorrent client that uses the Internet Archive as its search
//! backend. Phase 0 is the metadata and search layer, plus everything needed
//! to turn a search result into a torrent or a magnet link.

mod download;
mod fmt;
mod get;
mod torrent_source;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use balerion_ia::{
    AdvancedQuery, ClientConfig, IaClient, ItemMetadata, SearchHit, advanced, metadata, torrent,
};
use balerion_index::{Catalogue, Record};
use clap::{Args, Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};

use crate::download::EngineOptions;

#[derive(Debug, Parser)]
#[command(
    name = "balerion",
    version,
    about = "Find something to watch and fetch it over BitTorrent",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Where the local catalogue lives.
    #[arg(long, global = true, value_name = "DIR")]
    index_dir: Option<PathBuf>,

    /// Minimum milliseconds between archive.org requests.
    #[arg(long, global = true, default_value_t = 350, value_name = "MS")]
    min_interval: u64,

    /// Ask archive.org to serve us at reduced priority. Slower, but kinder.
    #[arg(long, global = true)]
    polite: bool,

    /// Log more. Repeat for more still.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Search the local catalogue, or archive.org directly with --remote.
    Search {
        /// Query terms. Lucene-ish syntax works: `mediatype:audio AND jazz`.
        #[arg(required = true)]
        query: Vec<String>,

        #[command(flatten)]
        filters: Filters,

        /// Skip the local catalogue and ask archive.org.
        #[arg(long)]
        remote: bool,

        /// Maximum results.
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,

        #[arg(long)]
        json: bool,
    },

    /// Fetch search results from archive.org into the local catalogue.
    Harvest {
        #[arg(required = true)]
        query: Vec<String>,

        #[command(flatten)]
        filters: Filters,

        /// How many items to harvest.
        #[arg(short = 'n', long, default_value_t = 1000)]
        limit: usize,

        /// Empty the catalogue first.
        #[arg(long)]
        reset: bool,
    },

    /// Show an item's metadata, files and torrent.
    Info {
        identifier: String,

        /// List every file, not just a summary.
        #[arg(short, long)]
        files: bool,

        #[arg(long)]
        json: bool,
    },

    /// Print an item's magnet link.
    Magnet { identifier: String },

    /// Download an item's `.torrent` file.
    Torrent {
        identifier: String,

        /// Where to write it. Defaults to `<identifier>_archive.torrent`.
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,

        /// Print the parsed torrent instead of writing it.
        #[arg(long)]
        show: bool,
    },

    /// Search archive.org and download what you pick, in one go.
    Get {
        /// What you are after.
        #[arg(required = true)]
        query: Vec<String>,

        /// How many results to offer.
        #[arg(short = 'n', long, default_value_t = 15)]
        limit: usize,

        /// Show the results and stop, without downloading anything.
        #[arg(short = 'l', long)]
        list: bool,

        /// Take the top result without asking.
        #[arg(long, conflicts_with = "list")]
        first: bool,

        /// Take result number N from the list.
        #[arg(long, value_name = "N", conflicts_with = "first")]
        pick: Option<usize>,

        /// Where to put the files. Defaults to the current directory.
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,

        /// Ignore the local catalogue and always ask archive.org.
        #[arg(long)]
        remote: bool,

        #[command(flatten)]
        engine: EngineArgs,
    },

    /// Resolve a magnet link (or .torrent, or archive.org identifier) to its
    /// file list, without downloading anything.
    Resolve {
        /// A magnet URI, a path to a .torrent, or an archive.org identifier.
        target: String,

        #[command(flatten)]
        engine: EngineArgs,
    },

    /// Download a magnet link, a .torrent, or an archive.org item.
    Download {
        /// A magnet URI, a path to a .torrent, or an archive.org identifier.
        target: String,

        /// Where to put the files. Defaults to the current directory.
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,

        #[command(flatten)]
        engine: EngineArgs,
    },

    /// Serve a local web player: paste a magnet, watch while it downloads.
    ///
    /// Engine flags are spelled out here rather than shared with the other
    /// commands, so that `--port` can mean the web server, which is the one a
    /// user of this command actually cares about.
    Serve {
        /// Port for the web player.
        #[arg(long, default_value_t = 8080)]
        port: u16,

        /// Address to bind. Loopback by default, because this endpoint will
        /// download whatever magnet it is handed.
        #[arg(long, default_value = "127.0.0.1")]
        host: std::net::IpAddr,

        /// Where torrents are stored. Defaults to the user cache directory.
        #[arg(short, long, value_name = "DIR")]
        output: Option<PathBuf>,

        /// Fewer peers and a shallower request pipeline, for a slow or flaky
        /// connection. Costs peak speed, shortens the queue in front of the
        /// piece the player is waiting on.
        #[arg(long)]
        low_bandwidth: bool,

        /// The port we tell peers and trackers we listen on.
        #[arg(long, default_value_t = 6881)]
        peer_port: u16,

        /// Do not use the DHT. Trackers and webseeds only.
        #[arg(long)]
        no_dht: bool,

        /// Ignore HTTP webseeds, even when the torrent has them.
        #[arg(long)]
        no_webseeds: bool,

        /// How many peers to talk to at once.
        #[arg(long, default_value_t = 30)]
        max_peers: usize,

        /// Seconds to spend looking for peers in the DHT.
        #[arg(long, default_value_t = 20)]
        dht_seconds: u64,
    },

    /// Inspect or clear the local catalogue.
    Index {
        #[command(subcommand)]
        action: IndexAction,
    },
}

#[derive(Debug, Args, Clone)]
struct EngineArgs {
    /// Do not use the DHT. Trackers and webseeds only.
    #[arg(long)]
    no_dht: bool,

    /// Ignore HTTP webseeds, even when the torrent has them.
    #[arg(long)]
    no_webseeds: bool,

    /// The port we tell peers and trackers we listen on.
    #[arg(long, default_value_t = 6881)]
    port: u16,

    /// How many peers to talk to at once.
    #[arg(long, default_value_t = 30)]
    max_peers: usize,

    /// Seconds to spend looking for peers in the DHT.
    #[arg(long, default_value_t = 20)]
    dht_seconds: u64,

    /// Re-hash everything already on disk instead of trusting the resume file.
    #[arg(long)]
    verify: bool,

    /// Suppress progress bars.
    #[arg(long)]
    quiet: bool,
}

impl From<&EngineArgs> for EngineOptions {
    fn from(args: &EngineArgs) -> Self {
        Self {
            no_dht: args.no_dht,
            no_webseeds: args.no_webseeds,
            port: args.port,
            max_peers: args.max_peers,
            dht_budget: Duration::from_secs(args.dht_seconds),
            verify: args.verify,
            quiet: args.quiet,
            ..Default::default()
        }
    }
}

#[derive(Debug, Subcommand)]
enum IndexAction {
    /// How many items are indexed, and where.
    Stats,
    /// Throw the catalogue away.
    Clear,
}

#[derive(Debug, Args, Clone)]
struct Filters {
    /// Only items archive.org has derived a .torrent for.
    #[arg(long)]
    torrents_only: bool,

    /// texts, audio, movies, software, image, data, web.
    #[arg(long, value_name = "TYPE")]
    mediatype: Option<String>,

    /// Restrict to an archive.org collection.
    #[arg(long, value_name = "NAME")]
    collection: Option<String>,
}

impl Filters {
    /// The filters as an archive.org query string.
    fn apply(&self, query: &str) -> String {
        let mut parts = vec![format!("({query})")];
        if let Some(mediatype) = &self.mediatype {
            parts.push(format!("mediatype:{mediatype}"));
        }
        if let Some(collection) = &self.collection {
            parts.push(format!("collection:{collection}"));
        }
        if self.torrents_only {
            parts.push(format!(
                "format:\"{}\"",
                balerion_ia::metadata::TORRENT_FORMAT
            ));
        }
        parts.join(" AND ")
    }

    /// The same filters expressed against the local tantivy schema.
    fn apply_local(&self, query: &str) -> String {
        let mut parts = vec![format!("({query})")];
        if let Some(mediatype) = &self.mediatype {
            parts.push(format!("mediatype:{mediatype}"));
        }
        if let Some(collection) = &self.collection {
            parts.push(format!("collection:{collection}"));
        }
        if self.torrents_only {
            parts.push("has_torrent:true".to_string());
        }
        parts.join(" AND ")
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    match &cli.command {
        Command::Search {
            query,
            filters,
            remote,
            limit,
            json,
        } => {
            let query = query.join(" ");
            if *remote {
                let client = client(&cli)?;
                let search_query =
                    AdvancedQuery::new(filters.apply(&query)).rows((*limit as u32).clamp(1, 500));
                let hits = advanced::collect(&client, &search_query, *limit, |_, _| {}).await?;
                print_remote_hits(&hits, *json)
            } else {
                let catalogue = catalogue(&cli)?;
                if catalogue.is_empty()? {
                    bail!(
                        "the catalogue is empty. Run `balerion harvest <query>` first, or search \
                         archive.org directly with `balerion search --remote {query}`"
                    );
                }
                let hits = catalogue.search(&filters.apply_local(&query), *limit)?;
                print_local_hits(&hits, *json)
            }
        }

        Command::Harvest {
            query,
            filters,
            limit,
            reset,
        } => {
            let query = AdvancedQuery::new(filters.apply(&query.join(" ")));
            harvest(&cli, query, *limit, *reset).await
        }

        Command::Info {
            identifier,
            files,
            json,
        } => {
            let client = client(&cli)?;
            let item = metadata::fetch(&client, identifier).await?;
            print_item(&item, *files, *json)
        }

        Command::Magnet { identifier } => {
            let client = client(&cli)?;
            let item = metadata::fetch(&client, identifier).await?;
            let meta = torrent::fetch(&client, &item).await?;
            println!("{}", meta.magnet_uri());
            Ok(())
        }

        Command::Torrent {
            identifier,
            output,
            show,
        } => {
            let client = client(&cli)?;
            let item = metadata::fetch(&client, identifier).await?;
            let url = item
                .torrent_url()
                .with_context(|| format!("{identifier} has no derived .torrent"))?;
            let raw = client.get(&url).await?;
            let meta = balerion_ia::Metainfo::parse(&raw)?;

            if *show {
                print_torrent(&meta);
                return Ok(());
            }
            let path = output
                .clone()
                .unwrap_or_else(|| PathBuf::from(format!("{identifier}_archive.torrent")));
            std::fs::write(&path, &raw).with_context(|| format!("writing {}", path.display()))?;
            println!(
                "{} ({}, {} pieces) -> {}",
                meta.name,
                fmt::bytes(meta.total_length),
                meta.piece_count(),
                path.display()
            );
            Ok(())
        }

        Command::Get {
            query,
            limit,
            list,
            first,
            pick,
            output,
            remote,
            engine,
        } => {
            let choice = match (list, first, pick) {
                (true, _, _) => get::Choice::ListOnly,
                (_, true, _) => get::Choice::First,
                (_, _, Some(position)) => get::Choice::Position(*position),
                _ => get::Choice::Prompt,
            };
            // The local catalogue is a cache, not a requirement: if it is
            // empty or absent we just ask archive.org.
            let catalogue = if *remote {
                None
            } else {
                catalogue(&cli)
                    .ok()
                    .filter(|c| !c.is_empty().unwrap_or(true))
            };
            get::get_command(
                &client(&cli)?,
                catalogue.as_ref(),
                &query.join(" "),
                *limit,
                choice,
                output.clone(),
                &engine.into(),
            )
            .await
        }

        Command::Resolve { target, engine } => {
            let source = torrent_source::resolve(target, &client(&cli)?).await?;
            download::resolve_command(&source, &engine.into()).await
        }

        Command::Download {
            target,
            output,
            engine,
        } => {
            let source = torrent_source::resolve(target, &client(&cli)?).await?;
            download::download_command(&source, output.clone(), &engine.into()).await
        }

        Command::Serve {
            port,
            host,
            output,
            low_bandwidth,
            peer_port,
            no_dht,
            no_webseeds,
            max_peers,
            dht_seconds,
        } => {
            let mut config = balerion_web::ServeConfig {
                host: *host,
                port: *port,
                data_dir: output
                    .clone()
                    .unwrap_or_else(balerion_web::default_data_dir),
                max_peers: *max_peers,
                use_dht: !*no_dht,
                use_webseeds: !*no_webseeds,
                peer_port: *peer_port,
                dht_budget: Duration::from_secs(*dht_seconds),
                ..Default::default()
            };
            // Applied after the explicit flags so it wins: someone passing
            // --low-bandwidth has said what they want louder than a default.
            if *low_bandwidth {
                config = config.thin_pipe();
            }
            balerion_web::serve(config).await
        }

        Command::Index { action } => {
            let catalogue = catalogue(&cli)?;
            match action {
                IndexAction::Stats => {
                    println!("path:         {}", index_dir(&cli)?.display());
                    println!("items:        {}", catalogue.len()?);
                    println!("with torrent: {}", catalogue.count("has_torrent:true")?);
                    Ok(())
                }
                IndexAction::Clear => {
                    let mut writer = catalogue.writer()?;
                    writer.clear()?;
                    println!("catalogue emptied");
                    Ok(())
                }
            }
        }
    }
}

async fn harvest(cli: &Cli, query: AdvancedQuery, limit: usize, reset: bool) -> Result<()> {
    let client = client(cli)?;
    let catalogue = catalogue(cli)?;
    let mut writer = catalogue.writer()?;
    if reset {
        writer.clear()?;
    }

    let progress = ProgressBar::new(limit.min(advanced::DEEP_PAGING_LIMIT) as u64);
    progress.set_style(
        ProgressStyle::with_template("{spinner} {pos}/{len} items  {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );

    // Collect first, index second: archive.org's paging is the slow part, and
    // holding a page of metadata in memory costs nothing worth worrying about.
    let hits = advanced::collect(&client, &query, limit, |hits, total| {
        progress.set_length(total.min(limit as u64).max(1));
        progress.set_position(hits.len() as u64);
        progress.set_message(format!("{total} match on archive.org"));
    })
    .await?;
    progress.finish_and_clear();

    for hit in &hits {
        writer.upsert(&to_record(hit))?;
    }
    writer.commit()?;

    if hits.len() >= advanced::DEEP_PAGING_LIMIT {
        eprintln!(
            "note: archive.org will not page past {} results; narrow the query to reach the rest",
            advanced::DEEP_PAGING_LIMIT
        );
    }
    println!(
        "harvested {} items into {} ({} in catalogue)",
        hits.len(),
        index_dir(cli)?.display(),
        catalogue.len()?
    );
    Ok(())
}

/// Map an archive.org search hit onto a catalogue record.
fn to_record(hit: &SearchHit) -> Record {
    Record {
        identifier: hit.identifier.clone(),
        title: hit.fields.title().map(str::to_string),
        creator: hit.fields.creator().map(str::to_string),
        description: hit
            .fields
            .description()
            .map(|d| fmt::truncate(&fmt::one_line(d), 2000)),
        mediatype: hit.fields.mediatype().map(str::to_string),
        publicdate: hit.fields.publicdate().map(str::to_string),
        subjects: hit
            .fields
            .subjects()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        collections: hit
            .fields
            .collections()
            .iter()
            .map(|s| s.to_string())
            .collect(),
        downloads: hit.downloads().unwrap_or(0),
        item_size: hit.item_size().unwrap_or(0),
        has_torrent: advanced::has_torrent(hit),
    }
}

fn print_remote_hits(hits: &[SearchHit], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(hits)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("nothing found");
        return Ok(());
    }
    for hit in hits {
        println!(
            "{:<40}  {:<9}  {:>10}  {}",
            fmt::truncate(&hit.identifier, 40),
            hit.mediatype().unwrap_or("-"),
            fmt::bytes(hit.item_size().unwrap_or(0)),
            fmt::truncate(&fmt::one_line(hit.title().unwrap_or("")), 60),
        );
    }
    println!("\n{} results", hits.len());
    Ok(())
}

fn print_local_hits(hits: &[balerion_index::Hit], json: bool) -> Result<()> {
    if json {
        let rows: Vec<_> = hits
            .iter()
            .map(|hit| {
                serde_json::json!({
                    "identifier": hit.record.identifier,
                    "title": hit.record.title,
                    "creator": hit.record.creator,
                    "mediatype": hit.record.mediatype,
                    "item_size": hit.record.item_size,
                    "downloads": hit.record.downloads,
                    "has_torrent": hit.record.has_torrent,
                    "score": hit.score,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!("nothing found");
        return Ok(());
    }
    for hit in hits {
        let record = &hit.record;
        println!(
            "{:<40}  {:<9}  {:>10}  {}{}",
            fmt::truncate(&record.identifier, 40),
            record.mediatype.as_deref().unwrap_or("-"),
            fmt::bytes(record.item_size),
            if record.has_torrent {
                ""
            } else {
                "(no torrent) "
            },
            fmt::truncate(&fmt::one_line(record.title.as_deref().unwrap_or("")), 60),
        );
    }
    println!("\n{} results", hits.len());
    Ok(())
}

fn print_item(item: &ItemMetadata, list_files: bool, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(item)?);
        return Ok(());
    }

    println!("identifier:  {}", item.identifier);
    if let Some(title) = item.metadata.title() {
        println!("title:       {}", fmt::one_line(title));
    }
    if let Some(creator) = item.metadata.creator() {
        println!("creator:     {creator}");
    }
    if let Some(mediatype) = item.metadata.mediatype() {
        println!("mediatype:   {mediatype}");
    }
    let collections = item.metadata.collections();
    if !collections.is_empty() {
        println!("collections: {}", collections.join(", "));
    }
    println!("size:        {}", fmt::bytes(item.total_size()));
    println!("files:       {}", item.files.len());
    println!(
        "details:     https://archive.org/details/{}",
        item.identifier
    );
    match item.torrent_url() {
        Some(url) => println!("torrent:     {url}"),
        None => println!("torrent:     none derived"),
    }
    let nodes = item.data_nodes();
    if !nodes.is_empty() {
        println!("data nodes:  {}", nodes.join(", "));
    }

    if list_files {
        println!();
        for file in &item.files {
            println!(
                "  {:>10}  {:<24}  {}",
                fmt::bytes(file.size.unwrap_or(0)),
                fmt::truncate(file.format.as_deref().unwrap_or("-"), 24),
                file.name
            );
        }
    }
    Ok(())
}

fn print_torrent(meta: &balerion_ia::Metainfo) {
    println!("name:        {}", meta.name);
    println!("infohash:    {}", meta.info_hash_hex());
    println!("size:        {}", fmt::bytes(meta.total_length));
    println!(
        "pieces:      {} x {}",
        meta.piece_count(),
        fmt::bytes(meta.piece_length)
    );
    println!("files:       {}", meta.files.len());
    if !meta.announce.is_empty() {
        println!("trackers:    {}", meta.announce.join("\n             "));
    }
    if !meta.webseeds.is_empty() {
        println!("webseeds:    {}", meta.webseeds.join("\n             "));
    }
    println!("magnet:      {}", meta.magnet_uri());
}

fn client(cli: &Cli) -> Result<IaClient> {
    let client = IaClient::with_config(ClientConfig {
        min_interval: Duration::from_millis(cli.min_interval),
        reduced_priority: cli.polite,
        s3_keys: s3_keys_from_env(),
        ..Default::default()
    })?;
    Ok(client)
}

/// Optional IA-S3 credentials. Reads do not need them, but they raise our
/// priority when archive.org is busy.
fn s3_keys_from_env() -> Option<(String, String)> {
    let access = std::env::var("IA_ACCESS_KEY").ok()?;
    let secret = std::env::var("IA_SECRET_KEY").ok()?;
    Some((access, secret))
}

fn catalogue(cli: &Cli) -> Result<Catalogue> {
    let dir = index_dir(cli)?;
    Catalogue::open(&dir).with_context(|| format!("opening catalogue at {}", dir.display()))
}

fn index_dir(cli: &Cli) -> Result<PathBuf> {
    if let Some(dir) = &cli.index_dir {
        return Ok(dir.clone());
    }
    let dirs = directories::ProjectDirs::from("org", "nightswatch", "balerion")
        .context("could not work out a data directory; pass --index-dir")?;
    Ok(dirs.data_dir().join("catalogue"))
}

fn init_tracing(verbose: u8) {
    let filter = match verbose {
        0 => "balerion=info,balerion_ia=info,balerion_index=warn",
        1 => "balerion=debug,balerion_ia=debug,balerion_index=info",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .with_target(false)
        .without_time()
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn local_filters_compose_into_a_tantivy_query() {
        let filters = Filters {
            torrents_only: true,
            mediatype: Some("audio".into()),
            collection: None,
        };
        assert_eq!(
            filters.apply_local("jazz"),
            "(jazz) AND mediatype:audio AND has_torrent:true"
        );
    }

    #[test]
    fn remote_filters_compose_into_a_scrape_query() {
        let filters = Filters {
            torrents_only: true,
            mediatype: None,
            collection: Some("librivoxaudio".into()),
        };
        assert_eq!(
            filters.apply("jazz"),
            "(jazz) AND collection:librivoxaudio AND format:\"Archive BitTorrent\""
        );
    }

    #[test]
    fn search_hits_become_records() {
        let body = br#"{"items":[{
            "identifier": "an-item",
            "title": "A Title",
            "creator": ["First Author", "Second Author"],
            "description": "line one\n   line two",
            "mediatype": "texts",
            "subject": ["birds", "rivers"],
            "collection": "opensource",
            "downloads": "17",
            "item_size": 4096,
            "format": ["Text PDF", "Archive BitTorrent"]
        }]}"#;
        let page = balerion_ia::SearchPage::parse("test", body).unwrap();
        let record = to_record(&page.hits[0]);

        assert_eq!(record.identifier, "an-item");
        assert_eq!(record.title.as_deref(), Some("A Title"));
        assert_eq!(record.creator.as_deref(), Some("First Author"));
        assert_eq!(record.description.as_deref(), Some("line one line two"));
        assert_eq!(record.subjects, vec!["birds", "rivers"]);
        assert_eq!(record.collections, vec!["opensource"]);
        assert_eq!(record.downloads, 17);
        assert_eq!(record.item_size, 4096);
        assert!(record.has_torrent);
    }

    #[test]
    fn items_without_a_torrent_format_are_marked_as_such() {
        let body = br#"{"items":[{"identifier":"plain","format":["Text PDF"]}]}"#;
        let page = balerion_ia::SearchPage::parse("test", body).unwrap();
        assert!(!to_record(&page.hits[0]).has_torrent);
    }
}
