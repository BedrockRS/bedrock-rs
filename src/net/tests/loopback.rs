//! End-to-end NetherNet session over loopback.
//!
//! A str0m peer driven through its standard SDP API stands in for the vanilla
//! client: it offers `a=setup:actpass`, opens both NetherNet data channels and
//! signs its DTLS fingerprint with an identity assertion. The test covers HTTP
//! signaling, identity checks in both directions, ICE, DTLS, SCTP and message
//! segmentation against a real [`Listener`].

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use bedrockrs_net::identity::verify_client;
use bedrockrs_net::sdp::{Offer, Setup};
use bedrockrs_net::segment::{Reassembler, split};
use bedrockrs_net::{
    Listener, ListenerConfig, RELIABLE_CHANNEL, Reliability, ServerIdentity, ServerStatus,
    UNRELIABLE_CHANNEL,
};
use bytes::Bytes;
use str0m::change::SdpAnswer;
use str0m::channel::{ChannelConfig, ChannelId};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, Input, Output, Rtc, RtcConfig};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

enum Command {
    Send(Reliability, Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
enum ClientEvent {
    Open(String),
    Message(Reliability, Vec<u8>),
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_connects_and_exchanges_segmented_messages() {
    // Opt into logs with RUST_LOG, e.g. `RUST_LOG=str0m=debug,bedrockrs_net=trace`.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let dir = std::env::temp_dir().join(format!("bedrockrs-loopback-{}", std::process::id()));
    let config = ListenerConfig {
        signaling_addr: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0),
        media_port: 0,
        media_ips: vec![Ipv4Addr::LOCALHOST.into()],
        identity_path: dir.join("identity.pem"),
        ..ListenerConfig::default()
    };
    let mut listener = Listener::bind(config, status()).await.unwrap();

    // The client opens both channels, as the vanilla client does.
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let mut rtc = RtcConfig::new().build(Instant::now());
    rtc.add_local_candidate(Candidate::host(socket.local_addr().unwrap(), "udp").unwrap())
        .unwrap();
    let mut changes = rtc.sdp_api();
    changes.add_channel(RELIABLE_CHANNEL.into());
    changes.add_channel_with_config(ChannelConfig {
        label: UNRELIABLE_CHANNEL.into(),
        ordered: false,
        reliability: str0m::channel::Reliability::MaxRetransmits { retransmits: 0 },
        ..ChannelConfig::default()
    });
    let (offer, pending) = changes.apply().unwrap();
    let offer = offer.to_sdp_string();

    // Bind the offer to a client key, like the GameServerToken's `cpk`.
    let client_key =
        ServerIdentity::generate("https://authorization.franchise.minecraft-services.net").unwrap();
    let assertion = client_key
        .assertion(&Offer::parse(&offer).unwrap().fingerprints)
        .unwrap();
    let offer = with_identity(&offer, &assertion);

    let (status, content_type, answer) =
        post(listener.signaling_addr(), "/v1/join/1234", &offer).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!(content_type, "application/sdp");

    // Check the server's assertion as a client would, then strip it before WebRTC.
    let parsed = Offer::parse(&answer).unwrap();
    assert_eq!(parsed.setup, Setup::Active);
    let server = verify_client(parsed.identity.as_deref().unwrap(), &parsed.fingerprints).unwrap();
    assert_eq!(server.issuer, "self");
    rtc.sdp_api()
        .accept_answer(
            pending,
            SdpAnswer::from_sdp_string(&for_str0m(&answer)).unwrap(),
        )
        .unwrap();

    let (commands, command_rx) = mpsc::channel(16);
    let (event_tx, mut events) = mpsc::channel(16);
    let client = tokio::spawn(run_client(rtc, socket, command_rx, event_tx));

    let mut connection = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    assert_eq!(connection.network_id(), 1234);
    assert_eq!(
        &connection.client_identity().unwrap().public_key,
        client_key.verifying_key()
    );
    let mut opened = vec![next_event(&mut events).await, next_event(&mut events).await];
    opened.sort_by_key(|event| format!("{event:?}"));
    assert_eq!(
        opened,
        [
            ClientEvent::Open(RELIABLE_CHANNEL.into()),
            ClientEvent::Open(UNRELIABLE_CHANNEL.into())
        ]
    );

    // Client to server: several segments on the reliable channel, then one unreliable message.
    let upload = pattern(300_000);
    commands
        .send(Command::Send(Reliability::Reliable, upload.clone()))
        .await
        .unwrap();
    let message = timeout(WAIT, connection.recv()).await.unwrap().unwrap();
    assert_eq!(message.reliability, Reliability::Reliable);
    assert_eq!(message.payload, upload);

    commands
        .send(Command::Send(Reliability::Unreliable, b"ping".to_vec()))
        .await
        .unwrap();
    let message = timeout(WAIT, connection.recv()).await.unwrap().unwrap();
    assert_eq!(message.reliability, Reliability::Unreliable);
    assert_eq!(&message.payload[..], b"ping");

    // Server to client: segmented by the server, reassembled by the client.
    let download = pattern(200_000);
    connection
        .send(Bytes::from(download.clone()), Reliability::Reliable)
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        ClientEvent::Message(Reliability::Reliable, download)
    );
    connection
        .send(Bytes::from_static(b"pong"), Reliability::Unreliable)
        .await
        .unwrap();
    assert_eq!(
        next_event(&mut events).await,
        ClientEvent::Message(Reliability::Unreliable, b"pong".to_vec())
    );

    client.abort();
    drop(listener);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Drives the stand-in client until the test drops its command sender.
async fn run_client(
    mut rtc: Rtc,
    socket: UdpSocket,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<ClientEvent>,
) {
    let local = socket.local_addr().unwrap();
    let mut buf = vec![0; 2048];
    let mut reliable: Option<ChannelId> = None;
    let mut unreliable: Option<ChannelId> = None;
    let mut reassembler = Reassembler::new(usize::MAX);
    let mut pending: VecDeque<Vec<u8>> = VecDeque::new();
    // str0m buffers at most 128 KiB, so keep segments well below that.
    let segment_payload = NonZeroUsize::new(60_000).unwrap();

    loop {
        let deadline = loop {
            match rtc.poll_output().unwrap() {
                Output::Timeout(at) => break at,
                Output::Transmit(transmit) => {
                    socket
                        .send_to(&transmit.contents, transmit.destination)
                        .await
                        .unwrap();
                }
                Output::Event(Event::ChannelOpen(id, label)) => {
                    if label == RELIABLE_CHANNEL {
                        reliable = Some(id);
                    } else {
                        unreliable = Some(id);
                    }
                    events.send(ClientEvent::Open(label)).await.unwrap();
                }
                Output::Event(Event::ChannelData(data)) => {
                    let message = if Some(data.id) == reliable {
                        reassembler
                            .push(&data.data)
                            .unwrap()
                            .map(|payload| ClientEvent::Message(Reliability::Reliable, payload))
                    } else {
                        assert_eq!(data.data[0], 0, "unreliable messages are never segmented");
                        Some(ClientEvent::Message(
                            Reliability::Unreliable,
                            data.data[1..].to_vec(),
                        ))
                    };
                    if let Some(message) = message {
                        events.send(message).await.unwrap();
                    }
                }
                Output::Event(_) => {}
            }
        };

        if let Some(id) = reliable {
            while let Some(segment) = pending.front() {
                let Some(mut channel) = rtc.channel(id) else {
                    break;
                };
                if !channel.write(true, segment).unwrap() {
                    break;
                }
                pending.pop_front();
            }
        }

        let wait = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(20));
        tokio::select! {
            received = socket.recv_from(&mut buf) => {
                let (len, source) = received.unwrap();
                let receive = Receive::new(Protocol::Udp, source, local, &buf[..len]).unwrap();
                rtc.handle_input(Input::Receive(Instant::now(), receive)).unwrap();
            }
            command = commands.recv() => match command {
                Some(Command::Send(Reliability::Reliable, payload)) => {
                    pending.extend(split(&payload, segment_payload).unwrap());
                }
                Some(Command::Send(Reliability::Unreliable, payload)) => {
                    let mut frame = vec![0];
                    frame.extend(payload);
                    let mut channel = rtc.channel(unreliable.unwrap()).unwrap();
                    assert!(channel.write(true, &frame).unwrap());
                }
                None => return,
            },
            () = tokio::time::sleep(wait) => {
                rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
            }
        }
    }
}

async fn next_event(events: &mut mpsc::Receiver<ClientEvent>) -> ClientEvent {
    timeout(WAIT, events.recv()).await.unwrap().unwrap()
}

/// Minimal HTTP/1.1 POST, returning the status, content type and body.
async fn post(addr: SocketAddr, path: &str, body: &str) -> (u16, String, String) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/sdp\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();

    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let content_type = head
        .lines()
        .find_map(|line| line.strip_prefix("content-type: "))
        .unwrap_or_default()
        .to_owned();
    (status, content_type, body.to_owned())
}

