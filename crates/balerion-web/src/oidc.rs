//! Verifying Vercel's OIDC tokens, so the relay needs no shared secret.
//!
//! A shared token works and is one more thing for somebody to copy between two
//! dashboards without pasting it into the wrong one. Vercel hands its own
//! functions a short-lived JWT instead, signed by a key we can fetch and check.
//! Nothing secret has to exist on both sides: the relay is configured with
//! public facts (whose account, which project) and rejects anything that does
//! not prove it is that project.
//!
//! What a token from `getVercelOidcToken()` claims, observed rather than assumed:
//!
//! ```text
//! iss  https://oidc.vercel.com/<owner>
//! aud  https://vercel.com/<owner>
//! sub  owner:<owner>:project:<project>:environment:<environment>
//! ```
//!
//! All four of issuer, audience, subject and expiry are checked. Dropping any
//! one of them is the difference between "a token from your project" and "a
//! token", and Vercel will issue one of the latter to anybody with an account.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;
use tokio::sync::Mutex;

/// How long a fetched key set is trusted before being fetched again.
///
/// Vercel rotates these, so an indefinite cache would eventually reject every
/// token. An hour is short enough to follow a rotation and long enough that the
/// relay is not fetching keys for every search.
const JWKS_TTL: Duration = Duration::from_secs(60 * 60);

/// Guards against a caller with a bad token forcing a fetch per request.
const MIN_REFETCH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct VercelIdentity {
    pub owner: String,
    pub project: String,
    pub environment: String,
}

impl VercelIdentity {
    pub fn issuer(&self) -> String {
        format!("https://oidc.vercel.com/{}", self.owner)
    }

    pub fn audience(&self) -> String {
        format!("https://vercel.com/{}", self.owner)
    }

    /// The exact `sub` a token from this project carries.
    pub fn subject(&self) -> String {
        format!(
            "owner:{}:project:{}:environment:{}",
            self.owner, self.project, self.environment
        )
    }

    fn jwks_url(&self) -> String {
        format!("{}/.well-known/jwks", self.issuer())
    }
}

#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kid: String,
    /// RSA modulus, base64url.
    n: String,
    /// RSA exponent, base64url.
    e: String,
}

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    // Deserialised so a malformed token fails here rather than later. The
    // library checks their values; we only need `sub` by name.
    #[allow(dead_code)]
    iss: String,
    #[allow(dead_code)]
    exp: u64,
}

struct Cache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
}

/// Checks bearer tokens against Vercel's published keys.
pub struct OidcVerifier {
    identity: VercelIdentity,
    /// Overrides the issuer's usual key set path. See [`Self::with_jwks_url`].
    jwks_url: Option<String>,
    http: reqwest::Client,
    cache: Mutex<Cache>,
}

impl OidcVerifier {
    pub fn new(identity: VercelIdentity) -> Result<Arc<Self>> {
        Self::with_jwks_url(identity, None)
    }

