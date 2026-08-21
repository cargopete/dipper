//! A local web UI for balerion: paste a magnet, watch the video while it
//! downloads.
//!
//! Browsers cannot speak BitTorrent, so this process does the torrenting and
//! serves the result over HTTP on localhost. The interesting part is that it
//! serves bytes that have not arrived yet: see [`stream`].

pub mod access;
pub mod cast;
pub mod convert;
pub mod fetched;
pub mod ffmpeg;
pub mod find;
pub mod fmp4;
pub mod history;
pub mod library;
pub mod media;
pub mod oidc;
pub mod play;
pub mod range;
pub mod relay;
pub mod release;
pub mod routes;
pub mod search;
pub mod state;
pub mod stream;
pub mod subsync;
pub mod subtitles;
pub mod supervise;
pub mod torrent;
pub mod tpb;
pub mod whisper;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::routing::{delete, get, post};

pub use state::{AppState, ServeConfig};

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 8080,
            data_dir: default_data_dir(),
            max_peers: 30,
            pipeline_depth: 16,
            use_dht: true,
            use_webseeds: true,
            peer_port: 6881,
            dht_budget: std::time::Duration::from_secs(20),
            tracker_timeout: std::time::Duration::from_secs(10),
            access_token: None,
            cast_port: None,
        }
    }
}

impl ServeConfig {
    /// Wind the request queues down for a slow or unreliable connection.
    ///
    /// Fewer peers and a shallower pipeline mean less data committed to
    /// requests that have not been answered yet. That costs peak throughput,
    /// which a thin link did not have, and buys the piece the player is
    /// waiting on a much shorter queue to sit in.
    pub fn thin_pipe(mut self) -> Self {
        self.max_peers = 8;
        self.pipeline_depth = 4;
        self
    }
}

/// Where torrents live when the user has not said otherwise.
pub fn default_data_dir() -> std::path::PathBuf {
    directories::ProjectDirs::from("", "", "balerion")
        .map(|dirs| dirs.cache_dir().join("torrents"))
        .unwrap_or_else(|| std::path::PathBuf::from(".balerion"))
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(routes::index))
        .route("/app.css", get(routes::stylesheet))
        .route("/app.js", get(routes::script))
        .route("/lib.js", get(routes::library_script))
        .route("/api/play/{hash}/{file}", get(play::info))
        .route("/api/play/{hash}/{file}/index.m3u8", get(play::playlist))
        .route("/api/play/{hash}/{file}/init.mp4", get(play::init))
        .route("/api/play/{hash}/{file}/seg/{index}", get(play::segment))
        .route("/api/play/{hash}/{file}/subs/{track}", get(play::embedded))
        .route("/api/subtitles/{hash}/{file}", get(play::sidecar))
        .route("/api/subtitles/{hash}/{file}/fetched", get(play::fetched))
        .route("/api/find", get(find::handler))
        .route("/api/sources", get(find::list))
        .route("/api/search", get(search::handler))
        .route("/api/shelves", get(search::shelves))
        .route("/api/tpb/search", get(tpb::handler))
        .route("/api/tpb/categories", get(tpb::categories))
        .route("/api/resolve", post(routes::resolve))
        .route("/api/cast", get(routes::cast_info))
        .route("/api/continue", get(routes::continuing))
        .route("/api/progress/{hash}/{file}", post(routes::progress))
        .route("/api/torrents", get(routes::list))
        .route("/api/torrents/{hash}", get(routes::stats))
        .route("/api/torrents/{hash}", delete(routes::remove))
        .route("/api/torrents/{hash}/keep", post(routes::keep))
        .route("/stream/{hash}/{file}", get(stream::handler))
        // A file that was converted up front, served as an ordinary file.
        .route("/ready/{hash}/{file}", get(convert::serve))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            access::guard,
        ))
        .with_state(state)
}

