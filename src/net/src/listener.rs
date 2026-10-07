//! The NetherNet listener: HTTP signaling, shared UDP sockets and session setup.

use std::hash::{BuildHasher, RandomState};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use str0m::config::{CryptoProvider, DtlsCert, Fingerprint};
use str0m::{Candidate, IceCreds, RtcConfig};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::identity::{self, IdentityError, ServerIdentity};
use crate::mux::{PeerRoute, UdpMux};
use crate::peer::{Connection, Peer, PeerSetup};
use crate::sdp::{Answer, Offer, SdpFingerprint, Setup};
use crate::signaling::{self, OfferError, OfferHandler, ServerStatus};

const ACCEPT_QUEUE: usize = 64;
const DATAGRAM_QUEUE: usize = 512;

/// Listener settings. The defaults match a vanilla dedicated server.
#[derive(Debug, Clone)]
pub struct ListenerConfig {
    /// TCP address of the HTTP signaling server. Clients connect to exactly the
    /// host and port a player enters, so this is the server's advertised port.
    pub signaling_addr: SocketAddr,
    /// UDP port for WebRTC traffic, shared by all sessions. 0 picks a free port.
    pub media_port: u16,
    /// Local addresses to bind for WebRTC traffic. Empty means every IPv4
    /// interface that is up, excluding loopback and link-local addresses.
    pub media_ips: Vec<IpAddr>,
    /// Extra public addresses to offer clients, e.g. when behind NAT with the
    /// media port forwarded unchanged.
    pub advertise_ips: Vec<IpAddr>,
    pub identity_path: PathBuf,
    /// Identity-provider domain in our assertions; BDS uses "self".
    pub identity_domain: String,
    /// Accept offers that carry no `a=identity` assertion.
    pub allow_anonymous: bool,
    pub negotiation_timeout: Duration,
    /// How long ICE, DTLS and SCTP may take before the client's channels open.
    pub connect_timeout: Duration,
    /// Largest message accepted from a client after reassembly.
    pub max_message_size: usize,
    /// Most simultaneous sessions, including ones still connecting.
    pub max_sessions: usize,
    /// Run ICE-lite, the usual mode for WebRTC servers: never probe candidate pairs,
    /// only answer the client's checks, so every datagram leaves through the socket
    /// that received the client's traffic. Full ICE also probes pairs the OS may be
    /// unable to route, such as virtual adapters.
    pub ice_lite: bool,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            signaling_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 19132),
            media_port: 19133,
            media_ips: Vec::new(),
            advertise_ips: Vec::new(),
            identity_path: PathBuf::from("keys/identity.pem"),
            identity_domain: identity::SELF_DOMAIN.to_owned(),
            allow_anonymous: false,
            negotiation_timeout: Duration::from_secs(15),
            connect_timeout: Duration::from_secs(20),
            max_message_size: 8 * 1024 * 1024,
            max_sessions: 1024,
            ice_lite: true,
        }
    }
}