    /// Build a verifier that fetches its keys from somewhere other than the
    /// issuer's usual path.
    ///
    /// Exists so the signature check can be tested against a local key set. It
    /// is also the hook a self-hosted or mirrored issuer would need, which is
    /// why it is not hidden behind `cfg(test)`.
    pub fn with_jwks_url(identity: VercelIdentity, jwks_url: Option<String>) -> Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .context("building the JWKS client")?;
        Ok(Arc::new(Self {
            identity,
            jwks_url,
            http,
            cache: Mutex::new(Cache {
                keys: HashMap::new(),
                fetched_at: None,
            }),
        }))
    }

    pub fn identity(&self) -> &VercelIdentity {
        &self.identity
    }

    async fn fetch_keys(&self) -> Result<HashMap<String, DecodingKey>> {
        let url = self
            .jwks_url
            .clone()
            .unwrap_or_else(|| self.identity.jwks_url());
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("fetching {url}"))?;
        if !response.status().is_success() {
            return Err(anyhow!("{url} returned {}", response.status()));
        }
        let jwks: Jwks = response.json().await.context("parsing the key set")?;

        let mut keys = HashMap::new();
        for key in jwks.keys {
            match DecodingKey::from_rsa_components(&key.n, &key.e) {
                Ok(decoding) => {
                    keys.insert(key.kid, decoding);
                }
                // One unusable key is not a reason to reject the rest.
                Err(err) => tracing::warn!(kid = key.kid, %err, "skipping an unusable JWKS key"),
            }
        }
        if keys.is_empty() {
            return Err(anyhow!("{url} held no usable keys"));
        }
        Ok(keys)
    }

    /// The key for `kid`, fetching the set if it is stale or does not have it.
    async fn key_for(&self, kid: &str) -> Result<DecodingKey> {
        {
            let cache = self.cache.lock().await;
            let fresh = cache.fetched_at.is_some_and(|at| at.elapsed() < JWKS_TTL);
            if fresh {
                if let Some(key) = cache.keys.get(kid) {
                    return Ok(key.clone());
                }
                // An unknown kid with a fresh cache means a rotation, which is
                // worth one refetch. Rate limited, so a stream of tokens with
                // invented kids cannot turn into a stream of fetches.
                if cache
                    .fetched_at
                    .is_some_and(|at| at.elapsed() < MIN_REFETCH_INTERVAL)
                {
                    return Err(anyhow!("no key for kid {kid}"));
                }
            }
        }

        let keys = self.fetch_keys().await?;
        let found = keys.get(kid).cloned();
        let mut cache = self.cache.lock().await;
        cache.keys = keys;
        cache.fetched_at = Some(Instant::now());
        found.ok_or_else(|| anyhow!("no key for kid {kid}"))
    }

    /// Verify a bearer token. `Ok(())` means it came from the configured project.
    pub async fn verify(&self, token: &str) -> Result<()> {
        let header = decode_header(token).context("that is not a JWT")?;
        if header.alg != Algorithm::RS256 {
            // Pinned rather than taken from the header: trusting the header's
            // algorithm is how "alg: none" attacks work.
            return Err(anyhow!("unexpected algorithm {:?}", header.alg));
        }
        let kid = header.kid.ok_or_else(|| anyhow!("no kid in the header"))?;
        let key = self.key_for(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[self.identity.issuer()]);
        validation.set_audience(&[self.identity.audience()]);
        validation.validate_exp = true;
        validation.validate_nbf = true;

        let data =
            decode::<Claims>(token, &key, &validation).context("the token did not verify")?;

        // The library checked issuer, audience and expiry. Subject is ours: it
        // is what separates this project from every other project in the
        // account, all of which get tokens from the same issuer.
        let expected = self.identity.subject();
        if data.claims.sub != expected {
            return Err(anyhow!("token is for {}, not {expected}", data.claims.sub));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> VercelIdentity {
        VercelIdentity {
            owner: "nbgn".into(),
            project: "balerion".into(),
            environment: "production".into(),
        }
    }

    #[test]
    fn the_urls_and_claims_match_what_vercel_actually_issues() {
        // Observed from a live token rather than inferred from documentation.
        let id = identity();
        assert_eq!(id.issuer(), "https://oidc.vercel.com/nbgn");
        assert_eq!(id.audience(), "https://vercel.com/nbgn");
        assert_eq!(
            id.subject(),
            "owner:nbgn:project:balerion:environment:production"
        );
        assert_eq!(
            id.jwks_url(),
            "https://oidc.vercel.com/nbgn/.well-known/jwks"
        );
    }

    #[test]
    fn a_different_project_in_the_same_account_has_a_different_subject() {
        // The reason `sub` is checked at all: these two share an issuer, an
        // audience and a signing key.
        let mine = identity();
        let theirs = VercelIdentity {
            project: "something-else".into(),
            ..identity()
        };
        assert_eq!(mine.issuer(), theirs.issuer());
        assert_eq!(mine.audience(), theirs.audience());
        assert_ne!(mine.subject(), theirs.subject());
    }

    #[tokio::test]
    async fn rubbish_is_not_a_token() {
        let verifier = OidcVerifier::new(identity()).unwrap();
        for offered in ["", "nonsense", "a.b.c", "Bearer something"] {
            assert!(
                verifier.verify(offered).await.is_err(),
                "accepted {offered:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unsigned_token_is_refused_however_good_its_claims_look() {
        // `alg: none` with perfect claims. Refused because the algorithm is
        // pinned rather than read from the header.
        let header = b"{\"alg\":\"none\",\"typ\":\"JWT\"}";
        let claims = format!(
            "{{\"iss\":\"{}\",\"aud\":\"{}\",\"sub\":\"{}\",\"exp\":9999999999}}",
            identity().issuer(),
            identity().audience(),
            identity().subject()
        );
        let encode = |bytes: &[u8]| {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
        };
        let token = format!("{}.{}.", encode(header), encode(claims.as_bytes()));

        let verifier = OidcVerifier::new(identity()).unwrap();
        let err = verifier.verify(&token).await.unwrap_err();
        assert!(
            err.to_string().contains("algorithm") || err.to_string().contains("JWT"),
            "refused for the wrong reason: {err}"
        );
    }
}