/// Adds a session-level `a=identity` line before the first media section.
fn with_identity(sdp: &str, assertion: &str) -> String {
    let media = sdp.find("\nm=").unwrap() + 1;
    format!(
        "{}a=identity:{assertion}\r\n{}",
        &sdp[..media],
        &sdp[media..]
    )
}

/// Prepares our answer for the str0m client: strips `a=identity`, as a vanilla
/// client does before WebRTC, and drops the `ufrag` candidate extension. The
/// answer writes candidate extensions in libwebrtc's order (`generation ufrag
/// network-id network-cost`), but str0m's SDP parser only accepts `ufrag` after
/// `network-id` and otherwise ignores the whole candidate. The ufrag duplicates
/// `a=ice-ufrag`, so nothing is lost.
fn for_str0m(sdp: &str) -> String {
    sdp.split_inclusive('\n')
        .filter(|line| !line.starts_with("a=identity:"))
        .map(|line| match line.strip_prefix("a=candidate:") {
            Some(candidate) => {
                let mut fields: Vec<&str> = candidate.trim_end().split(' ').collect();
                if let Some(at) = fields.iter().position(|field| *field == "ufrag") {
                    fields.drain(at..at + 2);
                }
                format!("a=candidate:{}\r\n", fields.join(" "))
            }
            None => line.to_owned(),
        })
        .collect()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 256) as u8).collect()
}

fn status() -> ServerStatus {
    ServerStatus {
        name: "BedrockRS test".into(),
        protocol: 2193,
        version: "1.26.51".into(),
        level: "world".into(),
        players: 0,
        max_players: 10,
        game_type: 0,
    }
}