/// Errors starting a [`Listener`].
#[derive(Debug, thiserror::Error)]
pub enum ListenerError {
    #[error(transparent)]
    Identity(#[from] IdentityError),
    #[error("failed to list network interfaces: {0}")]
    Interfaces(io::Error),
    #[error("no usable IPv4 interface found; configure the media addresses explicitly")]
    NoMediaAddress,
    #[error("failed to bind {what} on {addr}: {source}")]
    Bind {
        what: &'static str,
        addr: String,
        source: io::Error,
    },
    #[error("cannot offer {addr} as an ICE candidate: {reason}")]
    Candidate { addr: SocketAddr, reason: String },
    #[error("the crypto provider cannot generate a DTLS certificate")]
    DtlsCertificate,
}

/// Accepts NetherNet connections.
///
/// Serves HTTP signaling on TCP, answers each client's offer with a new WebRTC
/// session over shared UDP sockets, and yields a [`Connection`] once the client's
/// reliable data channel is open. Dropping the listener stops accepting new clients.
#[derive(Debug)]
pub struct Listener {
    incoming: mpsc::Receiver<Connection>,
    status: watch::Sender<ServerStatus>,
    signaling_addr: SocketAddr,
    media_addrs: Vec<SocketAddr>,
    key_fingerprint: String,
    tasks: Vec<JoinHandle<()>>,
}

impl Listener {
    /// Loads or creates the identity key, binds the signaling and media sockets
    /// and starts serving. `status` is what `GET /v1/join` reports.
    pub async fn bind(config: ListenerConfig, status: ServerStatus) -> Result<Self, ListenerError> {
        let identity =
            ServerIdentity::load_or_create(&config.identity_path, config.identity_domain.clone())?;
        let key_fingerprint = identity.key_fingerprint();

        let media_ips = if config.media_ips.is_empty() {
            detect_media_ips()?
        } else {
            config.media_ips.clone()
        };
        let mux = UdpMux::bind(&media_ips, config.media_port)
            .await
            .map_err(|source| ListenerError::Bind {
                what: "WebRTC UDP sockets",
                addr: format!("{media_ips:?} port {}", config.media_port),
                source,
            })?;
        let media_addrs = mux.local_addrs();
        let candidates = local_candidates(&media_addrs, &config.advertise_ips, config.ice_lite)?;

        let crypto = Arc::new(str0m::crypto::from_feature_flags());
        let dtls_cert = crypto
            .dtls_provider
            .generate_certificate()
            .ok_or(ListenerError::DtlsCertificate)?;

        let tcp = TcpListener::bind(config.signaling_addr)
            .await
            .map_err(|source| ListenerError::Bind {
                what: "HTTP signaling",
                addr: config.signaling_addr.to_string(),
                source,
            })?;
        let signaling_addr = tcp.local_addr().map_err(|source| ListenerError::Bind {
            what: "HTTP signaling",
            addr: config.signaling_addr.to_string(),
            source,
        })?;

        let (accept, incoming) = mpsc::channel(ACCEPT_QUEUE);
        let (status, status_updates) = watch::channel(status);
        let negotiation_timeout = config.negotiation_timeout;
        let sessions = Arc::new(Sessions {
            identity,
            crypto,
            dtls_cert,
            default_address: default_address(&candidates, media_addrs.len()),
            candidates,
            mux: Arc::clone(&mux),
            accept,
            config,
            next_id: AtomicU64::new(1),
            active: Arc::new(AtomicUsize::new(0)),
            session_ids: RandomState::new(),
        });

        let mut tasks = mux.spawn_readers();
        let router = signaling::router(sessions, status_updates, negotiation_timeout);
        tasks.push(tokio::spawn(async move {
            if let Err(err) = axum::serve(tcp, router).await {
                tracing::error!("The server stopped accepting connections: {err}");
            }
        }));

        Ok(Self {
            incoming,
            status,
            signaling_addr,
            media_addrs,
            key_fingerprint,
            tasks,
        })
    }

    /// Waits for the next client whose reliable data channel has opened.
    pub async fn accept(&mut self) -> Option<Connection> {
        self.incoming.recv().await
    }

    /// Replaces the status served by `GET /v1/join`.
    pub fn set_status(&self, status: ServerStatus) {
        self.status.send_replace(status);
    }

    /// Updates the status served by `GET /v1/join` in place, e.g. the player count.
    pub fn update_status(&self, update: impl FnOnce(&mut ServerStatus)) {
        self.status.send_modify(update);
    }

    pub fn signaling_addr(&self) -> SocketAddr {
        self.signaling_addr
    }

    pub fn media_addrs(&self) -> &[SocketAddr] {
        &self.media_addrs
    }

