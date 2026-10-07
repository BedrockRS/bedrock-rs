//! UDP sockets shared by every WebRTC session.
//!
//! One socket is bound per local address so each received datagram knows the
//! destination address str0m needs. Datagrams are routed to sessions by the
//! local ICE username fragment in STUN requests and, once a path is known, by
//! its (local, remote) address pair.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use str0m::ice::StunMessage;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Receive buffer size; str0m never sends datagrams larger than 2000 bytes.
const RECV_BUFFER: usize = 2048;

/// A datagram received for one session.
#[derive(Debug)]
pub(crate) struct Datagram {
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub received: Instant,
    pub data: Vec<u8>,
}

/// Where the mux delivers a session's datagrams.
#[derive(Debug, Clone)]
pub(crate) struct PeerRoute {
    pub id: u64,
    pub tx: mpsc::Sender<Datagram>,
}

#[derive(Default)]
struct Routes {
    by_ufrag: HashMap<String, PeerRoute>,
    by_path: HashMap<(SocketAddr, SocketAddr), PeerRoute>,
}

pub(crate) struct UdpMux {
    sockets: HashMap<SocketAddr, Arc<UdpSocket>>,
    routes: Mutex<Routes>,
}

impl UdpMux {
    /// Binds `port` on every address in `ips`. Port 0 picks a free port per address.
    pub async fn bind(ips: &[IpAddr], port: u16) -> io::Result<Arc<Self>> {
        let mut sockets = HashMap::with_capacity(ips.len());
        for &ip in ips {
            let socket = UdpSocket::bind(SocketAddr::new(ip, port)).await?;
            sockets.insert(socket.local_addr()?, Arc::new(socket));
        }
        Ok(Arc::new(Self {
            sockets,
            routes: Mutex::default(),
        }))
    }

    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        let mut addrs: Vec<_> = self.sockets.keys().copied().collect();
        addrs.sort();
        addrs
    }

    /// Starts one receive task per socket.
    pub fn spawn_readers(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        self.sockets
            .iter()
            .map(|(&local, socket)| {
                tokio::spawn(read_loop(Arc::clone(self), Arc::clone(socket), local))
            })
            .collect()
    }

    /// Routes STUN requests addressed to `ufrag`, our local ICE username fragment.
    pub fn register(&self, ufrag: String, route: PeerRoute) {
        self.routes().by_ufrag.insert(ufrag, route);
    }

    /// Routes datagrams arriving on `local` from `remote`, e.g. responses to our
    /// connectivity checks, which carry no username.
    pub fn learn(&self, local: SocketAddr, remote: SocketAddr, route: &PeerRoute) {
        self.routes().by_path.insert((local, remote), route.clone());
    }

    pub fn unregister(&self, id: u64) {
        let mut routes = self.routes();
        routes.by_ufrag.retain(|_, route| route.id != id);
        routes.by_path.retain(|_, route| route.id != id);
    }

    /// Sends without waiting; when the socket buffer is full the datagram is
    /// dropped and str0m's retransmissions recover.
    pub fn send(&self, source: SocketAddr, destination: SocketAddr, data: &[u8]) {
        let Some(socket) = self.sockets.get(&source) else {
            tracing::warn!("Couldn't send from {source}: no socket is bound there");
            return;
        };
        match socket.try_send_to(data, destination) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                tracing::trace!(%destination, "UDP send buffer full, dropping datagram");
            }
            // Some candidate pairs are unroutable from this host, e.g. from a virtual
            // adapter to another subnet (Windows error 10051). ICE simply moves on.
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NetworkUnreachable
                        | io::ErrorKind::HostUnreachable
                        | io::ErrorKind::AddrNotAvailable
                ) =>
            {
                tracing::trace!(%source, %destination, %err, "candidate pair unroutable");
            }
            Err(err) => tracing::debug!(%source, %destination, %err, "UDP send failed"),
        }
    }

    fn route(&self, local: SocketAddr, source: SocketAddr, data: &[u8]) -> Option<PeerRoute> {
        let mut routes = self.routes();
        // A STUN request names its session, so it wins over a stale path from an
        // earlier session with the same remote address.
        if let Some(route) = stun_ufrag(data).and_then(|ufrag| routes.by_ufrag.get(ufrag).cloned())
        {
            routes.by_path.insert((local, source), route.clone());
            return Some(route);
        }
        routes.by_path.get(&(local, source)).cloned()
    }

    fn routes(&self) -> MutexGuard<'_, Routes> {
        self.routes.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

