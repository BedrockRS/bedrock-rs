//! Minimal SDP handling for NetherNet's data-channel-only sessions.
//!
//! A NetherNet offer carries exactly one
//! `m=application … UDP/DTLS/SCTP webrtc-datachannel` section. We read the few
//! attributes the session needs and write answers in the same layout as
//! go-nethernet, which interoperates with the vanilla client
//! (`docs/ARCHITECTURE.md` §3.3).

use std::net::SocketAddr;

/// `a=max-message-size` we advertise: str0m accepts SCTP messages up to 256 KiB.
pub const MAX_MESSAGE_SIZE: u32 = 256 * 1024;

const SCTP_PORT: u16 = 5000;

/// DTLS role negotiation (`a=setup`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setup {
    Active,
    Passive,
    ActPass,
}

impl Setup {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "passive" => Some(Self::Passive),
            "actpass" => Some(Self::ActPass),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Passive => "passive",
            Self::ActPass => "actpass",
        }
    }
}

/// A certificate fingerprint exactly as written in an `a=fingerprint` line.
///
/// The text is kept verbatim because identity assertions sign it byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdpFingerprint {
    pub algorithm: String,
    pub digest: String,
}

/// Errors from parsing an SDP offer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SdpError {
    #[error("expected exactly one media section, found {0}")]
    MediaCount(usize),
    #[error("unsupported media section: m={0}")]
    UnsupportedMedia(String),
    #[error("missing a={0} attribute")]
    Missing(&'static str),
    #[error("invalid a={name} attribute: {value}")]
    Invalid { name: &'static str, value: String },
}

/// The parts of a client's SDP offer that NetherNet uses.
#[derive(Debug, Clone)]
pub struct Offer {
    pub ice_ufrag: String,
    pub ice_pwd: String,
    pub fingerprints: Vec<SdpFingerprint>,
    pub setup: Setup,
    pub mid: String,
    /// The client's `a=max-message-size`, which bounds the messages we may send it.
    pub max_message_size: Option<u32>,
    /// Inline ICE candidates, each starting with `candidate:`.
    pub candidates: Vec<String>,
    /// Raw base64 value of the session-level `a=identity` attribute.
    pub identity: Option<String>,
}

impl Offer {
    pub fn parse(sdp: &str) -> Result<Self, SdpError> {
        let mut session = Attributes::default();
        let mut media = Attributes::default();
        let mut media_lines = Vec::new();
        for line in sdp.lines().map(str::trim) {
            if let Some(m) = line.strip_prefix("m=") {
                media_lines.push(m);
            } else if let Some(attribute) = line.strip_prefix("a=") {
                let (key, value) = attribute.split_once(':').unwrap_or((attribute, ""));
                let section = if media_lines.is_empty() {
                    &mut session
                } else {
                    &mut media
                };
                section.0.push((key, value));
            }
        }

        let [media_line] = media_lines[..] else {
            return Err(SdpError::MediaCount(media_lines.len()));
        };
        let fields: Vec<&str> = media_line.split_whitespace().collect();
        if !matches!(
            fields[..],
            ["application", _, "UDP/DTLS/SCTP", "webrtc-datachannel", ..]
        ) {
            return Err(SdpError::UnsupportedMedia(media_line.to_owned()));
        }

        let either = |key| media.get(key).or_else(|| session.get(key));
        let required = |key: &'static str| either(key).ok_or(SdpError::Missing(key));

        let fingerprint_lines = if media.has("fingerprint") {
            media.all("fingerprint")
        } else {
            session.all("fingerprint")
        };
        let fingerprints = fingerprint_lines
            .map(|value| {
                let (algorithm, digest) =
                    value.split_once(' ').ok_or_else(|| SdpError::Invalid {
                        name: "fingerprint",
                        value: value.to_owned(),
                    })?;
                Ok(SdpFingerprint {
                    algorithm: algorithm.to_owned(),
                    digest: digest.trim().to_owned(),
                })
            })
            .collect::<Result<Vec<_>, SdpError>>()?;
        if fingerprints.is_empty() {
            return Err(SdpError::Missing("fingerprint"));
        }

        let setup = required("setup")?;
        let setup = Setup::parse(setup).ok_or_else(|| SdpError::Invalid {
            name: "setup",
            value: setup.to_owned(),
        })?;

        let max_message_size = media
            .get("max-message-size")
            .map(|value| match value.parse::<u32>() {
                Ok(size) if size > 1 => Ok(size),
                _ => Err(SdpError::Invalid {
                    name: "max-message-size",
                    value: value.to_owned(),
                }),
            })
            .transpose()?;

        Ok(Self {
            ice_ufrag: required("ice-ufrag")?.to_owned(),
            ice_pwd: required("ice-pwd")?.to_owned(),
            fingerprints,
            setup,
            mid: media.get("mid").unwrap_or("0").to_owned(),
            max_message_size,
            candidates: session
                .all("candidate")
                .chain(media.all("candidate"))
                .map(|value| format!("candidate:{value}"))
                .collect(),
            identity: session.get("identity").map(str::to_owned),
        })
    }
}

/// Attribute lines of one SDP section, in order.
#[derive(Default)]
struct Attributes<'a>(Vec<(&'a str, &'a str)>);

impl<'a> Attributes<'a> {
    fn get(&self, key: &str) -> Option<&'a str> {
        self.all(key).next()
    }

    fn has(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    fn all(&self, key: &str) -> impl Iterator<Item = &'a str> {
        self.0
            .iter()
            .filter(move |(k, _)| *k == key)
            .map(|&(_, value)| value)
    }
}

