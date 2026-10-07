//! The connection request inside a Login packet.
//!
//! The request holds two blobs, each prefixed with a little-endian u32 length:
//! authentication JSON (`AuthenticationType`, `Certificate`, `Token`) and a JWT
//! with the client's settings. The multiplayer `Token` from the Minecraft
//! services carries the player's identity (`xid`, `xname`) and key (`cpk`).
//! Nothing here verifies signatures.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD_INDIFFERENT;
use md5::{Digest as _, Md5};
use serde_json::{Value, json};
use uuid::{Builder, Uuid};

use crate::io::{DecodeError, Reader, Writer};

/// Errors from parsing a connection request.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error("invalid login JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("malformed login: {0}")]
    Malformed(&'static str),
}

/// A Login packet's connection request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionRequest {
    pub authentication_type: u8,
    /// The legacy Xbox Live certificate chain of JWTs.
    pub chain: Vec<String>,
    /// The Minecraft services multiplayer token (a JWT); empty if absent.
    pub token: String,
    /// JWT with the client's settings, skin and device information.
    pub client_data: String,
}

/// Who the player claims to be, read without verifying any signature.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityClaims {
    pub xuid: Option<String>,
    pub display_name: Option<String>,
    /// The player's persistent UUID, which stays the same across sessions and
    /// name changes. Derived as vanilla does; see [`identity_from_xuid`].
    pub identity: Option<Uuid>,
    /// The player's public key: a base64 SPKI DER string or a JWK object.
    pub public_key: Option<Value>,
}

