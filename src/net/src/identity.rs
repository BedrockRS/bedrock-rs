//! NetherNet identity assertions: the RFC 8827 `a=identity` SDP attribute.
//!
//! The server signs every answer with a persistent P-384 key. The attribute holds
//! a self-signed ES384 JWT whose `cpk` claim is our public key, plus a detached
//! JWS over the answer's DTLS fingerprints. Clients pin the key on first use when
//! signaling runs over plain HTTP, so it must stay stable across restarts.
//! Formats follow go-nethernet, which matches vanilla BDS (`docs/ARCHITECTURE.md` §3.2).

use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::{
    STANDARD, STANDARD_PAD_INDIFFERENT, URL_SAFE_NO_PAD, URL_SAFE_NO_PAD_INDIFFERENT,
};
use p384::ecdsa::signature::{Signer as _, Verifier as _};
use p384::ecdsa::{Signature, SigningKey, VerifyingKey};
use p384::elliptic_curve::Generate as _;
use p384::pkcs8::{DecodePrivateKey as _, DecodePublicKey as _, EncodePrivateKey as _};
use p384::pkcs8::{EncodePublicKey as _, LineEnding};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use crate::sdp::SdpFingerprint;

/// Identity-provider domain that vanilla BDS puts in its answers.
pub const SELF_DOMAIN: &str = "self";

/// Lifetime of the self-signed token in each answer, matching BDS.
const TOKEN_LIFETIME: Duration = Duration::from_secs(60);
const IDP_PROTOCOL: &str = "default";
const ES384: &str = "ES384";
const P384_COORDINATE_LEN: usize = 48;

/// Errors from loading keys or building and checking identity assertions.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("failed to read identity key {}: {source}", .path.display())]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to write identity key {}: {source}", .path.display())]
    Write { path: PathBuf, source: io::Error },
    #[error("invalid identity key: {0}")]
    InvalidKey(String),
    #[error("failed to generate identity key: {0}")]
    Generate(String),
    #[error("malformed identity assertion: {0}")]
    Malformed(String),
    #[error("identity signature does not cover the offered DTLS fingerprints")]
    BadSignature,
}

impl From<serde_json::Error> for IdentityError {
    fn from(err: serde_json::Error) -> Self {
        Self::Malformed(err.to_string())
    }
}

impl From<base64::DecodeError> for IdentityError {
    fn from(err: base64::DecodeError) -> Self {
        Self::Malformed(err.to_string())
    }
}

/// The server's persistent identity key.
pub struct ServerIdentity {
    key: SigningKey,
    domain: String,
    /// Base64 SPKI DER public key, sent in the JWT's `x5u` header like BDS does.
    x5u: String,
    cpk: Jwk,
}

