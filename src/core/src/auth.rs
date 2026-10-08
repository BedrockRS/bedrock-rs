//! Checking that players are who they say they are.
//!
//! A signed-in client's Login carries a multiplayer token from the Minecraft
//! authorization service: a JWT signed with RS256 whose claims hold the
//! player's XUID (`xid`), PlayFab ID (`mid`), gamertag (`xname`) and public
//! key (`cpk`). The
//! [`Authenticator`] checks the signature against the service's published keys
//! and the token's issuer, audience and lifetime, as gophertunnel does. Only a
//! verified token's identity is used: the XUID-derived UUID becomes the
//! player's identity everywhere, plugins included.
//!
//! Keys come from the service's OpenID configuration and are cached. An
//! unknown key ID (the service rotated its keys) triggers a refetch, at most
//! once a minute, and cached keys are refetched after six hours.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rsa::PublicKeyComponents;
use aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD_INDIFFERENT;
use bedrockrs_protocol::login::{IdentityClaims, Jwt, LoginError};
use serde_json::Value;
use tokio::sync::RwLock;

/// The authorization service that issues multiplayer tokens.
pub const ISSUER: &str = "https://authorization.franchise.minecraft-services.net/";
/// Where the service publishes its OpenID configuration, which names its keys.
const OPENID_CONFIGURATION: &str =
    "https://authorization.franchise.minecraft-services.net/.well-known/openid-configuration";
/// The audience multiplayer tokens are issued for.
pub const AUDIENCE: &str = "api://auth-minecraft-services/multiplayer";
/// How far clocks may disagree when checking a token's lifetime.
const CLOCK_SKEW: Duration = Duration::from_secs(60);
/// Shortest time between key fetches caused by an unknown key ID.
const REFETCH_COOLDOWN: Duration = Duration::from_secs(60);
/// Cached keys are refetched once they are this old.
const KEY_MAX_AGE: Duration = Duration::from_secs(6 * 60 * 60);
/// Longest a key fetch may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a player could not be authenticated.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("the login has no multiplayer token; sign in to a Microsoft account")]
    NoToken,
    #[error("the token is malformed: {0}")]
    Malformed(#[from] LoginError),
    #[error("the token is signed with {0:?}, not RS256")]
    Algorithm(String),
    #[error("the token is signed with an unknown key")]
    UnknownKey,
    #[error("the token's signature is invalid")]
    BadSignature,
    #[error("the token was issued by {0:?}, not the Minecraft authorization service")]
    Issuer(String),
    #[error("the token is not meant for multiplayer servers")]
    Audience,
    #[error("the token has expired or is not valid yet")]
    Expired,
    #[error("the token does not name a player")]
    NoIdentity,
    #[error("could not reach the Minecraft authorization service: {0}")]
    Unreachable(String),
}

/// One of the service's RSA signing keys.
#[derive(Debug, Clone)]
struct RsaKey {
    modulus: Vec<u8>,
    exponent: Vec<u8>,
}

impl RsaKey {
    fn verify(&self, message: &[u8], signature: &[u8]) -> bool {
        PublicKeyComponents {
            n: &self.modulus[..],
            e: &self.exponent[..],
        }
        .verify(&RSA_PKCS1_2048_8192_SHA256, message, signature)
        .is_ok()
    }
}

#[derive(Debug, Default)]
struct KeyCache {
    keys: HashMap<String, RsaKey>,
    fetched: Option<Instant>,
    last_attempt: Option<Instant>,
}

#[derive(Debug)]
enum Mode {
    /// Verify tokens against the authorization service.
    Online {
        http: reqwest::Client,
        keys: RwLock<KeyCache>,
    },
    /// Verify tokens against fixed keys, never fetching any; for tests.
    #[cfg(test)]
    Fixed(HashMap<String, RsaKey>),
    /// Trust tokens without checking them, for offline testing.
    Offline,
}

/// Verifies multiplayer tokens; see the module docs.
#[derive(Debug)]
pub struct Authenticator {
    mode: Mode,
}