async fn read_loop(mux: Arc<UdpMux>, socket: Arc<UdpSocket>, local: SocketAddr) {
    let mut buf = vec![0; RECV_BUFFER];
    loop {
        let (len, source) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            // Windows reports an ICMP port-unreachable for an earlier send as a receive error.
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionRefused
                ) =>
            {
                continue;
            }
            Err(err) => {
                tracing::error!("The network socket on {local} failed: {err}");
                return;
            }
        };
        let data = &buf[..len];
        let Some(route) = mux.route(local, source, data) else {
            continue;
        };
        let datagram = Datagram {
            source,
            destination: local,
            received: Instant::now(),
            data: data.to_vec(),
        };
        if route.tx.try_send(datagram).is_err() {
            tracing::trace!(
                peer = route.id,
                "dropping datagram for a busy or closed session"
            );
        }
    }
}

/// The local username fragment of a STUN request (`USERNAME` is `ours:theirs`).
fn stun_ufrag(data: &[u8]) -> Option<&str> {
    // STUN messages begin with two zero bits and a 20-byte header.
    if data.len() < 20 || data[0] > 1 {
        return None;
    }
    let username = StunMessage::parse(data).ok()?.username()?;
    Some(
        username
            .split_once(':')
            .map_or(username, |(local, _)| local),
    )
}

#[cfg(test)]
mod tests {
    use str0m::ice::{StunMessageBuilder, TransId};

    use super::*;

    fn binding_request(username: &str) -> Vec<u8> {
        // ICE binding requests always carry PRIORITY and MESSAGE-INTEGRITY, and
        // the parser insists on both. It does not check the HMAC value.
        let message = StunMessageBuilder::new()
            .binding()
            .request()
            .username(username)
            .prio(1)
            .build(TransId::new());
        let mut buf = vec![0; 512];
        let len = message
            .to_bytes(Some(b"password"), &mut buf, |_, _| [0; 20])
            .unwrap();
        buf.truncate(len);
        buf
    }

    #[test]
    fn extracts_local_ufrag_from_binding_requests() {
        assert_eq!(
            stun_ufrag(&binding_request("server:client")),
            Some("server")
        );
        // DTLS records start with a content type of 20..=63.
        assert_eq!(stun_ufrag(&[22; 40]), None);
    }

    #[tokio::test]
    async fn routes_by_ufrag_then_by_learned_path() {
        let mux = UdpMux::bind(&["127.0.0.1".parse().unwrap()], 0)
            .await
            .unwrap();
        let local = mux.local_addrs()[0];
        let remote: SocketAddr = "127.0.0.1:40000".parse().unwrap();
        let (tx, _rx) = mpsc::channel(1);
        mux.register("server".into(), PeerRoute { id: 7, tx });

        assert!(mux.route(local, remote, &[23; 40]).is_none());
        assert_eq!(
            mux.route(local, remote, &binding_request("server:client"))
                .map(|r| r.id),
            Some(7)
        );
        assert_eq!(mux.route(local, remote, &[23; 40]).map(|r| r.id), Some(7));

        mux.unregister(7);
        assert!(mux.route(local, remote, &[23; 40]).is_none());
        assert!(
            mux.route(local, remote, &binding_request("server:client"))
                .is_none()
        );
    }
}