impl ServerIdentity {
    /// Loads the PKCS#8 PEM key at `path`, generating and saving a new one if the
    /// file does not exist yet.
    pub fn load_or_create(path: &Path, domain: impl Into<String>) -> Result<Self, IdentityError> {
        let domain = domain.into();
        match fs::read_to_string(path) {
            Ok(pem) => Self::from_pkcs8_pem(&pem, domain),
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                let identity = Self::generate(domain)?;
                let pem = identity
                    .key
                    .to_pkcs8_pem(LineEnding::LF)
                    .map_err(|err| IdentityError::InvalidKey(err.to_string()))?;
                write_private_file(path, pem.as_bytes()).map_err(|source| {
                    IdentityError::Write {
                        path: path.to_owned(),
                        source,
                    }
                })?;
                tracing::info!(path = %path.display(), "generated a new NetherNet identity key");
                Ok(identity)
            }
            Err(source) => Err(IdentityError::Read {
                path: path.to_owned(),
                source,
            }),
        }
    }

    /// Creates an identity with a fresh random key.
    pub fn generate(domain: impl Into<String>) -> Result<Self, IdentityError> {
        let key =
            SigningKey::try_generate().map_err(|err| IdentityError::Generate(err.to_string()))?;
        Self::from_key(key, domain.into())
    }

    /// Creates an identity from a PKCS#8 PEM encoded P-384 private key.
    pub fn from_pkcs8_pem(pem: &str, domain: impl Into<String>) -> Result<Self, IdentityError> {
        let key = SigningKey::from_pkcs8_pem(pem)
            .map_err(|err| IdentityError::InvalidKey(err.to_string()))?;
        Self::from_key(key, domain.into())
    }

    fn from_key(key: SigningKey, domain: String) -> Result<Self, IdentityError> {
        let der = key
            .verifying_key()
            .to_public_key_der()
            .map_err(|err| IdentityError::InvalidKey(err.to_string()))?;
        let cpk = Jwk::from_key(key.verifying_key())?;
        Ok(Self {
            x5u: STANDARD.encode(der.as_bytes()),
            key,
            domain,
            cpk,
        })
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        self.key.verifying_key()
    }

    /// SHA-256 of the public key (SPKI DER) as colon-separated hex, for operator logs.
    pub fn key_fingerprint(&self) -> String {
        let der = STANDARD.decode(&self.x5u).unwrap_or_default();
        Sha256::digest(der)
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<Vec<_>>()
            .join(":")
    }

    /// Builds the base64 `a=identity` value binding this identity to `fingerprints`,
    /// the DTLS fingerprints written in the same answer.
    pub fn assertion(&self, fingerprints: &[SdpFingerprint]) -> Result<String, IdentityError> {
        let token = self.token(SystemTime::now())?;
        let fingerprints = detached_jws(&self.key, &fingerprint_payload(fingerprints));
        encode_identity(&self.domain, token, fingerprints)
    }

    /// Self-signed JWT carrying our public key, shaped like the one BDS sends.
    fn token(&self, now: SystemTime) -> Result<String, IdentityError> {
        let iat = now
            .duration_since(UNIX_EPOCH)
            .map_err(|err| IdentityError::Malformed(err.to_string()))?
            .as_secs();
        let header = serde_json::to_vec(&JwsHeader {
            alg: ES384.to_owned(),
            x5u: Some(self.x5u.clone()),
        })?;
        let claims = serde_json::to_vec(&ServerClaims {
            cpk: &self.cpk,
            exp: iat + TOKEN_LIFETIME.as_secs(),
            iat,
        })?;
        let (signing_input, signature) = sign(&self.key, &header, &claims);
        Ok(format!("{signing_input}.{signature}"))
    }
}

impl fmt::Debug for ServerIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("domain", &self.domain)
            .field("key_fingerprint", &self.key_fingerprint())
            .finish_non_exhaustive()
    }
}

/// A client identity recovered from an SDP offer.
///
/// `public_key` is proven to have signed the offer's DTLS fingerprints, so the
/// connection is bound to it. The `token` itself is **not** yet verified against
/// the Minecraft authorization service; until it is, the player is unauthenticated.
#[derive(Clone)]
pub struct ClientIdentity {
    /// Identity provider named by the client, e.g. the Minecraft authorization service.
    pub issuer: String,
    /// The client's GameServerToken (a JWT), unverified.
    pub token: String,
    pub public_key: VerifyingKey,
}

impl fmt::Debug for ClientIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientIdentity")
            .field("issuer", &self.issuer)
            .finish_non_exhaustive()
    }
}

/// Decodes a client's `a=identity` value and checks that the key in its token
/// signed `fingerprints`, the DTLS fingerprints of the same offer.
pub fn verify_client(
    attribute: &str,
    fingerprints: &[SdpFingerprint],
) -> Result<ClientIdentity, IdentityError> {
    let data: IdentityData =
        serde_json::from_slice(&STANDARD_PAD_INDIFFERENT.decode(attribute.trim())?)?;
    if data.idp.protocol != IDP_PROTOCOL {
        return Err(IdentityError::Malformed(format!(
            "unsupported identity protocol {:?}",
            data.idp.protocol
        )));
    }
    let assertion: Assertion = serde_json::from_str(&data.assertion)?;
    let public_key = token_public_key(&assertion.token)?;
    verify_detached(
        &assertion.fingerprints,
        &fingerprint_payload(fingerprints),
        &public_key,
    )?;
    Ok(ClientIdentity {
        issuer: data.idp.domain,
        token: assertion.token,
        public_key,
    })
}