impl Authenticator {
    /// An authenticator that checks tokens against the Minecraft
    /// authorization service. Keys are fetched on the first login.
    pub fn online() -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .user_agent(concat!("BedrockRS/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            mode: Mode::Online {
                http,
                keys: RwLock::new(KeyCache::default()),
            },
        })
    }

    /// An authenticator that trusts every token without checking it. Anyone
    /// can then join as anyone: only for testing without internet access.
    pub fn offline() -> Self {
        Self {
            mode: Mode::Offline,
        }
    }

    pub fn is_online(&self) -> bool {
        !matches!(self.mode, Mode::Offline)
    }

    /// The verified identity in a login's multiplayer token.
    pub async fn verify(&self, token: &str) -> Result<IdentityClaims, AuthError> {
        if token.is_empty() {
            return Err(AuthError::NoToken);
        }
        let jwt = Jwt::parse(token)?;
        match &self.mode {
            Mode::Offline => return identity(&jwt.claims),
            Mode::Online { http, keys } => {
                let key = self.online_key(http, keys, &jwt).await?;
                check_signature(&jwt, &key)?;
            }
            #[cfg(test)]
            Mode::Fixed(keys) => {
                let kid = jwt.header_field("kid").unwrap_or_default();
                check_signature(&jwt, keys.get(kid).ok_or(AuthError::UnknownKey)?)?;
            }
        }
        check_claims(&jwt.claims, SystemTime::now())?;
        identity(&jwt.claims)
    }

    /// The key a token names, fetching the service's keys if they are
    /// missing, stale, or do not include it.
    async fn online_key(
        &self,
        http: &reqwest::Client,
        cache: &RwLock<KeyCache>,
        jwt: &Jwt,
    ) -> Result<RsaKey, AuthError> {
        let alg = jwt.header_field("alg").unwrap_or_default();
        if alg != "RS256" {
            return Err(AuthError::Algorithm(alg.to_owned()));
        }
        let kid = jwt.header_field("kid").ok_or(AuthError::UnknownKey)?;
        {
            let cache = cache.read().await;
            let fresh = cache.fetched.is_some_and(|at| at.elapsed() < KEY_MAX_AGE);
            if let (true, Some(key)) = (fresh, cache.keys.get(kid)) {
                return Ok(key.clone());
            }
        }

        let mut cache = cache.write().await;
        // Another login may have fetched while this one waited.
        let fresh = cache.fetched.is_some_and(|at| at.elapsed() < KEY_MAX_AGE);
        if fresh && let Some(key) = cache.keys.get(kid) {
            return Ok(key.clone());
        }
        let cooling = cache
            .last_attempt
            .is_some_and(|at| at.elapsed() < REFETCH_COOLDOWN);
        if !cooling || !fresh {
            cache.last_attempt = Some(Instant::now());
            match fetch_keys(http).await {
                Ok(keys) => {
                    tracing::debug!(
                        keys = keys.len(),
                        "fetched the authorization service's signing keys"
                    );
                    cache.keys = keys;
                    cache.fetched = Some(Instant::now());
                }
                // Stale keys still verify tokens signed with them.
                Err(err) if cache.keys.contains_key(kid) => {
                    tracing::warn!(
                        "Couldn't refresh the Microsoft sign-in keys, so the cached ones are used: {err}"
                    );
                }
                Err(err) => return Err(err),
            }
        }
        cache.keys.get(kid).cloned().ok_or(AuthError::UnknownKey)
    }
}

/// Fetches the service's current signing keys, by key ID.
async fn fetch_keys(http: &reqwest::Client) -> Result<HashMap<String, RsaKey>, AuthError> {
    let unreachable = |err: reqwest::Error| AuthError::Unreachable(err.to_string());
    let configuration: Value = get_json(http, OPENID_CONFIGURATION)
        .await
        .map_err(unreachable)?;
    let jwks_uri = configuration
        .get("jwks_uri")
        .and_then(Value::as_str)
        .ok_or_else(|| AuthError::Unreachable("the OpenID configuration has no jwks_uri".into()))?;
    let key_set: Value = get_json(http, jwks_uri).await.map_err(unreachable)?;
    Ok(parse_key_set(&key_set))
}

async fn get_json(http: &reqwest::Client, url: &str) -> Result<Value, reqwest::Error> {
    let bytes = http
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    // A body that is not JSON reads as null, which the callers reject.
    Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// The RSA keys in a JWK set, by key ID; other key types are skipped.
fn parse_key_set(key_set: &Value) -> HashMap<String, RsaKey> {
    let decode = |key: &Value, field: &str| {
        key.get(field)
            .and_then(Value::as_str)
            .and_then(|value| URL_SAFE_NO_PAD_INDIFFERENT.decode(value).ok())
    };
    key_set
        .get("keys")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|key| key.get("kty").and_then(Value::as_str) == Some("RSA"))
        .filter_map(|key| {
            let kid = key.get("kid").and_then(Value::as_str)?.to_owned();
            let rsa = RsaKey {
                modulus: decode(key, "n")?,
                exponent: decode(key, "e")?,
            };
            Some((kid, rsa))
        })
        .collect()
}