/// Everything needed to write the server's SDP answer.
#[derive(Debug)]
pub struct Answer<'a> {
    pub session_id: u64,
    /// Announce ICE-lite (`a=ice-lite`): we only answer connectivity checks.
    pub ice_lite: bool,
    /// Base64 `a=identity` value; vanilla clients reject answers without one.
    pub identity: &'a str,
    pub ice_ufrag: &'a str,
    pub ice_pwd: &'a str,
    pub fingerprint: &'a SdpFingerprint,
    pub setup: Setup,
    pub mid: &'a str,
    /// Complete candidate attribute values, each starting with `candidate:`.
    pub candidates: &'a [String],
    /// Address advertised on the `m=` and `c=` lines, as libwebrtc does.
    pub default_address: Option<SocketAddr>,
}

impl Answer<'_> {
    /// Renders the answer with CRLF line endings.
    pub fn to_sdp(&self) -> String {
        let (port, address_type, address) = match self.default_address {
            Some(addr) => (
                addr.port(),
                if addr.is_ipv4() { "IP4" } else { "IP6" },
                addr.ip().to_string(),
            ),
            None => (9, "IP4", "0.0.0.0".to_owned()),
        };

        let mut lines = vec![
            "v=0".to_owned(),
            // RFC 3264 requires the session id to fit a signed 64-bit integer.
            format!(
                "o=- {} 2 IN IP4 127.0.0.1",
                self.session_id & i64::MAX as u64
            ),
            "s=-".to_owned(),
            "t=0 0".to_owned(),
        ];
        if self.ice_lite {
            lines.push("a=ice-lite".to_owned());
        }
        lines.extend([
            format!("a=group:BUNDLE {}", self.mid),
            "a=extmap-allow-mixed".to_owned(),
            "a=msid-semantic: WMS".to_owned(),
            format!("a=identity:{}", self.identity),
            format!("m=application {port} UDP/DTLS/SCTP webrtc-datachannel"),
            format!("c=IN {address_type} {address}"),
        ]);
        lines.extend(self.candidates.iter().map(|c| format!("a={c}")));
        lines.extend([
            format!("a=ice-ufrag:{}", self.ice_ufrag),
            format!("a=ice-pwd:{}", self.ice_pwd),
            // go-nethernet always includes this, even for non-trickle answers.
            "a=ice-options:trickle".to_owned(),
            format!(
                "a=fingerprint:{} {}",
                self.fingerprint.algorithm, self.fingerprint.digest
            ),
            format!("a=setup:{}", self.setup.as_str()),
            format!("a=mid:{}", self.mid),
            format!("a=sctp-port:{SCTP_PORT}"),
            format!("a=max-message-size:{MAX_MESSAGE_SIZE}"),
        ]);

        let mut sdp = lines.join("\r\n");
        sdp.push_str("\r\n");
        sdp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shape of a vanilla offer, based on the example in Mojang's onboarding guide.
    const OFFER: &str = "v=0\r\n\
        o=- 123456789 2 IN IP4 127.0.0.1\r\n\
        s=-\r\n\
        t=0 0\r\n\
        a=group:BUNDLE 0\r\n\
        a=extmap-allow-mixed\r\n\
        a=msid-semantic: WMS\r\n\
        a=identity:eyJpZHAiOnt9fQ==\r\n\
        m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
        c=IN IP4 0.0.0.0\r\n\
        a=ice-ufrag:abcd\r\n\
        a=ice-pwd:abcdefghijklmnopqrstuvwx\r\n\
        a=ice-options:trickle\r\n\
        a=fingerprint:sha-256 AA:BB:CC:DD\r\n\
        a=setup:actpass\r\n\
        a=mid:0\r\n\
        a=sctp-port:5000\r\n\
        a=max-message-size:262144\r\n\
        a=candidate:1 1 udp 2130706431 192.168.1.100 12345 typ host generation 0 ufrag abcd network-id 1 network-cost 10\r\n\
        a=candidate:2 1 udp 1694498815 203.0.113.50 54321 typ srflx raddr 192.168.1.100 rport 12345\r\n\
        a=end-of-candidates\r\n";

    #[test]
    fn parses_vanilla_offer() {
        let offer = Offer::parse(OFFER).unwrap();
        assert_eq!(offer.ice_ufrag, "abcd");
        assert_eq!(offer.ice_pwd, "abcdefghijklmnopqrstuvwx");
        assert_eq!(
            offer.fingerprints,
            vec![SdpFingerprint {
                algorithm: "sha-256".into(),
                digest: "AA:BB:CC:DD".into()
            }]
        );
        assert_eq!(offer.setup, Setup::ActPass);
        assert_eq!(offer.mid, "0");
        assert_eq!(offer.max_message_size, Some(262_144));
        assert_eq!(offer.candidates.len(), 2);
        assert!(
            offer.candidates[0].starts_with("candidate:1 1 udp 2130706431 192.168.1.100 12345")
        );
        assert_eq!(offer.identity.as_deref(), Some("eyJpZHAiOnt9fQ=="));
    }

    #[test]
    fn accepts_session_level_ice_and_fingerprint_and_bare_newlines() {
        let offer = Offer::parse(
            "v=0\na=ice-ufrag:u\na=ice-pwd:p\na=fingerprint:sha-256 01:02\n\
             m=application 9 UDP/DTLS/SCTP webrtc-datachannel\na=setup:active\n",
        )
        .unwrap();
        assert_eq!(
            (offer.ice_ufrag.as_str(), offer.ice_pwd.as_str()),
            ("u", "p")
        );
        assert_eq!(offer.fingerprints[0].digest, "01:02");
        assert_eq!(offer.setup, Setup::Active);
        assert_eq!(offer.max_message_size, None);
        assert_eq!(offer.identity, None);
    }

    #[test]
    fn rejects_offers_without_required_attributes() {
        let without_ufrag = OFFER.replace("a=ice-ufrag:abcd\r\n", "");
        assert_eq!(
            Offer::parse(&without_ufrag).unwrap_err(),
            SdpError::Missing("ice-ufrag")
        );
        let without_fingerprint = OFFER.replace("a=fingerprint:sha-256 AA:BB:CC:DD\r\n", "");
        assert_eq!(
            Offer::parse(&without_fingerprint).unwrap_err(),
            SdpError::Missing("fingerprint")
        );
        let bad_size = OFFER.replace("max-message-size:262144", "max-message-size:1");
        assert!(matches!(
            Offer::parse(&bad_size).unwrap_err(),
            SdpError::Invalid {
                name: "max-message-size",
                ..
            }
        ));
    }

    #[test]
    fn rejects_media_other_than_one_data_channel_section() {
        let audio = OFFER.replace(
            "m=application 9 UDP/DTLS/SCTP webrtc-datachannel",
            "m=audio 9 UDP/TLS/RTP/SAVPF 111",
        );
        assert!(matches!(
            Offer::parse(&audio).unwrap_err(),
            SdpError::UnsupportedMedia(_)
        ));
        assert_eq!(
            Offer::parse("v=0\r\n").unwrap_err(),
            SdpError::MediaCount(0)
        );
    }

    #[test]
    fn writes_answer_in_go_nethernet_layout() {
        let fingerprint = SdpFingerprint {
            algorithm: "sha-256".into(),
            digest: "11:22".into(),
        };
        let candidates = vec!["candidate:1 1 udp 2130706431 10.0.0.5 19133 typ host".to_owned()];
        let sdp = Answer {
            session_id: u64::MAX,
            ice_lite: true,
            identity: "SURFTlRJVFk=",
            ice_ufrag: "wxyz",
            ice_pwd: "secret",
            fingerprint: &fingerprint,
            setup: Setup::Active,
            mid: "0",
            candidates: &candidates,
            default_address: Some("10.0.0.5:19133".parse().unwrap()),
        }
        .to_sdp();

        let expected = [
            "v=0",
            "o=- 9223372036854775807 2 IN IP4 127.0.0.1",
            "s=-",
            "t=0 0",
            "a=ice-lite",
            "a=group:BUNDLE 0",
            "a=extmap-allow-mixed",
            "a=msid-semantic: WMS",
            "a=identity:SURFTlRJVFk=",
            "m=application 19133 UDP/DTLS/SCTP webrtc-datachannel",
            "c=IN IP4 10.0.0.5",
            "a=candidate:1 1 udp 2130706431 10.0.0.5 19133 typ host",
            "a=ice-ufrag:wxyz",
            "a=ice-pwd:secret",
            "a=ice-options:trickle",
            "a=fingerprint:sha-256 11:22",
            "a=setup:active",
            "a=mid:0",
            "a=sctp-port:5000",
            "a=max-message-size:262144",
        ];
        assert_eq!(sdp, expected.join("\r\n") + "\r\n");
    }
}