/// Canonical JSON over DTLS fingerprints that identity assertions sign, e.g.
/// `{"fingerprint":[{"algorithm":"sha-256","digest":"AA:BB"}]}`.
pub fn fingerprint_payload(fingerprints: &[SdpFingerprint]) -> Vec<u8> {
    let entries: Vec<String> = fingerprints
        .iter()
        .map(|fp| {
            format!(
                r#"{{"algorithm":{},"digest":{}}}"#,
                Value::from(fp.algorithm.as_str()),
                Value::from(fp.digest.as_str())
            )
        })
        .collect();
    format!(r#"{{"fingerprint":[{}]}}"#, entries.join(",")).into_bytes()
}

#[derive(Serialize, Deserialize)]
struct IdentityData {
    /// JSON-encoded [`Assertion`], nested as a string as RFC 8827 requires.
    assertion: String,
    idp: IdentityProvider,
}

#[derive(Serialize, Deserialize)]
struct IdentityProvider {
    domain: String,
    protocol: String,
}

#[derive(Serialize, Deserialize)]
struct Assertion {
    fingerprints: String,
    token: String,
}

#[derive(Serialize, Deserialize)]
struct JwsHeader {
    alg: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    x5u: Option<String>,
}

#[derive(Serialize)]
struct ServerClaims<'a> {
    cpk: &'a Jwk,
    exp: u64,
    iat: u64,
}

/// An EC public key as a JSON Web Key (RFC 7517).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Jwk {
    crv: String,
    kty: String,
    x: String,
    y: String,
}

impl Jwk {
    fn from_key(key: &VerifyingKey) -> Result<Self, IdentityError> {
        let point = key.to_sec1_point(false);
        let (Some(x), Some(y)) = (point.x(), point.y()) else {
            return Err(IdentityError::InvalidKey(
                "public key is the identity point".into(),
            ));
        };
        Ok(Self {
            crv: "P-384".into(),
            kty: "EC".into(),
            x: URL_SAFE_NO_PAD.encode(x),
            y: URL_SAFE_NO_PAD.encode(y),
        })
    }

    fn to_key(&self) -> Result<VerifyingKey, IdentityError> {
        if self.kty != "EC" || self.crv != "P-384" {
            return Err(IdentityError::Malformed(format!(
                "unsupported key type {} {}",
                self.kty, self.crv
            )));
        }
        let x = URL_SAFE_NO_PAD_INDIFFERENT.decode(&self.x)?;
        let y = URL_SAFE_NO_PAD_INDIFFERENT.decode(&self.y)?;
        if x.len() != P384_COORDINATE_LEN || y.len() != P384_COORDINATE_LEN {
            return Err(IdentityError::Malformed(
                "wrong P-384 coordinate length".into(),
            ));
        }
        let mut sec1 = Vec::with_capacity(1 + 2 * P384_COORDINATE_LEN);
        sec1.push(0x04);
        sec1.extend_from_slice(&x);
        sec1.extend_from_slice(&y);
        VerifyingKey::from_sec1_bytes(&sec1)
            .map_err(|err| IdentityError::Malformed(err.to_string()))
    }
}

fn encode_identity(
    domain: &str,
    token: String,
    fingerprints: String,
) -> Result<String, IdentityError> {
    let assertion = serde_json::to_string(&Assertion {
        fingerprints,
        token,
    })?;
    let data = serde_json::to_vec(&IdentityData {
        assertion,
        idp: IdentityProvider {
            domain: domain.to_owned(),
            protocol: IDP_PROTOCOL.to_owned(),
        },
    })?;
    Ok(STANDARD.encode(data))
}