fn check_signature(jwt: &Jwt, key: &RsaKey) -> Result<(), AuthError> {
    if key.verify(jwt.signing_input.as_bytes(), &jwt.signature) {
        Ok(())
    } else {
        Err(AuthError::BadSignature)
    }
}

/// The issuer, audience and lifetime of a verified token.
fn check_claims(claims: &Value, now: SystemTime) -> Result<(), AuthError> {
    let issuer = claims
        .get("iss")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if issuer.trim_end_matches('/') != ISSUER.trim_end_matches('/') {
        return Err(AuthError::Issuer(issuer.to_owned()));
    }
    let audience_matches = match claims.get("aud") {
        Some(Value::String(audience)) => audience == AUDIENCE,
        Some(Value::Array(audiences)) => audiences.iter().any(|audience| audience == AUDIENCE),
        _ => false,
    };
    if !audience_matches {
        return Err(AuthError::Audience);
    }

    let now = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let skew = CLOCK_SKEW.as_secs_f64();
    let time = |name: &str| claims.get(name).and_then(Value::as_f64);
    let expired = time("exp").is_none_or(|exp| now > exp + skew);
    let early = time("nbf").is_some_and(|nbf| now + skew < nbf);
    if expired || early {
        return Err(AuthError::Expired);
    }
    Ok(())
}

/// The identity a token names, which must include a player.
fn identity(claims: &Value) -> Result<IdentityClaims, AuthError> {
    let identity = IdentityClaims::from_token_claims(claims);
    if identity.display_name.is_none() || identity.identity.is_none() {
        return Err(AuthError::NoIdentity);
    }
    Ok(identity)
}

#[cfg(test)]
pub(crate) mod tests {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::{KeyPair as _, RSA_PKCS1_SHA256, RsaKeyPair};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::json;

    use super::*;

    /// A stand-in for the authorization service: a signing key and the
    /// authenticator that trusts it.
    pub(crate) struct TestIssuer {
        key: RsaKeyPair,
    }

    impl TestIssuer {
        pub(crate) fn new() -> Self {
            Self {
                key: RsaKeyPair::generate(KeySize::Rsa2048).unwrap(),
            }
        }

        pub(crate) fn authenticator(&self) -> Authenticator {
            let public = self.key.public_key();
            let key = RsaKey {
                modulus: public.modulus().big_endian_without_leading_zero().to_vec(),
                exponent: public.exponent().big_endian_without_leading_zero().to_vec(),
            };
            Authenticator {
                mode: Mode::Fixed(HashMap::from([("test-key".to_owned(), key)])),
            }
        }