    /// SHA-256 of the identity public key, the key clients pin on first use.
    pub fn key_fingerprint(&self) -> &str {
        &self.key_fingerprint
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Answers offers by starting a WebRTC session for each.
struct Sessions {
    identity: ServerIdentity,
    crypto: Arc<CryptoProvider>,
    dtls_cert: DtlsCert,
    candidates: Vec<Candidate>,
    default_address: Option<SocketAddr>,
    mux: Arc<UdpMux>,
    accept: mpsc::Sender<Connection>,
    config: ListenerConfig,
    next_id: AtomicU64,
    active: Arc<AtomicUsize>,
    session_ids: RandomState,
}

impl OfferHandler for Sessions {
    async fn handle_offer(&self, network_id: u64, offer: String) -> Result<String, OfferError> {
        self.answer(network_id, &offer)
    }
}

impl Sessions {
    fn answer(&self, network_id: u64, offer: &str) -> Result<String, OfferError> {
        let offer = Offer::parse(offer).map_err(|err| OfferError::BadOffer(err.to_string()))?;
        let identity = match &offer.identity {
            Some(attribute) => Some(
                identity::verify_client(attribute, &offer.fingerprints)
                    .map_err(|err| OfferError::Forbidden(err.to_string()))?,
            ),
            None if self.config.allow_anonymous => None,
            None => {
                return Err(OfferError::Forbidden(
                    "the offer has no identity assertion".into(),
                ));
            }
        };
        if self.active.load(Ordering::Relaxed) >= self.config.max_sessions {
            return Err(OfferError::Unavailable("too many sessions".into()));
        }
        let remote_fingerprint = remote_fingerprint(&offer.fingerprints)?;

        let mut rtc = RtcConfig::new()
            .set_crypto_provider(Arc::clone(&self.crypto))
            .set_dtls_cert(self.dtls_cert.clone())
            .set_ice_lite(self.config.ice_lite)
            .build(Instant::now());
        // The client offers, so it controls ICE. Like go-nethernet we take the
        // DTLS client role unless the offer claims it.
        let dtls_active = offer.setup != Setup::Active;
        let (local_ice, local_fingerprint) = {
            let mut api = rtc.direct_api();
            api.set_ice_controlling(false);
            api.set_remote_ice_credentials(IceCreds {
                ufrag: offer.ice_ufrag.clone(),
                pass: offer.ice_pwd.clone(),
            });
            api.set_remote_fingerprint(remote_fingerprint);
            api.start_dtls(dtls_active)
                .map_err(|err| OfferError::Internal(err.to_string()))?;
            api.start_sctp(dtls_active);
            (
                api.local_ice_credentials(),
                api.local_dtls_fingerprint().to_string(),
            )
        };

        let mut candidate_lines = Vec::with_capacity(self.candidates.len());
        for (network_index, candidate) in self.candidates.iter().enumerate() {
            if let Some(added) = rtc.add_local_candidate(candidate.clone()) {
                candidate_lines.push(candidate_attribute(added, &local_ice.ufrag, network_index));
            }
        }
        for candidate in &offer.candidates {
            match Candidate::from_sdp_string(candidate) {
                Ok(candidate) => rtc.add_remote_candidate(candidate),
                Err(err) => {
                    tracing::debug!(network_id, %err, %candidate, "skipping unusable remote candidate")
                }
            }
        }

        let (algorithm, digest) = local_fingerprint
            .split_once(' ')
            .ok_or_else(|| OfferError::Internal("malformed local DTLS fingerprint".into()))?;
        let fingerprint = SdpFingerprint {
            algorithm: algorithm.to_owned(),
            digest: digest.to_owned(),
        };
        let assertion = self
            .identity
            .assertion(std::slice::from_ref(&fingerprint))
            .map_err(|err| OfferError::Internal(err.to_string()))?;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let answer = Answer {
            session_id: self.session_ids.hash_one(id),
            ice_lite: self.config.ice_lite,
            identity: &assertion,
            ice_ufrag: &local_ice.ufrag,
            ice_pwd: &local_ice.pass,
            fingerprint: &fingerprint,
            setup: if dtls_active {
                Setup::Active
            } else {
                Setup::Passive
            },
            mid: &offer.mid,
            candidates: &candidate_lines,
            default_address: self.default_address,
        }
        .to_sdp();

        let (datagram_tx, datagrams) = mpsc::channel(DATAGRAM_QUEUE);
        let route = PeerRoute {
            id,
            tx: datagram_tx,
        };
        self.mux.register(local_ice.ufrag, route.clone());
        let peer = Peer::new(PeerSetup {
            id,
            network_id,
            rtc,
            identity,
            mux: Arc::clone(&self.mux),
            route,
            datagrams,
            accept: self.accept.clone(),
            remote_max_message_size: offer.max_message_size,
            max_message_size: self.config.max_message_size,
            connect_timeout: self.config.connect_timeout,
        });
        let active = Arc::clone(&self.active);
        active.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            peer.run().await;
            active.fetch_sub(1, Ordering::Relaxed);
        });

        tracing::debug!(network_id, peer = id, "answered NetherNet offer");
        Ok(answer)
    }
}

/// The offer's SHA-256 certificate fingerprint, which DTLS will enforce.
fn remote_fingerprint(fingerprints: &[SdpFingerprint]) -> Result<Fingerprint, OfferError> {
    let fingerprint = fingerprints
        .iter()
        .find(|fp| fp.algorithm.eq_ignore_ascii_case("sha-256"))
        .ok_or_else(|| OfferError::BadOffer("the offer has no sha-256 DTLS fingerprint".into()))?;
    format!("sha-256 {}", fingerprint.digest)
        .parse()
        .map_err(OfferError::BadOffer)
}

/// Formats a local candidate the way libwebrtc and go-nethernet do.
fn candidate_attribute(candidate: &Candidate, ufrag: &str, network_index: usize) -> String {
    let line = candidate.to_sdp_string();
    let line = line
        .split_once(" ufrag ")
        .map_or(line.as_str(), |(head, _)| head);
    format!("{line} generation 0 ufrag {ufrag} network-id {network_index} network-cost 0")
}

fn detect_media_ips() -> Result<Vec<IpAddr>, ListenerError> {
    let mut ips: Vec<IpAddr> = if_addrs::get_if_addrs()
        .map_err(ListenerError::Interfaces)?
        .into_iter()
        .filter(|iface| {
            iface.is_oper_up()
                && !iface.is_loopback()
                && !iface.is_link_local()
                && iface.ip().is_ipv4()
        })
        .map(|iface| iface.ip())
        .collect();
    ips.sort();
    ips.dedup();
    if ips.is_empty() {
        return Err(ListenerError::NoMediaAddress);
    }
    Ok(ips)
}

/// Host candidates for every bound socket, followed by the advertised public addresses.
///
/// Full ICE offers a public address as a server-reflexive candidate based on a
/// local socket. str0m's ICE-lite agent accepts only host candidates, so in lite
/// mode it is announced as a host candidate instead. That works because a lite
/// agent never sends first: each reply leaves through the socket that received
/// the client's check, and the NAT maps it back to the public address.
fn local_candidates(
    media_addrs: &[SocketAddr],
    advertise_ips: &[IpAddr],
    ice_lite: bool,
) -> Result<Vec<Candidate>, ListenerError> {
    let invalid = |addr: SocketAddr| {
        move |err: str0m::error::IceError| ListenerError::Candidate {
            addr,
            reason: err.to_string(),
        }
    };
    let mut candidates = Vec::with_capacity(media_addrs.len() + advertise_ips.len());
    for &addr in media_addrs {
        candidates.push(Candidate::host(addr, "udp").map_err(invalid(addr))?);
    }
    for &ip in advertise_ips {
        let Some(&base) = media_addrs
            .iter()
            .find(|addr| addr.is_ipv4() == ip.is_ipv4())
        else {
            tracing::warn!("Can't advertise {ip}: no network address of the same kind is in use");
            continue;
        };
        let addr = SocketAddr::new(ip, base.port());
        let candidate = if ice_lite {
            Candidate::host(addr, "udp")
        } else {
            Candidate::server_reflexive(addr, base, "udp")
        };
        candidates.push(candidate.map_err(invalid(addr))?);
    }
    Ok(candidates)
}

/// Address for the answer's `m=`/`c=` lines: the first advertised IPv4 address,
/// otherwise the first IPv4 media address. `candidates` lists the `media_count`
/// socket candidates before the advertised ones.
fn default_address(candidates: &[Candidate], media_count: usize) -> Option<SocketAddr> {
    let (media, advertised) = candidates.split_at(media_count.min(candidates.len()));
    let first_ipv4 =
        |list: &[Candidate]| list.iter().map(Candidate::addr).find(SocketAddr::is_ipv4);
    first_ipv4(advertised)
        .or_else(|| first_ipv4(media))
        .or_else(|| candidates.first().map(Candidate::addr))
}