/// Extracts the `cpk` public key from a JWT's claims without verifying the JWT.
fn token_public_key(token: &str) -> Result<VerifyingKey, IdentityError> {
    let [_, claims, _] = token.split('.').collect::<Vec<_>>()[..] else {
        return Err(IdentityError::Malformed(
            "token is not a compact JWT".into(),
        ));
    };
    let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD_INDIFFERENT.decode(claims)?)?;
    let cpk = claims
        .get("cpk")
        .ok_or_else(|| IdentityError::Malformed("token has no cpk claim".into()))?;
    parse_public_key(cpk)
}

/// Decodes a `cpk` claim: a JWK object (SDP assertions from 26.40+ clients) or a
/// base64 SPKI DER string (older clients and Login packet tokens).
pub fn parse_public_key(cpk: &Value) -> Result<VerifyingKey, IdentityError> {
    match cpk {
        Value::Object(_) => Jwk::deserialize(cpk)?.to_key(),
        Value::String(der) => {
            VerifyingKey::from_public_key_der(&STANDARD_PAD_INDIFFERENT.decode(der)?)
                .map_err(|err| IdentityError::Malformed(err.to_string()))
        }
        _ => Err(IdentityError::Malformed(
            "cpk is neither a JWK nor a base64 key".into(),
        )),
    }
}

/// ES384-signs `header.payload`, returning the signing input and the base64url signature.
fn sign(key: &SigningKey, header: &[u8], payload: &[u8]) -> (String, String) {
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header),
        URL_SAFE_NO_PAD.encode(payload)
    );
    let signature: Signature = key.sign(signing_input.as_bytes());
    (signing_input, URL_SAFE_NO_PAD.encode(signature.to_bytes()))
}

/// Detached compact JWS (RFC 7515 Appendix F): `header..signature`.
fn detached_jws(key: &SigningKey, payload: &[u8]) -> String {
    let header = br#"{"alg":"ES384"}"#;
    let (_, signature) = sign(key, header, payload);
    format!("{}..{signature}", URL_SAFE_NO_PAD.encode(header))
}

fn verify_detached(jws: &str, payload: &[u8], key: &VerifyingKey) -> Result<(), IdentityError> {
    let (header_b64, signature_b64) = jws
        .split_once("..")
        .ok_or_else(|| IdentityError::Malformed("fingerprints are not a detached JWS".into()))?;
    let header: JwsHeader =
        serde_json::from_slice(&URL_SAFE_NO_PAD_INDIFFERENT.decode(header_b64)?)?;
    if header.alg != ES384 {
        return Err(IdentityError::Malformed(format!(
            "unsupported JWS algorithm {:?}",
            header.alg
        )));
    }
    let signature = Signature::from_slice(&URL_SAFE_NO_PAD_INDIFFERENT.decode(signature_b64)?)
        .map_err(|_| IdentityError::BadSignature)?;
    let signing_input = format!("{header_b64}.{}", URL_SAFE_NO_PAD.encode(payload));
    key.verify(signing_input.as_bytes(), &signature)
        .map_err(|_| IdentityError::BadSignature)
}