/// Run the server until interrupted.
pub async fn serve(config: ServeConfig) -> Result<()> {
    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .with_context(|| format!("creating {}", config.data_dir.display()))?;

    let address = SocketAddr::new(config.host, config.port);
    let mut config = config;

    // Bound anywhere but loopback, this needs a token. `/api/resolve` downloads
    // whatever magnet it is handed and `DELETE /api/torrents` deletes what you
    // were watching, so a log line saying "careful" was never enough. One is
    // generated rather than demanded: `--host` is a deliberate request, and
    // answering it with an error would be obstructive where answering it with
    // a working URL is not.
    if !config.host.is_loopback() && config.access_token.is_none() {
        config.access_token = Some(access::generate());
    }

    // Listen for peers before anything else needs to know the port. Failing to
    // bind is not fatal: outbound connections still work, they just reach
    // fewer peers, and a player that refuses to start over a busy port would
    // be a worse answer than a player that finds fewer seeders.
    let inbound = match balerion_bt::Inbound::bind(config.peer_port).await {
        Ok(inbound) => Some(inbound),
        Err(err) => {
            tracing::warn!(%err, "could not listen for peers; making outbound connections only");
            None
        }
    };
    if let Some(inbound) = &inbound {
        // What we announce is now what we are actually on, which for a while
        // it was not.
        config.peer_port = inbound.port();
    }

    let mut state = AppState::new(config);
    state.history = Arc::new(history::History::load(&state.config.data_dir).await);
    state.tools = ffmpeg::Tools::detect().await;
    state.whisper = whisper::Whisper::detect().await;
    state.inbound = inbound;
    let has_ffmpeg = state.tools.is_some();
    let state = Arc::new(state);

    // What the last run left behind: put back what was kept, collect what was
    // abandoned. Done before the sweeper starts, so the two cannot race over
    // the same directory.
    let found = library::adopt(&state).await;
    if found.kept > 0 || found.swept > 0 {
        tracing::info!(
            kept = found.kept,
            swept = found.swept,
            left = found.left,
            "read the data directory"
        );
    }

    // Supervised rather than spawned and forgotten. A panic in either of these
    // used to be absorbed by the runtime and reported nowhere: the sweeper
    // simply stopped sweeping for ever, and the first anyone would know is a
    // disk filling up a fortnight later.
    let sweeping = Arc::clone(&state);
    supervise::forever("sweeper", move || routes::sweep(Arc::clone(&sweeping)));
    let writing = Arc::clone(&state.history);
    supervise::forever("history", move || {
        history::keep_written(Arc::clone(&writing))
    });

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("could not bind {address}"))?;

    // ffmpeg reads files back through our own range endpoint, so it needs a
    // reachable address. Taken from the listener rather than the config, since
    // port 0 means the kernel chose it.
    let bound = listener.local_addr().unwrap_or(address);
    let reachable = if bound.ip().is_unspecified() {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), bound.port())
    } else {
        bound
    };
    *state.self_base.lock().expect("self_base lock") = format!("http://{reachable}");

    match &state.config.access_token {
        // Said on stdout with the other startup lines rather than through the
        // log, because it is the address the user has to type and a warning
        // nobody sees is a warning nobody acts on.
        Some(token) => {
            println!("Balerion is serving at http://{bound}");
            println!();
            println!("This is reachable from the network, so it is gated. Open:");
            println!("  http://{bound}/?{}={token}", access::TOKEN);
            println!("Anyone with that link can make balerion download and delete things.");
            println!("Requests from this machine are let through without it.");
            println!();
        }
        None => println!("Balerion is serving at http://{bound}"),
    }
    if has_ffmpeg {
        println!("ffmpeg found: files browsers cannot open will be converted as they play.");
    } else {
        println!(
            "ffmpeg not found: MKV, AVI and similar will be offered as downloads rather than \
             played. Install ffmpeg to have them converted."
        );
    }
    if let Some(port) = state.config.cast_port {
        // Bound wide on purpose: the whole point is that a television on the
        // network can reach it. What it serves is the reason that is safe.
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port);
        let cast_state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = cast::serve(cast_state, address).await {
                tracing::error!(%err, "the cast listener stopped");
            }
        });
        match cast::lan_address() {
            Some(ip) => println!("televisions should be pointed at http://{ip}:{port}"),
            None => println!(
                "casting is on, but this machine has no LAN address, so nothing can reach it"
            ),
        }
    }

    println!("paste a magnet link to start watching. Ctrl-C to stop.");

    // ConnectInfo, so the guard can tell a request from this machine from one
    // off the network.
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
        println!("\nstopping");
    })
    .await
    .context("the server stopped unexpectedly")?;
    Ok(())
}