impl ConnectionRequest {
    pub fn parse(data: &[u8]) -> Result<Self, LoginError> {
        let mut reader = Reader::new(data);
        let auth_len = reader.u32_le()?;
        let auth: Value = serde_json::from_slice(reader.take(auth_len as usize)?)?;
        let client_data_len = reader.u32_le()?;
        let client_data = std::str::from_utf8(reader.take(client_data_len as usize)?)
            .map_err(|_| DecodeError::InvalidUtf8)?
            .to_owned();

        // `Certificate` is JSON inside a string; older clients sent `chain` at the top level.
        let certificate = match auth.get("Certificate").and_then(Value::as_str) {
            Some(certificate) => serde_json::from_str(certificate)?,
            None => auth.clone(),
        };
        let chain = certificate
            .get("chain")
            .and_then(Value::as_array)
            .map(|chain| {
                chain
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        Ok(Self {
            authentication_type: auth
                .get("AuthenticationType")
                .and_then(Value::as_u64)
                .and_then(|kind| u8::try_from(kind).ok())
                .unwrap_or_default(),
            chain,
            token: auth
                .get("Token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            client_data,
        })
    }

    /// Encodes the request as a client would, e.g. for tests and tools.
    pub fn encode(&self) -> Vec<u8> {
        let certificate = json!({ "chain": self.chain }).to_string();
        let auth = json!({
            "AuthenticationType": self.authentication_type,
            "Certificate": certificate,
            "Token": self.token,
        })
        .to_string();
        let mut writer = Writer::new();
        writer.u32_le(len_u32(auth.len()));
        writer.raw(auth.as_bytes());
        writer.u32_le(len_u32(self.client_data.len()));
        writer.raw(self.client_data.as_bytes());
        writer.into_bytes()
    }

    /// The player's claimed identity: from the multiplayer token, or, without one,
    /// from the last certificate in the legacy chain.
    pub fn identity(&self) -> Result<IdentityClaims, LoginError> {
        if !self.token.is_empty() {
            return Ok(IdentityClaims::from_token_claims(&jwt_claims(&self.token)?));
        }
        let certificate = self
            .chain
            .last()
            .ok_or(LoginError::Malformed("no token and no certificate chain"))?;
        let claims = jwt_claims(certificate)?;
        let extra = claims.get("extraData").unwrap_or(&Value::Null);
        Ok(IdentityClaims {
            xuid: text_claim(extra, "XUID"),
            display_name: text_claim(extra, "displayName"),
            identity: uuid_claim(extra, "identity"),
            public_key: claims.get("identityPublicKey").cloned(),
        })
    }
}

/// The UUID vanilla gives the player with this XUID: the MD5 (version 3) UUID
/// of `pocket-auth-1-xuid:` followed by the XUID, as gophertunnel derives it.
pub fn identity_from_xuid(xuid: &str) -> Uuid {
    let digest = Md5::new()
        .chain_update(b"pocket-auth-1-xuid:")
        .chain_update(xuid.as_bytes())
        .finalize();
    Builder::from_md5_bytes(digest.into()).into_uuid()
}

/// Decodes a JWT's claims without checking its signature.
fn jwt_claims(token: &str) -> Result<Value, LoginError> {
    Ok(Jwt::parse(token)?.claims)
}

impl IdentityClaims {
    /// The identity in a multiplayer token's claims: `xid`, `xname`, `cpk`,
    /// and the UUID (the offline `leguuid`, or else derived from the XUID).
    pub fn from_token_claims(claims: &Value) -> Self {
        let xuid = text_claim(claims, "xid");
        let identity =
            uuid_claim(claims, "leguuid").or_else(|| xuid.as_deref().map(identity_from_xuid));
        Self {
            xuid,
            display_name: text_claim(claims, "xname"),
            identity,
            public_key: claims.get("cpk").cloned(),
        }
    }
}

/// A compact JWS/JWT taken apart for checking its signature. Parsing verifies
/// nothing: the signature covers `signing_input` and must be checked by the
/// caller before trusting `claims`.
#[derive(Debug, Clone, PartialEq)]
pub struct Jwt {
    pub header: Value,
    pub claims: Value,
    /// `base64url(header).base64url(claims)`, the bytes the signature covers.
    pub signing_input: String,
    pub signature: Vec<u8>,
}

impl Jwt {
    pub fn parse(token: &str) -> Result<Self, LoginError> {
        let mut parts = token.split('.');
        let (Some(header), Some(claims), Some(signature), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(LoginError::Malformed("not a compact JWT"));
        };
        let decode = |part: &str, what: &'static str| {
            URL_SAFE_NO_PAD_INDIFFERENT
                .decode(part)
                .map_err(|_| LoginError::Malformed(what))
        };
        Ok(Self {
            header: serde_json::from_slice(&decode(header, "JWT header is not base64url")?)?,
            claims: serde_json::from_slice(&decode(claims, "JWT claims are not base64url")?)?,
            signing_input: format!("{header}.{claims}"),
            signature: decode(signature, "JWT signature is not base64url")?,
        })
    }

    /// A text field of the header, such as `alg` or `kid`.
    pub fn header_field(&self, name: &str) -> Option<&str> {
        self.header.get(name).and_then(Value::as_str)
    }
}

fn text_claim(claims: &Value, name: &str) -> Option<String> {
    claims
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn uuid_claim(claims: &Value, name: &str) -> Option<Uuid> {
    text_claim(claims, name).and_then(|value| Uuid::parse_str(&value).ok())
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("login blobs are shorter than 4 GiB")
}

#[cfg(test)]
mod tests {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::*;

    fn unsigned_jwt(claims: Value) -> String {
        format!(
            "{}.{}.",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"ES384"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    #[test]
    fn parses_what_it_encodes() {
        let request = ConnectionRequest {
            authentication_type: 0,
            chain: vec!["a.b.c".into()],
            token: "x.y.z".into(),
            client_data: "client.data.jwt".into(),
        };
        assert_eq!(
            ConnectionRequest::parse(&request.encode()).unwrap(),
            request
        );
    }

    #[test]
    fn accepts_the_legacy_top_level_chain() {
        let auth = br#"{"chain":["one","two"]}"#;
        let mut data = Vec::new();
        data.extend((auth.len() as u32).to_le_bytes());
        data.extend(auth);
        data.extend(3u32.to_le_bytes());
        data.extend(b"jwt");
        let request = ConnectionRequest::parse(&data).unwrap();
        assert_eq!(request.chain, ["one", "two"]);
        assert_eq!(request.token, "");
        assert_eq!(request.client_data, "jwt");
    }

    #[test]
    fn identity_comes_from_the_multiplayer_token() {
        let request = ConnectionRequest {
            authentication_type: 0,
            chain: Vec::new(),
            token: unsigned_jwt(
                json!({ "xid": "2535400000000000", "xname": "Steve", "cpk": "MHYw" }),
            ),
            client_data: String::new(),
        };
        assert_eq!(
            request.identity().unwrap(),
            IdentityClaims {
                xuid: Some("2535400000000000".into()),
                display_name: Some("Steve".into()),
                identity: Some(identity_from_xuid("2535400000000000")),
                public_key: Some(json!("MHYw")),
            }
        );
    }

    #[test]
    fn identity_uuids_derive_from_the_xuid_like_vanilla() {
        // MD5 of "pocket-auth-1-xuid:2535400000000000" with the version 3 and
        // RFC 4122 variant bits set, computed independently with .NET's MD5.
        assert_eq!(
            identity_from_xuid("2535400000000000").to_string(),
            "174319cc-f69f-30d8-a279-6ace57f2011e"
        );
    }

    #[test]
    fn offline_tokens_carry_their_own_identity() {
        let request = ConnectionRequest {
            authentication_type: 0,
            chain: Vec::new(),
            token: unsigned_jwt(json!({
                "xname": "Guest",
                "leguuid": "01234567-89ab-4cde-8f01-23456789abcd"
            })),
            client_data: String::new(),
        };
        let claims = request.identity().unwrap();
        assert_eq!(claims.xuid, None);
        assert_eq!(
            claims.identity.map(|uuid| uuid.to_string()).as_deref(),
            Some("01234567-89ab-4cde-8f01-23456789abcd")
        );
    }

    #[test]
    fn identity_falls_back_to_the_legacy_chain() {
        let request = ConnectionRequest {
            authentication_type: 0,
            chain: vec![unsigned_jwt(json!({
                "identityPublicKey": "MHYw",
                "extraData": {
                    "XUID": "",
                    "displayName": "Alex",
                    "identity": "01234567-89ab-4cde-8f01-23456789abcd"
                }
            }))],
            token: String::new(),
            client_data: String::new(),
        };
        assert_eq!(
            request.identity().unwrap(),
            IdentityClaims {
                xuid: None,
                display_name: Some("Alex".into()),
                identity: Uuid::parse_str("01234567-89ab-4cde-8f01-23456789abcd").ok(),
                public_key: Some(json!("MHYw")),
            }
        );
    }

    #[test]
    fn rejects_truncated_requests() {
        assert!(matches!(
            ConnectionRequest::parse(&[10, 0, 0, 0, b'{']),
            Err(LoginError::Decode(DecodeError::UnexpectedEnd))
        ));
    }
}
