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

use crate::oidc::{OidcVerifier, VercelIdentity};
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
    /// A shared bearer token, when one is in use. Simple, and something a human
    /// has to copy between two places without getting it wrong.
    pub token: Option<String>,
    /// A Vercel project whose OIDC tokens are accepted instead. Nothing secret
    /// exists on both sides: these are public facts about whose project it is.
    pub vercel: Option<VercelIdentity>,
}

struct RelayState {
    tpb: balerion_tpb::TpbClient,
    token: Option<String>,
    vercel: Option<Arc<OidcVerifier>>,
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

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
}

/// Whether this request may proceed, by any method the relay was given.
///
/// The shared token is checked first because it costs nothing; OIDC verification
/// may fetch a key set. A relay with neither configured never starts, so there
/// is no path here that returns true by default.
async fn authorised(headers: &HeaderMap, state: &RelayState) -> bool {
    let Some(offered) = bearer(headers) else {
        return false;
    };

    if let Some(expected) = &state.token
        && same_token(offered, expected)
    {
        return true;
    }

    if let Some(verifier) = &state.vercel {
        match verifier.verify(offered).await {
            Ok(()) => return true,
            // Logged rather than returned: the caller is told only that it was
            // refused, while whoever runs the relay can see why.
            Err(err) => tracing::debug!(%err, "an OIDC token did not verify"),
        }
    }

    false
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
    if !authorised(&headers, &state).await {
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
    if !authorised(&headers, &state).await {
        return refused();
    }
    match tpb::run_search(&state.tpb, &params).await {
        Ok(results) => Json(results).into_response(),
        Err(err) => err.into_response(),
    }
}

async fn categories(State(state): State<Arc<RelayState>>, headers: HeaderMap) -> Response {
    if !authorised(&headers, &state).await {
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
    // At least one way in, and never none: a relay that authorises nothing would
    // be an open door with this machine's connection behind it.
    if config.token.is_none() && config.vercel.is_none() {
        bail!(
            "a relay needs either a shared token or a Vercel project to trust; there is no \
             default, because a relay without one is open"
        );
    }
    if let Some(token) = &config.token {
        if token.trim().is_empty() {
            bail!("the shared token is empty; leave it unset rather than blank");
        }
        if token.len() < 24 {
            bail!(
                "that token is {} characters; use at least 24, since it is one of the only \
                 things standing between the internet and this process",
                token.len()
            );
        }
    }

    let client = balerion_tpb::TpbClient::with_config(balerion_tpb::ClientConfig {
        timeout: UPSTREAM_TIMEOUT,
        ..Default::default()
    })
    .context("building the apibay client")?;

    let vercel = match config.vercel {
        Some(identity) => {
            println!(
                "trusting OIDC tokens from Vercel project {}/{} ({})",
                identity.owner, identity.project, identity.environment
            );
            Some(OidcVerifier::new(identity)?)
        }
        None => None,
    };
    if config.token.is_some() {
        println!("accepting a shared bearer token");
    }

    let state = Arc::new(RelayState {
        tpb: client,
        token: config.token,
        vercel,
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

    fn header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::AUTHORIZATION, value.parse().unwrap());
        headers
    }

    #[test]
    fn only_the_bearer_scheme_yields_a_token() {
        let token = "t".repeat(32);
        assert_eq!(bearer(&HeaderMap::new()), None, "no header at all");
        assert_eq!(bearer(&header(&token)), None, "bare token, no scheme");
        assert_eq!(
            bearer(&header(&format!("Bearer {token}"))),
            Some(token.as_str())
        );
        assert_eq!(
            bearer(&header(&format!("Bearer  {token} "))),
            Some(token.as_str()),
            "surrounding space is forgiven"
        );
    }

    fn state(token: Option<&str>) -> RelayState {
        RelayState {
            tpb: balerion_tpb::TpbClient::new().unwrap(),
            token: token.map(str::to_string),
            vercel: None,
        }
    }

    #[tokio::test]
    async fn the_shared_token_authorises_and_nothing_else_does() {
        let token = "t".repeat(32);
        let state = state(Some(&token));

        assert!(authorised(&header(&format!("Bearer {token}")), &state).await);
        assert!(!authorised(&HeaderMap::new(), &state).await, "no header");
        assert!(!authorised(&header(&token), &state).await, "no scheme");
        assert!(
            !authorised(&header("Bearer wrong-but-the-same-length-aaaa"), &state).await,
            "wrong token"
        );
    }

    #[tokio::test]
    async fn a_relay_with_no_shared_token_refuses_every_token() {
        // The OIDC-only case. Nothing may pass on the strength of a token the
        // relay was never given anything to check against.
        let state = state(None);
        assert!(!authorised(&header(&format!("Bearer {}", "t".repeat(32))), &state).await);
    }

    fn config(token: Option<&str>, vercel: Option<VercelIdentity>) -> RelayConfig {
        RelayConfig {
            host: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: 0,
            token: token.map(str::to_string),
            vercel,
        }
    }

    #[tokio::test]
    async fn a_relay_refuses_to_start_with_no_way_in_at_all() {
        let err = serve(config(None, None)).await.expect_err("should refuse");
        assert!(
            err.to_string().contains("token or a Vercel project"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_relay_refuses_a_token_too_short_to_be_worth_having() {
        for token in ["", "   ", "short"] {
            let err = serve(config(Some(token), None))
                .await
                .expect_err("should refuse");
            assert!(
                err.to_string().contains("token"),
                "unhelpful refusal for {token:?}: {}",
                err
            );
        }
    }
}