/// Writes a new file readable only by its owner where the platform supports it.
fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprints(digest: &str) -> Vec<SdpFingerprint> {
        vec![SdpFingerprint {
            algorithm: "sha-256".into(),
            digest: digest.into(),
        }]
    }

    fn decode_json(part: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn fingerprint_payload_is_canonical_json() {
        let payload = fingerprint_payload(&[
            SdpFingerprint {
                algorithm: "sha-256".into(),
                digest: "AA:BB".into(),
            },
            SdpFingerprint {
                algorithm: "sha-1".into(),
                digest: "CC".into(),
            },
        ]);
        assert_eq!(
            String::from_utf8(payload).unwrap(),
            r#"{"fingerprint":[{"algorithm":"sha-256","digest":"AA:BB"},{"algorithm":"sha-1","digest":"CC"}]}"#
        );
    }

    #[test]
    fn assertion_verifies_only_for_signed_fingerprints() {
        let identity = ServerIdentity::generate(SELF_DOMAIN).unwrap();
        let attribute = identity.assertion(&fingerprints("AA:BB")).unwrap();

        let client = verify_client(&attribute, &fingerprints("AA:BB")).unwrap();
        assert_eq!(client.issuer, SELF_DOMAIN);
        assert_eq!(&client.public_key, identity.verifying_key());

        assert!(matches!(
            verify_client(&attribute, &fingerprints("AA:BC")),
            Err(IdentityError::BadSignature)
        ));
    }

    #[test]
    fn token_has_the_shape_bds_sends() {
        let identity = ServerIdentity::generate(SELF_DOMAIN).unwrap();
        let token = identity.token(SystemTime::now()).unwrap();
        let [header, claims, signature] = token.split('.').collect::<Vec<_>>()[..] else {
            panic!("not a compact JWT: {token}");
        };

        let header = decode_json(header);
        assert_eq!(header["alg"], ES384);
        let x5u = STANDARD.decode(header["x5u"].as_str().unwrap()).unwrap();
        assert_eq!(
            &VerifyingKey::from_public_key_der(&x5u).unwrap(),
            identity.verifying_key()
        );

        let claims = decode_json(claims);
        assert_eq!(claims["cpk"]["kty"], "EC");
        assert_eq!(claims["cpk"]["crv"], "P-384");
        assert_eq!(
            claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
            TOKEN_LIFETIME.as_secs()
        );
        assert_eq!(&token_public_key(&token).unwrap(), identity.verifying_key());

        let signature = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).unwrap()).unwrap();
        let signing_input = token.rsplit_once('.').unwrap().0;
        identity
            .verifying_key()
            .verify(signing_input.as_bytes(), &signature)
            .unwrap();
    }

    #[test]
    fn accepts_pre_26_40_der_public_keys() {
        let client_key = SigningKey::try_generate().unwrap();
        let der = client_key.verifying_key().to_public_key_der().unwrap();
        let claims = serde_json::json!({ "cpk": STANDARD.encode(der.as_bytes()) });
        let (signing_input, signature) = sign(
            &client_key,
            br#"{"alg":"ES384"}"#,
            &serde_json::to_vec(&claims).unwrap(),
        );
        let token = format!("{signing_input}.{signature}");
        let fps = fingerprints("01:02");
        let attribute = encode_identity(
            "https://authorization.franchise.minecraft-services.net",
            token,
            detached_jws(&client_key, &fingerprint_payload(&fps)),
        )
        .unwrap();

        let client = verify_client(&attribute, &fps).unwrap();
        assert_eq!(&client.public_key, client_key.verifying_key());
    }

    #[test]
    fn rejects_unsupported_jws_algorithms() {
        let identity = ServerIdentity::generate(SELF_DOMAIN).unwrap();
        let payload = fingerprint_payload(&fingerprints("AA"));
        let forged = format!(
            "{}..{}",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#),
            URL_SAFE_NO_PAD.encode(b"x")
        );
        assert!(matches!(
            verify_detached(&forged, &payload, identity.verifying_key()),
            Err(IdentityError::Malformed(_))
        ));
    }

    #[test]
    fn key_persists_across_loads() {
        let dir = std::env::temp_dir().join(format!("bedrockrs-identity-{}", std::process::id()));
        let path = dir.join("keys").join("identity.pem");
        let _ = fs::remove_dir_all(&dir);

        let created = ServerIdentity::load_or_create(&path, SELF_DOMAIN).unwrap();
        assert!(path.exists());
        let loaded = ServerIdentity::load_or_create(&path, SELF_DOMAIN).unwrap();
        assert_eq!(created.verifying_key(), loaded.verifying_key());
        assert_eq!(created.key_fingerprint(), loaded.key_fingerprint());

        fs::remove_dir_all(&dir).unwrap();
    }
}
