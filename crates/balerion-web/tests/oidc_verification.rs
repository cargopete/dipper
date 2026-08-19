//! Verifying a real signature, against a real key set, over real HTTP.
//!
//! This file exists because the unit tests in `oidc.rs` did not. They fed the
//! verifier malformed tokens, all of which are rejected before any cryptography
//! happens, so they passed while the crate was missing a provider feature and
//! **panicked** the moment it was asked to check a signature. That reached
//! production and came back as a 502.
//!
//! Everything here therefore goes through the whole path: sign a token, serve a
//! matching JWKS from a local listener, and make the verifier fetch it.
//!
//! The key below is a throwaway generated for this test and is worth nothing.

use std::net::SocketAddr;

use axum::Json;
use axum::routing::get;
use balerion_web::oidc::{OidcVerifier, VercelIdentity};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde::Serialize;

/// PKCS#1 DER rather than PEM: reading a PEM needs jsonwebtoken's `use_pem`
/// feature, which the production code has no use for, and enabling a feature for
/// a test's convenience is how a dependency grows. PKCS#1 specifically: the
/// parser's error message says "PKCS#8" but what it wants is the traditional
/// RSAPrivateKey, so `openssl rsa -traditional -outform DER` rather than
/// `openssl pkcs8 -topk8`.
const TEST_KEY: &[u8] = include_bytes!("oidc-testkey.der");
/// The modulus of TEST_KEY, base64url, as a JWKS would publish it.
const TEST_N: &str = "2QwTg-Y8TJhLe2SPmhKdcEDcIxvfDCvBXo25MDXAg5SPAKq4qyH-xwc6Vh2ye5fdDv3XnzsAVrLul0qdWf4JpwkRu6M68EAbskgKy09-Au5ZJJXcf1bs4Ft5RYytSFaiXAWS1WSVCJPt38_RW-XmTsRvb6pKO4faQOShKIBa8Ztmi3PvrRbuUFg5yq2xAkL9WQqL_M8pEKogq5pmlFJGlAsZc7u3SN-bMS9OYK_NjmWXtYHPkqxpMmxfDJyIc0BsNgYj9LdAvn6qQC5oEAsFS4LjG_OgsWodnKYg21ArLshNLvQm73eivcnxZlMI8cE30R6YODraTQEY1RGu0Erk_Q";
const TEST_E: &str = "AQAB";
const KID: &str = "test-key-1";

fn identity() -> VercelIdentity {
    VercelIdentity {
        owner: "nbgn".into(),
        project: "balerion".into(),
        environment: "production".into(),
    }
}

#[derive(Serialize)]
struct Claims {
    iss: String,
    aud: String,
    sub: String,
    exp: u64,
    nbf: u64,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A token signed by TEST_KEY, with whatever claims the caller wants to try.
fn sign(iss: &str, aud: &str, sub: &str, exp_offset: i64) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(KID.to_string());
    let claims = Claims {
        iss: iss.to_string(),
        aud: aud.to_string(),
        sub: sub.to_string(),
        exp: (now() as i64 + exp_offset) as u64,
        nbf: now() - 60,
    };
    let key = EncodingKey::from_rsa_der(TEST_KEY);
    encode(&header, &claims, &key).expect("signing should work")
}

/// Serve a JWKS holding the public half of TEST_KEY.
async fn spawn_jwks() -> SocketAddr {
    let app = Router::new().route(
        "/.well-known/jwks",
        get(|| async {
            Json(serde_json::json!({
                "keys": [{ "kty": "RSA", "alg": "RS256", "use": "sig",
                           "kid": KID, "n": TEST_N, "e": TEST_E }]
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

use axum::Router;

async fn verifier(addr: SocketAddr) -> std::sync::Arc<OidcVerifier> {
    OidcVerifier::with_jwks_url(identity(), Some(format!("http://{addr}/.well-known/jwks")))
        .unwrap()
}

#[tokio::test]
async fn a_properly_signed_token_from_the_right_project_verifies() {
    // The case the unit tests could not reach, and the one that panicked.
    let addr = spawn_jwks().await;
    let verifier = verifier(addr).await;
    let id = identity();
    let token = sign(&id.issuer(), &id.audience(), &id.subject(), 3600);
    verifier.verify(&token).await.expect("should verify");
}

#[tokio::test]
async fn a_token_for_another_project_in_the_same_account_is_refused() {
    // The security-critical one. Same issuer, same audience, same signing key:
    // only the subject separates them, so dropping that check would let any
    // project in the account through.
    let addr = spawn_jwks().await;
    let verifier = verifier(addr).await;
    let id = identity();
    let token = sign(
        &id.issuer(),
        &id.audience(),
        "owner:nbgn:project:something-else:environment:production",
        3600,
    );
    let err = verifier.verify(&token).await.expect_err("should refuse");
    assert!(err.to_string().contains("something-else"), "{err}");
}

#[tokio::test]
async fn a_preview_deployments_token_is_refused_by_a_production_relay() {
    let addr = spawn_jwks().await;
    let verifier = verifier(addr).await;
    let id = identity();
    let token = sign(
        &id.issuer(),
        &id.audience(),
        "owner:nbgn:project:balerion:environment:preview",
        3600,
    );
    assert!(verifier.verify(&token).await.is_err());
}

#[tokio::test]
async fn an_expired_token_is_refused() {
    let addr = spawn_jwks().await;
    let verifier = verifier(addr).await;
    let id = identity();
    let token = sign(&id.issuer(), &id.audience(), &id.subject(), -3600);
    assert!(verifier.verify(&token).await.is_err());
}

#[tokio::test]
async fn a_token_from_another_issuer_is_refused() {
    // Signed by the same key, which is the point: a valid signature is not the
    // same as a token meant for us.
    let addr = spawn_jwks().await;
    let verifier = verifier(addr).await;
    let id = identity();
    let token = sign(
        "https://oidc.vercel.com/somebody-else",
        &id.audience(),
        &id.subject(),
        3600,
    );
    assert!(verifier.verify(&token).await.is_err());
}

#[tokio::test]
async fn a_token_for_another_audience_is_refused() {
    let addr = spawn_jwks().await;
    let verifier = verifier(addr).await;
    let id = identity();
    let token = sign(
        &id.issuer(),
        "https://vercel.com/somebody-else",
        &id.subject(),
        3600,
    );
    assert!(verifier.verify(&token).await.is_err());
}