        /// A token with these claims, signed with the key under `kid`.
        pub(crate) fn sign(&self, kid: &str, claims: &Value) -> String {
            let header = json!({ "alg": "RS256", "kid": kid, "typ": "JWT" });
            let input = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(header.to_string()),
                URL_SAFE_NO_PAD.encode(claims.to_string())
            );
            let mut signature = vec![0; self.key.public_modulus_len()];
            self.key
                .sign(
                    &RSA_PKCS1_SHA256,
                    &SystemRandom::new(),
                    input.as_bytes(),
                    &mut signature,
                )
                .unwrap();
            format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature))
        }

        /// A valid token for a player with this XUID and name.
        pub(crate) fn token(&self, xuid: &str, name: &str, cpk: &Value) -> String {
            self.sign("test-key", &claims(xuid, name, cpk))
        }
    }

    pub(crate) fn claims(xuid: &str, name: &str, cpk: &Value) -> Value {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        json!({
            "iss": ISSUER,
            "aud": AUDIENCE,
            "iat": now,
            "nbf": now,
            "exp": now + 3600,
            "xid": xuid,
            "mid": format!("PF{xuid}"),
            "xname": name,
            "cpk": cpk,
            "ipt": "PlayFab",
            "tid": "20CA2",
        })
    }

    #[tokio::test]
    async fn accepts_a_signed_token_and_derives_the_uuid() {
        let issuer = TestIssuer::new();
        let token = issuer.token("2535400000000000", "Steve", &json!("MHYw"));
        let identity = issuer.authenticator().verify(&token).await.unwrap();
        assert_eq!(identity.display_name.as_deref(), Some("Steve"));
        assert_eq!(identity.pfid.as_deref(), Some("PF2535400000000000"));
        assert_eq!(
            identity.identity.unwrap().to_string(),
            "174319cc-f69f-30d8-a279-6ace57f2011e"
        );
    }

    #[tokio::test]
    async fn rejects_forged_and_misdirected_tokens() {
        let issuer = TestIssuer::new();
        let authenticator = issuer.authenticator();
        let cpk = json!("MHYw");

        // Signed by someone else under the trusted key ID.
        let forger = TestIssuer::new();
        let forged = forger.token("2535400000000000", "Steve", &cpk);
        assert!(matches!(
            authenticator.verify(&forged).await,
            Err(AuthError::BadSignature)
        ));

        // A real token whose claims were edited afterwards.
        let token = issuer.token("2535400000000000", "Steve", &cpk);
        let mut parts: Vec<&str> = token.split('.').collect();
        let edited = URL_SAFE_NO_PAD.encode(claims("2535400000000001", "Admin", &cpk).to_string());
        parts[1] = &edited;
        assert!(matches!(
            authenticator.verify(&parts.join(".")).await,
            Err(AuthError::BadSignature)
        ));

        // An unknown key ID.
        let unknown = issuer.sign("other-key", &claims("1", "Alex", &cpk));
        assert!(matches!(
            authenticator.verify(&unknown).await,
            Err(AuthError::UnknownKey)
        ));

        // Wrong issuer, wrong audience, expired.
        let mut wrong = claims("1", "Alex", &cpk);
        wrong["iss"] = json!("https://example.com/");
        assert!(matches!(
            authenticator.verify(&issuer.sign("test-key", &wrong)).await,
            Err(AuthError::Issuer(_))
        ));
        let mut wrong = claims("1", "Alex", &cpk);
        wrong["aud"] = json!("api://somewhere-else");
        assert!(matches!(
            authenticator.verify(&issuer.sign("test-key", &wrong)).await,
            Err(AuthError::Audience)
        ));
        let mut wrong = claims("1", "Alex", &cpk);
        wrong["exp"] = json!(1_000_000);
        assert!(matches!(
            authenticator.verify(&issuer.sign("test-key", &wrong)).await,
            Err(AuthError::Expired)
        ));

        // No token, and a token without a player.
        assert!(matches!(
            authenticator.verify("").await,
            Err(AuthError::NoToken)
        ));
        let mut nameless = claims("1", "Alex", &cpk);
        nameless.as_object_mut().unwrap().remove("xname");
        assert!(matches!(
            authenticator
                .verify(&issuer.sign("test-key", &nameless))
                .await,
            Err(AuthError::NoIdentity)
        ));
    }

    #[tokio::test]
    async fn offline_mode_trusts_the_claims() {
        let forger = TestIssuer::new();
        let token = forger.token("2535400000000000", "Steve", &json!("MHYw"));
        let identity = Authenticator::offline().verify(&token).await.unwrap();
        assert_eq!(identity.display_name.as_deref(), Some("Steve"));
    }

    /// Fetches the live keys; needs internet access, so it only runs when asked
    /// (`cargo test -p bedrockrs_core -- --ignored`).
    #[tokio::test]
    #[ignore = "reaches the Minecraft authorization service"]
    async fn fetches_the_live_signing_keys() {
        let Mode::Online { http, .. } = Authenticator::online().unwrap().mode else {
            unreachable!();
        };
        let keys = fetch_keys(&http).await.unwrap();
        assert!(!keys.is_empty());
        assert!(keys.values().all(|key| key.modulus.len() >= 256));
    }

    #[test]
    fn key_sets_keep_rsa_keys_by_id() {
        let key_set = json!({ "keys": [
            { "kty": "RSA", "kid": "a", "n": "AQAB", "e": "AQAB" },
            { "kty": "EC", "kid": "b", "x": "AA", "y": "AA" },
            { "kty": "RSA", "kid": "c", "n": "not base64 ☃", "e": "AQAB" },
        ]});
        let keys = parse_key_set(&key_set);
        assert_eq!(keys.keys().collect::<Vec<_>>(), ["a"]);
    }
}
