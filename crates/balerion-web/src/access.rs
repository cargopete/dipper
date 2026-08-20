//! Who is allowed to talk to the player.
//!
//! On loopback the answer is everybody, which is the whole design: the player
//! is a local program with a browser for a front end, and putting a password on
//! your own machine protects you from nobody.
//!
//! `--host` changes that completely. Bound to a LAN address, `/api/resolve` will
//! download whatever magnet any stranger on the wifi hands it, and
//! `DELETE /api/torrents/{hash}` will delete what you were watching. That is
//! exactly the exposure [`crate::cast`] was written to avoid, still reachable
//! through a flag, and until this module existed the only thing standing in the
//! way was a log line.
//!
//! So: requests arriving from loopback are let through untouched, and anything
//! from elsewhere has to carry the token. Loopback is exempt for a practical
//! reason as well as a philosophical one, since ffmpeg reads segments back
//! through our own range endpoint and would otherwise have to be taught to
//! authenticate to us.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// The name used for the query parameter, the header and the cookie alike.
pub const TOKEN: &str = "balerion_token";

/// A token to hand out when the player is exposed and nobody chose one.
///
/// Generated rather than refused. The user asked for `--host`, and answering a
/// deliberate request with an error is obstructive; answering it by standing
/// open is worse. Printing a URL that works is the only version of this that is
/// both safe and useful.
pub fn generate() -> String {
    use rand::RngExt;
    // `rng()` is a ChaCha generator seeded from the operating system, which is
    // what makes it fit to produce a credential. A reproducible token would be
    // worse than no token at all.
    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Compare two secrets without leaking where they first differ.
fn same_secret(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |diff, (x, y)| diff | (x ^ y))
        == 0
}

/// The token this request carries, from wherever it put it.
///
/// Three places because three kinds of client turn up: a person following a
/// link has it in the query, the page's own `fetch` calls have the cookie that
/// link set, and anything scripted will reach for a header.
fn offered(request: &Request) -> Option<String> {
    if let Some(query) = request.uri().query() {
        for pair in query.split('&') {
            if let Some(value) = pair.strip_prefix(&format!("{TOKEN}=")) {
                return Some(value.to_string());
            }
        }
    }
    if let Some(value) = request.headers().get(TOKEN).and_then(|v| v.to_str().ok()) {
        return Some(value.to_string());
    }
    request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|cookie| {
                cookie
                    .trim()
                    .strip_prefix(&format!("{TOKEN}="))
                    .map(str::to_string)
            })
        })
}

/// Let loopback through; make everyone else prove they were invited.
///
/// The peer address is optional rather than required, and deliberately so. A
/// required `ConnectInfo` extractor answers 500 when the service was not built
/// with one, which would turn a wiring mistake into a server that refuses
/// everybody with no explanation. Not knowing where a request came from is
/// instead treated as not being local, so the failure mode is "asks for the
/// token" rather than "breaks".
pub async fn guard(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let Some(token) = state.config.access_token.clone() else {
        return next.run(request).await;
    };

    // Read from the extensions rather than through the extractor, which is how
    // the absence stays a "no" instead of a rejection.
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    if peer.is_some_and(|peer| peer.ip().is_loopback()) {
        return next.run(request).await;
    }

    let Some(offered) = offered(&request) else {
        return refuse();
    };
    if !same_secret(&offered, &token) {
        return refuse();
    }

    // It was right, so remember it. Otherwise every asset and every poll would
    // need the token repeating in its URL, and the first `fetch` without one
    // would fail in a way that looks like a broken page.
    let mut response = next.run(request).await;
    if let Ok(cookie) = format!("{TOKEN}={token}; Path=/; SameSite=Lax; Max-Age=31536000").parse() {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response
}

fn refuse() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        "This balerion is listening beyond localhost and needs its access token. \
         It was printed when the server started; open the link it gave you.\n",
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_token_is_long_enough_to_be_worth_having() {
        let token = generate();
        assert_eq!(token.len(), 32, "128 bits as hex");
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(generate(), generate(), "two calls must not agree");
    }

    #[test]
    fn secrets_compare_by_value_not_by_prefix() {
        assert!(same_secret("abc123", "abc123"));
        assert!(!same_secret("abc123", "abc124"));
        assert!(!same_secret("abc", "abc123"), "length must not pass");
        assert!(!same_secret("", "x"));
    }

    fn request_with(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = Request::builder().uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    #[test]
    fn a_token_is_found_in_the_query() {
        let request = request_with("/?balerion_token=abc", &[]);
        assert_eq!(offered(&request).as_deref(), Some("abc"));
    }

    #[test]
    fn a_token_is_found_among_other_query_parameters() {
        let request = request_with("/stream/x/0?download=true&balerion_token=abc", &[]);
        assert_eq!(offered(&request).as_deref(), Some("abc"));
    }

    #[test]
    fn a_token_is_found_in_a_header_or_a_cookie() {
        assert_eq!(
            offered(&request_with("/", &[("balerion_token", "abc")])).as_deref(),
            Some("abc")
        );
        assert_eq!(
            offered(&request_with(
                "/",
                &[("cookie", "theme=dark; balerion_token=abc; other=1")]
            ))
            .as_deref(),
            Some("abc")
        );
    }

    /// A router with one route and the guard in front of it.
    fn guarded(token: Option<&str>) -> axum::Router {
        let state = Arc::new(AppState::new(crate::ServeConfig {
            access_token: token.map(str::to_string),
            ..Default::default()
        }));
        axum::Router::new()
            .route("/x", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                guard,
            ))
            .with_state(state)
    }

    /// Ask, claiming to be `from`. The address goes in as an extension rather
    /// than through a socket, which is the only way to be somewhere other than
    /// loopback in a test.
    async fn ask(router: axum::Router, uri: &str, from: &str) -> axum::response::Response {
        use tower::ServiceExt;
        let mut request = request_with(uri, &[]);
        request
            .extensions_mut()
            .insert(ConnectInfo(from.parse::<SocketAddr>().unwrap()));
        router.oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn with_no_token_configured_nothing_is_gated() {
        // The loopback default. A password on your own machine protects nobody.
        let response = ask(guarded(None), "/x", "192.0.2.7:1234").await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_stranger_without_the_token_is_refused() {
        let response = ask(guarded(Some("sesame")), "/x", "192.0.2.7:1234").await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_stranger_with_the_wrong_token_is_refused() {
        let response = ask(
            guarded(Some("sesame")),
            "/x?balerion_token=open",
            "192.0.2.7:1234",
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_stranger_with_the_token_is_let_in_and_given_a_cookie() {
        let response = ask(
            guarded(Some("sesame")),
            "/x?balerion_token=sesame",
            "192.0.2.7:1234",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        // Without this every later fetch would need the token repeating in its
        // URL, and the first one that forgot would look like a broken page.
        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .expect("the token should be remembered")
            .to_str()
            .unwrap();
        assert!(cookie.contains("balerion_token=sesame"), "{cookie}");
    }

    #[tokio::test]
    async fn this_machine_is_never_asked_for_the_token() {
        // ffmpeg reads segments back through our own range endpoint, and it
        // has not been taught to authenticate to us.
        let response = ask(guarded(Some("sesame")), "/x", "127.0.0.1:5555").await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn a_request_with_nothing_offers_nothing() {
        assert_eq!(offered(&request_with("/", &[])), None);
        assert_eq!(
            offered(&request_with(
                "/?download=true",
                &[("cookie", "theme=dark")]
            )),
            None
        );
    }
}
