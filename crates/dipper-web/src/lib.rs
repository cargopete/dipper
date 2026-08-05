//! A local web UI for dipper: paste a magnet, watch the video while it
//! downloads.
//!
//! Browsers cannot speak BitTorrent, so this process does the torrenting and
//! serves the result over HTTP on localhost. The interesting part is that it
//! serves bytes that have not arrived yet: see [`stream`].

pub mod ffmpeg;
pub mod fmp4;
pub mod media;
pub mod play;
pub mod range;
pub mod routes;
pub mod search;
pub mod state;
pub mod stream;
pub mod subtitles;
pub mod torrent;

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
    directories::ProjectDirs::from("", "", "dipper")
        .map(|dirs| dirs.cache_dir().join("torrents"))
        .unwrap_or_else(|| std::path::PathBuf::from(".dipper"))
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(routes::index))
        .route("/app.css", get(routes::stylesheet))
        .route("/app.js", get(routes::script))
        .route("/api/play/{hash}/{file}", get(play::info))
        .route("/api/play/{hash}/{file}/init.mp4", get(play::init))
        .route("/api/play/{hash}/{file}/seg/{index}", get(play::segment))
        .route("/api/play/{hash}/{file}/subs/{track}", get(play::embedded))
        .route("/api/subtitles/{hash}/{file}", get(play::sidecar))
        .route("/api/search", get(search::handler))
        .route("/api/shelves", get(search::shelves))
        .route("/api/resolve", post(routes::resolve))
        .route("/api/torrents", get(routes::list))
        .route("/api/torrents/{hash}", get(routes::stats))
        .route("/api/torrents/{hash}", delete(routes::remove))
        .route("/api/torrents/{hash}/keep", post(routes::keep))
        .route("/stream/{hash}/{file}", get(stream::handler))
        .with_state(state)
}

/// Run the server until interrupted.
pub async fn serve(config: ServeConfig) -> Result<()> {
    tokio::fs::create_dir_all(&config.data_dir)
        .await
        .with_context(|| format!("creating {}", config.data_dir.display()))?;

    let address = SocketAddr::new(config.host, config.port);
    if !config.host.is_loopback() {
        // Worth being loud about: this endpoint downloads whatever magnet it
        // is handed, so exposing it to a network is a decision, not a default.
        tracing::warn!(
            %address,
            "serving beyond localhost. Anyone who can reach this port can make \
             dipper download things on your behalf"
        );
    }

    let mut state = AppState::new(config);
    state.tools = ffmpeg::Tools::detect().await;
    let has_ffmpeg = state.tools.is_some();
    let state = Arc::new(state);
    tokio::spawn(routes::sweep(Arc::clone(&state)));

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

    println!("dipper is serving at http://{bound}");
    if has_ffmpeg {
        println!("ffmpeg found: files browsers cannot open will be converted as they play.");
    } else {
        println!(
            "ffmpeg not found: MKV, AVI and similar will be offered as downloads rather than \
             played. Install ffmpeg to have them converted."
        );
    }
    println!("paste a magnet link to start watching. Ctrl-C to stop.");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            println!("\nstopping");
        })
        .await
        .context("the server stopped unexpectedly")?;
    Ok(())
}
