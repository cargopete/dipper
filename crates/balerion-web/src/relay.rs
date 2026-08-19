//! A deliberately tiny public face for the apibay search.
//!
//! Why this exists rather than tunnelling [`crate::serve`]: apibay refuses
//! requests from datacentre addresses (Cloudflare answers them with a bot
//! challenge, not JSON), so a search running on Vercel cannot work. Running it
//! from a domestic connection can. This is the smallest thing that can sit on
//! that connection and be reachable from outside.
//!
//! What it deliberately does **not** serve is the point. The full server has
//! `/api/resolve`, which downloads whatever magnet it is handed; exposing that
//! to the internet would let any stranger who guesses the URL fill this
//! machine's disk with things nobody asked for. This process cannot start a
//! download, cannot read a file and cannot see the torrent session. It searches
//! and it answers, and that is the whole of its remit.
//!
//! Every route requires a bearer token, and there is no default token: a relay
//! started without one refuses to start rather than standing open.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use crate::tpb::{self, SearchParams};

/// How long a relay waits on apibay before giving up.
///
/// Shorter than the client's own patience on purpose: the caller is a serverless
/// function with a limit of its own, and a relay still waiting when that expires
/// is holding a connection nobody is listening to.
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub struct RelayConfig {
    pub host: std::net::IpAddr,
    pub port: u16,
    /// The bearer token every request must carry.
    pub token: String,
}

struct RelayState {
    tpb: balerion_tpb::TpbClient,
    token: String,
}

/// A token comparison that does not leak its answer through timing.
///
/// The obvious `==` returns as soon as two bytes differ, which tells an attacker
/// how much of the token they have right. Over enough requests that is a way in.
fn same_token(offered: &str, expected: &str) -> bool {
    if offered.len() != expected.len() {
        return false;
    }
    offered
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |differences, (a, b)| differences | (a ^ b))
        == 0
}

fn authorised(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|offered| same_token(offered.trim(), expected))
}

/// Said the same way whatever was wrong, so a caller learns only that it was
/// refused.
fn refused() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "not authorised" })),
    )
        .into_response()
}

#[derive(Debug, Serialize)]
struct Health {
    ok: bool,
    service: &'static str,
    version: &'static str,
}

async fn health(State(state): State<Arc<RelayState>>, headers: HeaderMap) -> Response {
    if !authorised(&headers, &state.token) {
        return refused();
    }
    Json(Health {
        ok: true,
        service: "balerion-relay",
        version: env!("CARGO_PKG_VERSION"),
    })
    .into_response()
}

async fn search(
    State(state): State<Arc<RelayState>>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Response {
    if !authorised(&headers, &state.token) {
        return refused();
    }
    match tpb::run_search(&state.tpb, &params).await {
        Ok(results) => Json(results).into_response(),
        Err(err) => err.into_response(),
    }
}

async fn categories(State(state): State<Arc<RelayState>>, headers: HeaderMap) -> Response {
    if !authorised(&headers, &state.token) {
        return refused();
    }
    tpb::categories().await.into_response()
}

fn router(state: Arc<RelayState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/search", get(search))
        .route("/categories", get(categories))
        .with_state(state)
}

/// Run the relay until interrupted.
pub async fn serve(config: RelayConfig) -> Result<()> {
    if config.token.trim().is_empty() {
        bail!("a relay needs a token; there is no default, because a relay without one is open");
    }
    if config.token.len() < 24 {
        bail!(
            "that token is {} characters; use at least 24, since this one is the only thing \
             standing between the internet and this process",
            config.token.len()
        );
    }

    let client = balerion_tpb::TpbClient::with_config(balerion_tpb::ClientConfig {
        timeout: UPSTREAM_TIMEOUT,
        ..Default::default()
    })
    .context("building the apibay client")?;

    let state = Arc::new(RelayState {
        tpb: client,
        token: config.token,
    });

    let address = SocketAddr::new(config.host, config.port);
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("could not bind {address}"))?;
    let bound = listener.local_addr().unwrap_or(address);

    println!("balerion relay listening on http://{bound}");
    if config.host.is_loopback() {
        println!("loopback only. Put Tailscale Funnel in front of it to reach it from outside.");
    } else {
        // Worth being loud about even though the token protects it: a relay on
        // a LAN address is reachable by everything on that LAN.
        tracing::warn!(%bound, "the relay is bound beyond loopback");
    }

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            println!("\nstopping");
        })
        .await
        .context("the relay stopped unexpectedly")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wrong_token_of_the_right_length_is_refused() {
        assert!(!same_token(
            "a".repeat(32).as_str(),
            "b".repeat(32).as_str()
        ));
    }

    #[test]
    fn the_right_token_is_accepted() {
        let token = "x".repeat(32);
        assert!(same_token(&token, &token));
    }

    #[test]
    fn a_prefix_of_the_token_is_not_enough() {
        // The length check has to come first, or the fold below reads past the
        // end of the shorter string.
        assert!(!same_token("xxxx", &"x".repeat(32)));
        assert!(!same_token(&"x".repeat(32), "xxxx"));
    }

    #[test]
    fn only_a_bearer_header_authorises() {
        let expected = "t".repeat(32);
        let mut headers = HeaderMap::new();
        assert!(!authorised(&headers, &expected), "no header at all");

        headers.insert(axum::http::header::AUTHORIZATION, expected.parse().unwrap());
        assert!(!authorised(&headers, &expected), "bare token, no scheme");

        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {expected}").parse().unwrap(),
        );
        assert!(authorised(&headers, &expected));
    }

    #[tokio::test]
    async fn a_relay_refuses_to_start_without_a_usable_token() {
        for token in ["", "   ", "short"] {
            let err = serve(RelayConfig {
                host: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                port: 0,
                token: token.to_string(),
            })
            .await
            .expect_err("should refuse");
            let message = err.to_string();
            assert!(
                message.contains("token"),
                "unhelpful refusal for {token:?}: {message}"
            );
        }
    }
}
