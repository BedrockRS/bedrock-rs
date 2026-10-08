# BedrockRS — Architecture

Living design document. Plan approved 2026-09-25; research is current as of that date.

| | |
|---|---|
| Target | Minecraft Bedrock Edition **26.51**, network protocol **2193** |
| Transport | **NetherNet only** (WebRTC). RakNet is not implemented. |
| Language | Rust, edition 2024, MSRV 1.93, safe Rust (`unsafe_code = "forbid"`), tokio |
| Plugins | Zero-build, hot-reloaded scripts: Luau (`mlua`), JS/TS (`deno_core`), Python (RustPython) |

Contents: [1 Goals](#1-goals-and-constraints) ·
[2 Research findings](#2-research-findings) ·
[3 NetherNet wire reference](#3-nethernet-wire-reference) ·
[4 Architecture](#4-architecture) ·
[5 Approved decisions](#5-approved-decisions-2026-09-25) ·
[6 Build plan](#6-build-plan) ·
[7 Risks](#7-risks-and-open-questions) ·
[8 Sources](#8-sources)

---

## 1. Goals and constraints

- A high-performance custom Bedrock dedicated server written in Rust.
- Speak Bedrock 26.51 / protocol 2193 (released 2026-09-15 on Windows and PlayStation,
  2026-09-16 elsewhere; server version 1.26.51.1).
- NetherNet only. BDS has defaulted to `transport=nethernet` since 1.26.50, and 26.60
  (in preview) removes RakNet entirely.
- Plugins are folders of plain text files in `plugins/`, each with a `plugin.json`
  manifest, loaded and hot-reloaded with no build step.
- Safe Rust: our crates forbid `unsafe`. FFI stays inside `mlua` and `rusty_v8`.

## 2. Research findings

### 2.1 Confirmed from the original brief

- 26.51 is protocol 2193.
- Signaling runs over HTTP on the server's TCP port (default 19132), under `/v1/join`.
- Game traffic runs over UDP on the `ReliableDataChannel` and `UnreliableDataChannel`
  WebRTC data channels.

### 2.2 Corrections to the original brief

1. **Exact endpoints.** `GET /v1/join` is both the capability probe and the server-list
   status. `POST /v1/join/{networkId}` exchanges the SDP offer and answer
   ([§3.1](#31-signaling-http-on-the-tcp-server-port)).
2. **A mandatory `a=identity` (RFC 8827) in the SDP answer.** The brief does not mention
   it, and the client rejects answers without it ([§3.2](#32-identity-assertion-rfc-8827)).
3. **Segmentation is not a fixed "> 10,000 bytes" rule.** Every message carries a 1-byte
   countdown, and the segment payload is the negotiated `max-message-size` − 1
   ([§3.4](#34-data-channel-framing-segmentation)). The 10,000 figure comes from
   `df-mc/nethernet-spec`, which was reverse-engineered from 1.20.50. It survives only as
   a stale doc comment in go-nethernet.
4. **Framing above the transport.** There is no `0xFE` batch header and no app-level
   encryption. Compression and batching still apply ([§3.5](#35-game-packet-framing)).

DeepWiki's AI-generated page on Mojang's docs describes a uint32 fragment header. That
contradicts both Mojang's guide and go-nethernet's code, so it is disregarded.

### 2.3 Reference projects

- **WaterdogPE** (Java proxy)
  - Runs RakNet and NetherNet side by side, using a bundled libdatachannel.
  - Signaling modes: `builtin`, `nxs` (external provider), `hybrid` and `plugin`.
  - Signaling defaults to the listener port. Media uses `udp_port`: 0 means an ephemeral
    port per peer, and a fixed port means ICE UDP muxing.
  - `server_type: bedrock` probes a backend's `GET /v1/join` to pick the transport.
  - Reverse proxies must forward both routes.
  - The P-384 identity key (`identity_file`) must be shared across proxy instances.
  - *Takeaways:* keep signaling behind a trait, and make the identity key portable.
- **Pumpkin** (Rust)
  - Merged 26.51 support on 2026-09-17 (PR #3472).
  - NetherNet uses the `webrtc` crate plus axum, with a P-384 identity and hashed Bedrock
    block IDs.
  - Uses tokio for I/O and rayon for CPU-bound work. Plugins run via wasmtime and native
    libraries.
  - Explored LAN discovery (UDP 7551) in PR #2825, then dropped it on 2026-09-22 in favor
    of HTTP signaling.
- **Dragonfly** (Go)
  - Each world is owned by a single goroutine, and every mutation runs in a transaction
    (`world.Tx`). Code running elsewhere schedules closures with `World.Do`, so the hot
    path needs no locks.
  - `Loader`/`Viewer` stream chunks to players.
  - Sub-chunks are paletted storages with bits per block ∈ {0,1,2,3,4,5,6,8,16}.
  - Network block IDs are FNV-1a-32 hashes of the block's `{name, states}` NBT, with
    `UseBlockNetworkIDHashes` set in StartGame.
  - Worlds persist to LevelDB (`mcdb`).
  - NetherNet support (PR #1290) is built on go-nethernet:
    - Its UDP mux defaults to 19133, because pion fails with `EADDRINUSE` next to RakNet
      on 0.0.0.0:19132.
    - Its built-in signaling is plaintext HTTP, with TLS left to a reverse proxy.
- **go-nethernet / gophertunnel** (Go): the reference implementation of the wire details
  in §3.
- **Mojang/bedrock-protocol-docs**
  - Official packet, type and enum JSON schemas for each release (tag `v1.26.51` = 2193),
    plus the NetherNet onboarding guide.
  - The schemas are machine-readable: `x-underlying-type`, `x-ordinal-index`,
    `x-serialization-options`, and packet IDs in `$metaProperties["[cereal:packet]"]`.
  - Licensed "All rights reserved" under the Minecraft EULA. **Use as a reference only;
    never vendor it into this repository.**

## 3. NetherNet wire reference

### 3.1 Signaling (HTTP on the TCP server port)

Clients request the signaling endpoint on exactly the host and port the player entered,
with no fallback ports. They try HTTPS first, then plain HTTP.

**`GET /v1/join`** is the capability probe and the server-list status.

- Any 2xx means NetherNet is supported. Any other status makes the client abort.
- The response is `Content-Type: application/json`.
- `gameType` values: 0 Survival, 1 Creative, 2 Adventure.

```json
{
  "name": "Dedicated Server",
  "protocol": 2193,
  "version": "1.26.51",
  "level": "Bedrock level",
  "players": 0,
  "maxPlayers": 10,
  "gameType": 0
}
```

**`POST /v1/join/{networkId}`** exchanges the SDP offer and answer.

- `networkId` is an opaque client identifier, currently a decimal u64.
- The request body is the SDP offer (`application/sdp`, UTF-8) with every ICE candidate
  inline.
- A 2xx response carries the SDP answer (`application/sdp`) with every candidate inline.
- The client sends exactly one request per attempt and never retries. Any non-2xx
  response ends the attempt.

BedrockRS mirrors go-nethernet's handler:

| Condition | Response |
|---|---|
| Missing or invalid `networkId`, empty body | 400 |
| Offer larger than 1 MiB | 413 |
| Offer not admitted | 503 |
| Negotiation took longer than 15 s | 502 |

Error bodies are `text/plain`.

### 3.2 Identity assertion (RFC 8827)

**Attribute format**

- Both the offer and the answer carry a session-level `a=identity:<base64(JSON)>`
  (standard, padded base64), placed before the first `m=` line.
- The decoded JSON is `{"assertion":"<string>","idp":{"domain":"<domain>","protocol":"default"}}`.
  The `assertion` value is itself JSON encoded as a string (double-encoded):
  `{"fingerprints":"<detached JWS>","token":"<JWT>"}`.
- `fingerprints` is a detached JWS (RFC 7515 Appendix F) in the compact form
  `base64url(header)..base64url(signature)`.
- The JWS covers the canonical JSON of the SDP's `a=fingerprint` lines:
  `{"fingerprint":[{"algorithm":"sha-256","digest":"AA:BB:..."}]}`.
  Canonical means sorted keys, no insignificant whitespace and minimal escaping
  (a subset of RFC 8785).

**Client offer.** The `token` is the player's GameServerToken from the Minecraft auth
service. It is RS256-signed (per go-nethernet), carries the XUID, UUID and PlayFabId, and
includes a `cpk` claim with the client's P-384 public key. 26.40+ clients send `cpk` as a
JWK object; older clients sent a base64 SPKI DER string. `idp.domain` names the issuer,
observed from a 1.26.51 client as `https://authorization.franchise.minecraft-services.net/`
(with the trailing slash). The server:

1. Validates the JWT signature against the auth service's keys.
2. Rebuilds the fingerprint JSON from the offer.
3. Verifies the JWS with `cpk`.
4. Authorizes the player.
5. **Strips `a=identity` before handing the SDP to WebRTC.** Unknown attributes cause the
   SDP to be rejected.

Over HTTP, any non-2xx response rejects the player. go-nethernet uses
`ErrorCodeIdentityNotAllowed` (37) on its other signaling transports.

**Server answer**

- The `token` is a self-signed ES384 JWT. BedrockRS shapes it like BDS does (per
  go-nethernet):
  - header `{"alg":"ES384","x5u":"<base64 SPKI DER public key>"}`
  - claims `cpk` (our public key as a JWK, RFC 7517), `iat`, and `exp` = `iat` + 60 s
- Mojang's guide also recommends `iss`, which the trust prompt would display. BDS omits it.
- `idp.domain` is `"self"`, as on BDS.
- `fingerprints` is a detached JWS (`{"alg":"ES384"}` header) over the answer's
  fingerprints, signed with the same key.
- **The answer must always include `a=identity`.**

**How the client verifies our answer**

1. It checks the JWT self-signature using `cpk`, then the fingerprint JWS, then `exp`.
2. It applies a trust anchor:
   - **HTTPS:** accepted silently, because TLS authenticates the endpoint.
   - **Plain HTTP:** trust on first use (TOFU). A pinned key is accepted. An unknown key
     triggers a prompt that shows the key fingerprint, and an accepted key is pinned.
3. **Changing our key re-prompts every player.**

### 3.3 WebRTC session

**Transport settings**

- Data channels only, with no audio or video.
- UDP only (TCP candidates disabled), `max-bundle`, and no trickle ICE.
- No STUN/TURN by default, so only host candidates are exchanged. The server must
  therefore put a client-reachable address in its answer's candidates, via a configurable
  advertise address for NAT or a public IP.
- ICE roles: the client is controlling and the server is controlled.
- **BedrockRS runs ICE-lite by default.** The answer carries a session-level `a=ice-lite`,
  and the server never probes candidate pairs; it only answers the client's checks.
  Every datagram therefore leaves through the socket that received the client's
  traffic. Under full ICE, the server's own checks also tried pairs Windows cannot route,
  e.g. from a VirtualBox host-only adapter to the LAN (error 10051); those failures were
  harmless but noisy. `ListenerConfig::ice_lite` (`BEDROCKRS_ICE_LITE=false`) switches
  back to full ICE.
- str0m's ICE-lite agent accepts only host candidates, so advertised public addresses
  are announced as host candidates in lite mode (standard for 1:1 NAT).

**SDP**

- Media line: `m=application 9 UDP/DTLS/SCTP webrtc-datachannel`.
- Attributes: `a=sctp-port:5000` and `a=max-message-size:262144`.
- The offer uses `a=setup:actpass`. Mojang's example answer uses `a=setup:active`; the
  guide allows either active or passive.

**Data channels.** The client creates both channels, and the server receives them
(`ondatachannel`):

| Label | Ordered | Reliable | maxRetransmits |
|---|---|---|---|
| `ReliableDataChannel` | yes | yes | default |
| `UnreliableDataChannel` | no | no | 0 |

**Connection sequence**

1. The client gathers all its candidates.
2. The client sends `POST /v1/join/{networkId}` with its offer.
3. The server sets the remote description, creates an answer and gathers its candidates.
4. The server responds 200 with the answer.
5. ICE connectivity checks run.
6. DTLS completes. The certificate must match `a=fingerprint`.
7. The SCTP association is established.
8. The data channels open.

Answer layout written by `bedrockrs_net::sdp`. It mirrors go-nethernet's encoder, which
works with vanilla clients:
- `a=identity` is session-level, while `a=fingerprint` is media-level.
- `a=ice-options:trickle` is always present.
- There is no `a=end-of-candidates`.
- Candidates use libwebrtc's extension format.

```text
v=0
o=- <session-id> 2 IN IP4 127.0.0.1
s=-
t=0 0
a=ice-lite                                (unless full ICE is configured)
a=group:BUNDLE <mid>
a=extmap-allow-mixed
a=msid-semantic: WMS
a=identity:<base64 identity JSON>
m=application <default port> UDP/DTLS/SCTP webrtc-datachannel
c=IN IP4 <default address>
a=candidate:<foundation> 1 udp <priority> <ip> <port> typ host generation 0 ufrag <ufrag> network-id 0 network-cost 0
a=ice-ufrag:<ufrag>
a=ice-pwd:<pwd>
a=ice-options:trickle
a=fingerprint:sha-256 <OUR:DTLS:CERT:DIGEST>
a=setup:active
a=mid:<mid, echoed from the offer>
a=sctp-port:5000
a=max-message-size:262144
```

### 3.4 Data-channel framing (segmentation)

- Every message on both channels is `[u8 remaining][payload]`.
- `remaining` is the number of segments still to follow. **0 means a complete message or
  the final segment.** A 3-segment message is `[0x02]…`, `[0x01]…`, `[0x00]…`. The
  receiver accumulates payloads until it sees 0.
- **Segment payload = negotiated SCTP `max-message-size` − 1**, which is 262,143 for
  256 KiB. go-nethernet reads the size from the remote SDP. Sending smaller segments is
  always valid.
- Only the reliable channel fragments. On the unreliable channel the header is always
  `0x00`, and oversize messages are dropped.
- The u8 header allows at most 256 segments. go-nethernet refuses to send more than 255.
- **BedrockRS's current limits (str0m `DirectApi`):**
  - We send segments of min(client `max-message-size`, 64 KiB) − 1 = 65,535 bytes, because
    str0m's direct API cannot pass it the client's advertised size.
  - We receive SCTP messages up to the 256 KiB we advertise.
  - str0m buffers at most 128 KiB across streams, so outgoing segments wait in a
    per-session queue, and `Connection::send` applies backpressure beyond 4 MiB.
- The BedrockRS receiver adds two protections:
  - It enforces a strict countdown: a restart mid-message or a skipped value is a protocol
    error, and the connection closes.
  - It caps the reassembled size as a DoS guard.

### 3.5 Game packet framing

**What NetherNet removes**

- No `0xFE` batch header: go-nethernet's `BatchHeader()` returns nil.
- No app-level encryption: DTLS already encrypts, so go-nethernet's `DisableEncryption()`
  returns true. There is no ServerToClientHandshake and no AES layer.

**What still applies.** Compression and batching are independent of the transport:

1. The first message is `RequestNetworkSettings`, uncompressed.
   `ClientNetworkVersion` is an int32, big-endian.
2. The server replies with `NetworkSettings` (packet ID 143). Its fields, in order:
   - compression threshold, u16 (0 = disabled, 1 = compress everything)
   - compression algorithm, u16
   - client throttle enabled, bool
   - client throttle threshold, u8
   - client throttle scalar, f32
3. After that, every message is `[algorithm u8][(varuint32 length, packet)…]`, and the
   tail is compressed:
   - `0x00`: zlib (raw DEFLATE)
   - `0x01`: snappy
   - `0xFF`: none
4. gophertunnel caps a batch at 812 packets.

The first client message is exactly `06 | C1 01 | 00 00 08 91`: a one-byte batch length
(6), packet ID 193 as a varuint32, then protocol 2193 as a big-endian int32.

**Login flow over NetherNet** (gophertunnel's server; implemented in
`bedrockrs_core::session`). There is no ServerToClientHandshake step.

| Client sends | Server replies |
|---|---|
| RequestNetworkSettings (193) | NetworkSettings (143): raw DEFLATE, threshold 256; compression starts for both sides. On a protocol mismatch, PlayStatus `LoginFailedClient`/`LoginFailedServer` instead. |
| Login (1) | PlayStatus `LoginSuccess` (2) + ResourcePacksInfo (6), with no packs |
| ClientCacheStatus (129) | nothing; the blob cache is not supported |
| ResourcePackClientResponse (8) `downloadingfinished` | ResourcePackStack (7): gophertunnel's 8 exempted vanilla packs, base game version `1.26.51` |
| ResourcePackClientResponse `resourcepackstackfinished` | StartGame is next. It is **not implemented**, so BedrockRS sends Disconnect (5) with a message. |

- **Packet layouts at protocol 2193:**
  - `ResourcePackClientResponse` is a varuint32 tag (0 cancel, 1 downloading,
    2 downloadingfinished, 3 resourcepackstackfinished) followed by the tag's name as a
    string.
  - `Disconnect` is a varint32 reason, a "hide screen" bool, then message and filtered
    message.
  - Lists use varuint32 counts, except the experiments list, which uses a u32.
- **The Login connection request** holds two blobs, each prefixed with a u32 LE length:
  - auth JSON `{AuthenticationType, Certificate: "{\"chain\":[…]}", Token}`
  - the client-data JWT

  The multiplayer `Token` carries `xid` (XUID), `xname` (gamertag) and `cpk`
  (base64 SPKI DER).

**Security consequence.** Without app-level encryption, a captured Login packet could be
replayed; go-nethernet warns about this itself. BedrockRS therefore requires the Login
token's `cpk` to equal the key proven by the SDP `a=identity`, as gophertunnel does.
Otherwise it sends Disconnect `NotAuthenticated`.

**Player authentication chain** (gophertunnel `service` package):

1. Minecraft services discovery.
2. The auth service environment (`ServiceURI`, `Issuer`).
3. The OpenID configuration and JWKS.
4. Verify the multiplayer token.

The exact URLs will be confirmed when auth is implemented.

### 3.5.1 Spawn sequence

Implemented in `bedrockrs_core::session`, following gophertunnel's server with
Dragonfly's values.

| Client sends | Server replies |
|---|---|
| ResourcePackClientResponse `resourcepackstackfinished` | JigsawStructureData (313; NBT with empty `processors`, `template_pools`, `jigsaws`, `structure_sets` lists), VoxelShapes (337, empty), StartGame (11), ItemRegistry (162, every vanilla item) |
| RequestChunkRadius (69) | ChunkRadiusUpdated (70, capped at 8), NetworkChunkPublisherUpdate (121: spawn and radius × 16 blocks), every LevelChunk (58) in the circle nearest-first, then on the first request the player's InventoryContent (49: windows 0, 119 offhand, 120 armour), PlayStatus `PlayerSpawn` + CreativeContent (145, the vanilla creative inventory) |
| SetLocalPlayerAsInitialized (113) | nothing; the player is in the world |
| anything else once in the world, e.g. PlayerAuthInput (144) every tick | ignored for now |

- **StartGame** has 81 fields. The encoder was checked line by line against
  gophertunnel's 2193 marshal. The quirks:
  - `BlockPos` is three zigzag varints.
  - UUIDs are two u64 LE halves, the most significant first.
  - Optionals are a presence bool followed by the value.
  - `PropertyData` is network NBT.
  - `PlayerPosition` is the eye position (feet + 1.62).
- **Values:**
  - Entity IDs are 1.
  - Creative mode, peaceful, the flat generator (2), noon.
  - The `showcoordinates` game rule is on.
  - Server-authoritative inventory and block breaking, as Dragonfly sends.
  - `use_block_network_id_hashes = true`.
- **Block network IDs** are FNV-1a-32 of little-endian NBT `{"name", "states"}`, with
  states sorted by name and no version field (Dragonfly's `network_block_hash.go`). The
  client hashes its own states, so no block palette is sent.
- **Data-driven vanilla blocks:** since 1.26.50 Mojang defines some vanilla blocks in
  JSON (every wool and concrete slab, double slab and stair, the red shrub and the shelf
  mushroom: 98 in all). The client ships their textures but only knows the blocks from
  StartGame's block list, which carries each one's definition (name and components as
  network NBT), as Dragonfly sends them from its `data_driven_blocks.nbt`. Without them
  (live test, 2026-10-08) the client logged "Block … couldn't be found in the registry"
  and showed their items as untextured `item.white_wool_slab.name` squares.
- **LevelChunk:** chunk x/z and dimension as varints; the sub-chunk count as a varuint32
  (at most 64); an optional sub-chunk limit, absent; a cache bool, false; empty blob
  hashes; then the payload.
- **The payload** is:
  - the sub-chunks, from the bottom up
  - one biome storage per sub-chunk of the dimension (24 for the overworld), where
    `0xFF` repeats the previous one
  - a zero border-block byte
  - no block entities
- **Sub-chunks** are `[9][layer count][y index]` followed by paletted layers. Each layer
  is:
  - a header of `bits << 1 | 1`
  - little-endian u32 words, packing `32 / bits` indices LSB-first in x→z→y order
    (bits ∈ {0, 1, 2, 3, 4, 5, 6, 8, 16})
  - a palette of zigzag varints; 0-bit layers omit the palette size
- **Deliberately not sent:**
  - BiomeDefinitionList. Mojang describes it as the list of *all available biomes*, so
    an empty one could remove plains, which the chunks use. gophertunnel's minimal
    server omits it too.
  - AvailableActorIdentifiers.
  - Inventories, attributes and entity metadata.
- The old ItemComponent packet does not exist in 2193; ItemRegistry (162) replaced it.

### 3.6 Ports and discovery

| Port | Protocol | Use |
|---|---|---|
| 19132 (`server-port`) | TCP | HTTP(S) signaling |
| 19133 (default, configurable) | UDP | WebRTC media for all peers, on one muxed socket |
| 7551 | UDP | LAN discovery. BDS binds it; not planned for BedrockRS. |

Xbox Live and Realms use WebSocket signaling via `signal.franchise.minecraft-services.net`.
That is out of scope.

### 3.7 Version timeline

| Release | Protocol | Date |
|---|---|---|
| 1.26.50 | 2193 | 2026-09-15 |
| 1.26.51 (hotfix) | 2193 | 2026-09-15/16 |
| 1.26.60 preview.21 → preview.28 | 2207 → 2216 | 2026-09-02 → 2026-09-22 |

26.60 removes RakNet.

## 4. Architecture

### 4.1 Workspace layout

```text
bedrock-rs/
├── Cargo.toml              # workspace: resolver 3, edition 2024, MSRV 1.93, shared deps + lints
├── docs/ARCHITECTURE.md    # this document
├── tools/                  # Python scripts that generate src/core/data/*.json
├── server/                 # the folder the server runs in
│   ├── bedrockrs.toml      # runtime: configuration (created on first run, git-ignored)
│   ├── keys/               # runtime: identity.pem (auto-generated, git-ignored)
│   ├── plugins/            # hot-reloaded *.luau / *.ts / *.js / *.py
│   └── worlds/             # runtime: saved worlds (git-ignored)
└── src/
    ├── protocol/           # bedrockrs_protocol: varints, NBT, batch codec, packets @ 2193
    ├── net/                # bedrockrs_net: signaling + WebRTC + segmentation → byte messages
    ├── plugins/            # bedrockrs_plugins: engine trait, Luau/JS/Python hosts, hot reload
    └── core/               # bedrockrs_core: 20 TPS loop, world, entities; binary `bedrockrs`
```

### 4.2 Dependency graph

```text
                 bedrockrs_core  (lib + bin `bedrockrs`)
                /       |        \
   bedrockrs_net  bedrockrs_protocol  bedrockrs_plugins
```

- The three leaf crates don't depend on each other, so each one builds and tests in
  isolation.
- `bedrockrs_net` only moves bytes and knows nothing about Minecraft packets.

### 4.3 `bedrockrs_net`

| Module | Responsibility |
|---|---|
| `signaling` | axum routes for `GET /v1/join` (status JSON from a `watch` channel) and `POST /v1/join/{networkId}`, with the limits and status codes in §3.1. Offers go through the `OfferHandler` trait so a proxy- or NXS-style provider can plug in. Serving TLS on the same port (peeking the first byte, `0x16`) is not implemented yet. |
| `sdp` | Parses the data-channel-only offer (`a=identity` is read separately and never reaches str0m) and writes answers in the §3.3 layout. |
| `identity` | Loads or creates the persistent P-384 key (`keys/identity.pem`, PKCS#8 PEM, 0600 on Unix) and signs answers (§3.2). For clients, verifies that the token's `cpk` key signed the offer's fingerprints; the token itself is **not** verified yet (§7). |
| `mux` | One UDP socket per local address (default port 19133). Datagrams are routed to sessions by the STUN `USERNAME` ufrag, then by learned (local, remote) paths. Sends always leave through the socket bound to str0m's chosen source address. Windows `ConnectionReset` receive errors are ignored, and unroutable-pair send errors are logged only at trace level. |
| `peer` | One tokio task per session driving str0m through `DirectApi`: ICE controlled, DTLS active, SCTP client, the client's channels matched by label. Handles segmentation, backpressure, a connect timeout, a 10 s ICE-disconnect grace and graceful close. |
| `listener` | `Listener` / `ListenerConfig`. Discovers interfaces, generates one DTLS certificate at startup, answers offers and yields a `Connection` once the reliable channel opens. |
| `segment` | Pure, I/O-free `split` / `Reassembler` (§3.4), with unit tests. |

API: `Listener::bind(config, status)` → `accept()` → a `Connection` with
`recv() -> Option<Message>`, `send(Bytes, Reliability)` and `client_identity()`.
`Listener::update_status` changes what `GET /v1/join` reports.

The `bedrockrs` binary starts a listener with the defaults. These environment variables
override them:
- `BEDROCKRS_SIGNALING_ADDR`
- `BEDROCKRS_MEDIA_PORT`
- `BEDROCKRS_MEDIA_IPS`
- `BEDROCKRS_ADVERTISE_IPS`
- `BEDROCKRS_ICE_LITE` (`false` switches to full ICE)

For local testing, bind to `127.0.0.1` to avoid the Windows Firewall prompt.

Tests: 26 unit tests plus `tests/loopback.rs`, an end-to-end session against a str0m client
using its standard SDP API. That run covers signaling, identity in both directions, ICE,
DTLS, SCTP, and multi-segment messages both ways.

### 4.4 `bedrockrs_protocol`

- **Primitives:** varint/zigzag, little-endian numerics, varuint32-prefixed strings.
- **NBT:** written in-house, because existing crates target Java's big-endian format.
  Supports Bedrock's little-endian (disk) and network (varint) flavors, serde-based.
- **Batch codec:** as described in §3.5.
- **Packets:** hand-written for the login path first. Later, an optional `xtask codegen`
  can read a developer's local copy of Mojang's schemas (never committed).
- **Versioning:** `PROTOCOL_VERSION = 2193`, structured so 26.60 (2216+) can be added
  alongside it.
- **Implemented:**

  | Module | Contents |
  |---|---|
  | `io` | `Reader` / `Writer` primitives |
  | `batch` | framing; raw DEFLATE / Snappy / none; a 16 MiB decompression cap and 812 packets per batch |
  | `packet` | the varuint32 header with sub-client bits; `Packet` / `Encode` / `Decode` traits; IDs checked against Mojang's schemas |
  | `packets` | the handshake packets |
  | `login` | connection-request parsing and unverified identity claims, including the persistent UUID: the token's `leguuid`, otherwise vanilla's MD5 (v3) UUID of `pocket-auth-1-xuid:` + XUID; the legacy chain's `extraData.identity` |
  | `block` | `BlockState` and hashed network IDs |
  | `chunk` | `PalettedStorage`, `SubChunk` and the LevelChunk payload builder |
  | `nbt` | encode-only NBT in the network flavor |
  | `types` | `BlockPos` and `Vec3` |
  | `packets::spawn` | StartGame and the §3.5.1 packets |
  | `packets::text` | Text (ID 9): `[bool translate][varuint32 body variant][u8 type]`, then author (chat, whisper, announcement), message (1..=65536 bytes), parameters (translate, popups; ≤ 4), XUID, platform chat ID, optional filtered message. The variant must match the type. |

  | `packets::movement` | PlayerAuthInput, decoded up to the position delta, with the rest skipped; input flags kept as raw IDs, since Mojang's enum and gophertunnel's number them differently. MovePlayer, without teleports. |
  | `packets::entity` | AddPlayer, RemoveActor, PlayerList (a per-entry varuint32 variant plus an action byte), PlayerSkin, entity metadata, and `Skin` (2193 layout: u8 arm size, big-endian ARGB skin colour; see `skin` for reading it from login) |
  | `packets::block` | PlayerAction, PlayerAuthInput `BlockAction`s, UpdateBlock, and the action numbers (StartBreak 0, CreativeDestroyBlock 13, PredictDestroyBlock 26), confirmed by PocketMine. PlayerAuthInput now reads on to its block actions: it steps over an item interaction, gives up at an item stack request, and a tail it cannot parse only leaves the block actions unread. |
  | `types` (additions) | `Vec2`, and `uuid_bytes`: Bedrock's UUID order, two little-endian u64 halves |

  51 unit tests. They include a golden decode of a real client's first message, and FNV-1a
  checked against the reference vectors plus golden hash-input bytes. NBT decoding is not
  started yet.

### 4.5 `bedrockrs_core`

- **Game loop (first cut implemented):** `tick::TickLoop` runs `Server::tick` on a
  dedicated OS thread (`game-loop`) with a fixed 50 ms step, not a tokio task, to avoid
  scheduler jitter. The tokio runtime owns I/O.
  - Ticks follow a schedule rather than sleeping 50 ms after each one, so a slow tick is
    made up by quicker ones.
  - A tick over 50 ms logs a warning. More than 1 s behind, the loop skips the missed
    ticks instead of racing to catch up.
  - Dropping the `TickLoop` stops it after the current tick.
  - So far a tick only broadcasts movement. Sessions still write player state into
    `Players` behind a mutex rather than through channels; that changes once the
    simulation owns real state.
- **Tick phases:** drain inbound → simulate → dispatch plugin events → flush one batch per
  player. This gives natural batching and better compression.
- **World:** a single owner, as in Dragonfly, with a chunk map per dimension.
  - Sub-chunks use Bedrock's paletted storage.
  - Network block IDs are hashed, with `UseBlockNetworkIDHashes` set.
  - Chunks generate on a rayon pool, and results merge at the start of a tick.
  - Each player has a chunk streaming radius.
- **Metrics:** MSPT metrics, and bounded catch-up after tick overruns.
- **Entities:** an ECS. `bevy_ecs` standalone is the leading candidate; the choice is made
  when implementing core.
- **Sessions (implemented):** `session::Session` is a sans-IO state machine for the §3.5
  login and the §3.5.1 spawn. Its stages are RequestNetworkSettings → Login →
  Authenticating → ResourcePacks → Spawning → Initializing → InGame. It returns replies to send,
  compression to enable afterwards, and whether to close. `session::run` drives it over a
  NetherNet `Connection`.
  - Before the player is in the world, any protocol error ends with a Disconnect **with a
    message**, so the client shows a reason instead of timing out.
  - Once in the world, packets without a handler are ignored.
  - After a Disconnect it waits up to 5 s for the client to hang up.
  - Replies also carry `SessionEvent`s for the rest of the server: `Joined` on the first
    SetLocalPlayerAsInitialized, `Moved` when PlayerAuthInput reports a new position or
    rotation (in game only; non-finite values are ignored), and `Chat(message)`.
  - Each session gets its own entity ID from `Players::allocate_entity_id` (runtime ID =
    unique ID), used in its StartGame and by everyone who sees the player.
  - `session::run` handles each batch's packets in order from a queue of replies, so a
    reply can queue the next. Plugins hear `player_join` 750 ms (15 ticks) after the
    player spawns: messages that arrive while the client's HUD is still starting are
    shown twice.
- **Authentication (implemented):** `auth::Authenticator` verifies the multiplayer token
  in each Login, as gophertunnel does.
  - The token is a JWT from `https://authorization.franchise.minecraft-services.net/`,
    signed with RS256. The server checks the signature against the service's published
    keys (the OpenID configuration's `jwks_uri`, looked up by `kid`), then the issuer, the
    audience `api://auth-minecraft-services/multiplayer`, and `exp` and `nbf` with 60 s of
    clock slack. The token must name a player (`xid` and `xname`).
  - Keys are fetched on the first login and cached. An unknown `kid` refetches (at most
    once a minute), and keys are refetched after 6 h. Stale keys keep working if the
    service is unreachable.
  - Crypto and TLS add no new stack: RSA uses the AWS-LC build str0m already brings
    (`aws-lc-rs`, plus its `ring-io` accessors), and `reqwest` 0.13 uses rustls on the
    same AWS-LC with the OS certificate store.
  - The session waits in **Authenticating** while `run` awaits the verdict, then
    `Session::authenticated` continues. Only a verified token's identity is used: the
    XUID-derived UUID and the gamertag become the player's name and UUID for saves,
    other players and plugins.
  - The replay check (the token's `cpk` must be the key proven during signaling) runs on
    the verified key. A failure disconnects with NotAuthenticated (46), showing why.
  - `BEDROCKRS_AUTHENTICATION=false` switches to offline mode, which trusts every token,
    for testing without internet access. The server warns when it starts this way.
- **One session per player (implemented):** a verified login claims its UUID in
  `logins::Logins`. A newer login for the same UUID sends the older session a kick. That
  session shows "You logged in from another location" (reason 43,
  LoggedInOtherLocation), saves its player and closes. The claim is released when its
  session ends, unless a newer session holds it.
- **Players and chat (implemented):** `server::Server` holds the world, the
  `players::Players` registry and the plugin `Dispatcher`. Each session has a bounded
  queue (256 packets) that `run` drains alongside the connection, batching whatever is
  waiting. On `Joined` the player is registered first, then plugins get `player_join`, so
  a greeting reaches the new player too. A `Membership` guard removes the player on any
  exit.
  - Chat: only `TextType::Chat` is accepted, and only in game. The author is always the
    session's player; the packet's source name and XUID are ignored. Control characters
    become spaces, so nobody can fake a second line. Messages over 512 characters are
    refused with a warning to the sender. Everything else goes to everyone as Raw text
    `<name> message`, as Dragonfly does, so no player list is needed.
  - A full player queue drops that player's packet rather than stalling the others.
- **Visibility and movement (implemented):** `Players` keys players by entity ID and
  keeps each one's `Movement`: eye position, pitch, yaw, head yaw, and an on-ground guess
  (no vertical delta).
  - Joining puts everyone in everyone's player list at once, which also gives clients the
    skins before any entity appears.
  - **Entity tracker:** each player also has their client's `View` (centre chunk and
    radius, reported by the session whenever it recentres) and the set of entities their
    client has. Every tick, `Players::tick`:
    - sends AddPlayer (with the current position) to each viewer whose view now contains
      another player's chunk;
    - sends RemoveActor to viewers whose view no longer does;
    - then sends each player who moved since the last tick, as a MovePlayer (normal mode,
      eye position, the server tick), only to viewers who already had them.
    A player is never sent their own entity or movement. Either side moving updates
    visibility: a player flying back towards someone standing still reappears for them.
  - Leaving (the `Membership` drop) sends RemoveActor to those who saw the player, and a
    PlayerList removal to everyone.
  - AddPlayer uses the feet position (eyes − 1.62), as Dragonfly does. Its metadata,
    `players::player_metadata`, is: name; scale 1; a 0.6 × 1.8 bounding box; an
    always-shown name tag (key 81); and the flags HasGravity, HasCollision, Breathing,
    CanClimb, ShowName and AlwaysShowName. Gophertunnel and PocketMine number these flags
    the same way.
  - The same metadata goes to each player about **their own** entity, as a SetActorData
    just before PlayerSpawn. Without HasGravity the client does not pull its own player
    down: the first live test showed players drifting upward after they stopped flying.
  - Also during spawn, each player gets their own **UpdateAttributes** (`minecraft:movement`
    0.1, underwater and lava movement 0.02, health 20) and **UpdateAbilities** (a base
    layer defining all 20 abilities, granting the creative ones, at walk 0.1, fly 0.05 and
    vertical fly 1.0). The client moves its own player with these; without them walking
    was far faster than vanilla. Other players' AddPlayer carries the same base layer
    without granted abilities; its all-abilities mask used to miss bit 19 (vertical fly
    speed).
  - **Skins (implemented)** in `bedrockrs_protocol::skin`: each player's own skin, read
    from the ClientData JWT in their Login (image, resource patch, geometry, geometry
    engine version, animation data, animated images, cape, arm size, persona flag).
    - Checked before anyone else sees it, since a malformed skin can crash other
      clients: base64 must decode, every image must be at most 512 on a side with
      exactly width × height × 4 bytes, the geometry (at most 4 MiB) and resource patch
      must be JSON, and there are at most 16 animations. A skin that fails is replaced
      by a plain 64×64 placeholder coloured from the UUID, with a warning.
    - Character-creator skins are sent with their persona pieces (piece ID, type
      number from Mojang's `persona::PieceType`, pack UUID, default flag, product ID),
      piece tints (the type named as gophertunnel does, `persona_eyes` as `eyes` and
      `persona_hand` as `hands`, plus four colours) and skin colour, and their persona
      and premium flags. Without the pieces, clients drew them as default skins
      (found live on 2026-09-27; Dragonfly does not forward them). Colours are
      `#RRGGBB` or `#AARRGGBB` in the login and big-endian ARGB on the wire.
    - As in Dragonfly, the skin and cape get fresh IDs (the client's skin ID becomes the
      full ID), and the skin is sent trusted, overriding the appearance.
    - The ClientData signature is not checked: the connection already belongs to the
      client whose key the Login proved.
    - The skin travels in PlayerList. When a player's entity is shown to someone, AddPlayer
      is followed by PlayerSkin (93) and, if they hold something, MobEquipment, as
      Dragonfly does. AddPlayer carries the held item itself too, which the registry
      learns as the player joins: a hotbar slot chosen while the world was still
      loading used to be lost, leaving observers an empty hand (found live on
      2026-09-27).
- **Chunk streaming (implemented):** each session keeps a `view::ChunkView`: the chunk
  its player stands in, the granted radius (capped at 8), and the chunks its client has.
  - A RequestChunkRadius, or PlayerAuthInput crossing into another chunk (the feet's
    block, divided by 16 and rounded down), recentres the view. The session then sends a
    NetworkChunkPublisherUpdate at the player's block with radius × 16 blocks, followed
    by every chunk in the new circle the client lacks, nearest first.
  - Chunks that fall out of range are forgotten, because the client unloads them, so
    they are sent again on return. A one-chunk step sends only the circle's leading
    edge (about 17 chunks at radius 8).
  - Streaming happens in the session as input arrives, not in the tick loop. Chunks
    come from `FlatWorld`, so no generation cost is involved yet.
- **World (mutable, implemented):** `world::World` is an endless superflat overworld with
  vanilla's default layers (bedrock at y = -64, two dirt, grass at -61) in plains. New
  players spawn at (0, -60, 0), centred on the block.
  - Unchanged columns all share one generated column and its encoded payload.
  - The first change to a column copies it into a map of changed columns behind a
    mutex. Each changed column keeps its sub-chunk storages and a cached payload, which
    is dropped on change and rebuilt when the chunk is next sent. So streaming, and
    players arriving later, see the change.
  - A column sends sub-chunks up to its highest non-air one, filling any gaps with air.
  - `block` and `set_block` are bounded to y = -64..=319; `set_block` reports whether
    anything changed.
- **Block breaking (implemented):**
  - The session accepts breaks from PlayerAuthInput block actions (StartBreak, since
    everyone is in creative, and PredictDestroyBlock) and from a PlayerAction with
    CreativeDestroyBlock. It checks each one first: the block must be within the world's
    height, within 12 blocks of the eyes (creative reach is about 7.5), and in a chunk
    the client has.
  - `Server::break_block` sets the block to air. If that changed anything, it sends an
    UpdateBlock (network flag, layer 0) to every player whose view contains the chunk,
    the breaker included.
  - Breaking is handled as input arrives, not in the tick loop. The block's item pops
    out (see Item entities), even in creative mode, where vanilla drops nothing. There
    is no survival break timing, tool check or loot table yet: a block drops its own
    item.
  - Breaking also sends a LevelEvent 2001 (`DESTROY_BLOCK`, with the broken block's
    network ID) to the chunk's viewers: the breaking particles and sound.
- **Items (implemented)** in `items`: every vanilla item, loaded once from
  `data/items.json`, which `tools/item_data.py` generates from Dragonfly's vanilla data
  at its 1.26.50 commit (the same one the block palette comes from).
  - About 2,080 items: name, network ID, version, component-based flag with the
    components converted to network NBT, the largest stack, and for about 1,420 block
    items the block state they place (the state the creative inventory shows).
  - The data must match the client's version. PocketMine's BedrockData (1.26.30, its
    newest) was used first, but 587 network IDs changed by 1.26.50, and a live client
    (2026-10-08) put creative items by its own IDs, whatever the ItemRegistry said: a
    poplar boat as the harness group's icon, a cyan cushion among the hoes.
  - Largest stacks come from components where present, and otherwise from a table of
    suffixes and names in the script (tools and armour 1, pearls and signs 16, the rest
    64).
  - A block item's block is the one the client shows for a tick when it places it,
    until the server's answer arrives; vanilla's data gives walls neither a post nor
    sides, which flashed into view, so wall items hold a lone wall, a post (found live
    on 2026-10-08). The server sends each placed block once, already in its final
    state, and the transport adds about 0.2 ms; a block whose shape depends on its
    neighbours can still change shape a tick after the client shows it.
  - The creative inventory: 124 groups (category, name, icon) and 1,980 items, in
    vanilla's order. Creative network IDs count from 1. Education Edition items
    (elements, compounds and the like), which BedrockData's dump had, are not in
    vanilla's creative inventory.
  - 175 creative entries carry NBT (125 enchanted books, banners with patterns,
    fireworks and firework stars). Items carry it as user data: a length of -1, version
    1, then little-endian NBT, as gophertunnel writes it. The server treats it as opaque
    bytes, kept once each for the server's life (`items::intern_nbt`) so stacks stay
    `Copy`; stacks only merge with the same NBT, and it is saved with the player
    (base64). What a client sends an item to carry is never taken.
  - The ItemRegistry and CreativeContent packets are encoded once and shared.
- **Inventories (implemented)** in `inventory`: the server owns each player's 36 main
  slots (hotbar 0 to 8), four armour slots, the offhand, the cursor and the inventory
  screen's 2x2 crafting grid.
  - New players start with nothing; the creative inventory has everything.
  - **Opening the screen:** the inventory key sends Interact action 6 (open
    inventory), and the client shows the screen only once the server answers with
    ContainerOpen (window 0, type 0xFF, the player's block, entity -1). A second open
    while it is open is not answered: Dragonfly notes that opening twice crashes the
    client. The client's ContainerClose (47) for window 0 is echoed back (type 0, not
    server-side), and anything left on the cursor or in the crafting grid goes back into
    the inventory (onto stacks of the same item, then free slots, the rest thrown out),
    with the inventory, cursor and grid sent again;
    window 0xFF (inventory and chat together) just marks it closed. Without these, the
    screen never opens (found live on 2026-09-27).
  - Every stack has a stack network ID, unique per player. A whole stack that moves
    keeps its ID; a part that splits off, or a creative item landing, gets a new one.
  - **Item stack requests** (147) are applied whole or not at all, on a copy, and
    answered with ItemStackResponse (148): OK with the touched slots' counts and stack
    IDs, grouped by the container names the client used, or an error, which makes the
    client undo the request. A request that cannot be decoded is ignored.
  - Supported actions: take, place (onto nothing or the same item, up to its largest
    stack), swap, destroy and creative pick (creative only), plus the informational
    craft-results and mine-block actions. Drop (from any slot, cursor included) takes
    the items out and the world throws them (see Item entities). Consume, create and
    crafting are rejected for now.
  - **Crafting grid:** container 13 (`CRAFTING_INPUT`), slots 28 to 31 of the UI
    window, as Dragonfly numbers them (a crafting table's 3x3 grid is 32 to 40). Items
    can be put in and taken out; nothing is crafted yet. Saves count grid items as back
    in the inventory, as for the cursor.
  - **Armour:** armour slots only take what is worn there (`items::armor_slot`:
    helmets, skulls, heads and the carved pumpkin on the head; chestplates and the
    elytra on the chest; leggings; boots); a request breaking that is rejected. Using
    armour in the air (UseItem, click air) swaps it with what is worn, as Dragonfly
    does: the client predicts the swap, and before this the server ignored it, so the
    next click on the armour slot was rejected and the item snapped back (found live,
    2026-10-08).
  - **Legacy requests:** a transaction the client already carried out on its side
    (armour put on by using it, an item thrown from the HUD) carries a legacy request
    ID and the slots it changed. As gophertunnel documents, the server must answer
    with an ItemStackResponse for that ID listing what those slots hold; until then
    the client keeps them locked. And as after any request, the client goes on naming
    the stacks in those slots by the request's ID, so the stack IDs they hold are
    remembered under it with the other recent requests. Before both, a helmet put on
    by using it could not be clicked, dropped or moved until the player rejoined
    (found live, 2026-10-08). Dragonfly only resends the slots, without the response.
  - Whichever way armour goes on, its sound plays where the player stands
    (LevelSoundEvent `armor.equip_<material>`, generic for others, by Dragonfly's
    mapping), and others see it (MobArmorEquipment, 32), as do players who come into
    view later. Armour does not protect yet.
  - **Pick block** (BlockPickRequest, 34; middle click): `Items::pick` maps the block
    to the item vanilla gives: a table for blocks whose item has another name (crops
    give seeds, redstone wire redstone, plain signs the oak sign, candle cakes cake),
    nothing for air, liquids, fire and portals, double slabs their slab, then the
    same-name item if the creative inventory has it, then lit, powered, inverted and
    wall forms stripped (`lit_furnace` → furnace, `spruce_wall_sign` → spruce sign),
    and last the same-name item even if it is not in the creative inventory (barriers,
    command and light blocks). A test checks every block in the palette. As in
    Dragonfly, an item already on the hotbar is held; one elsewhere swaps with the held
    slot; otherwise creative players get one in the first empty hotbar slot (or the held
    slot, its stack moving to the first empty slot). PlayerHotBar (48) switches the
    client's slot. Ctrl (`add_block_nbt`) should copy the block's block entity data,
    but there are no block entities yet.
  - Each slot names the stack the client believes is there: its stack ID, 0 for an
    empty slot, or a negative request ID. The client sends requests without waiting
    for answers, so a request ID stands for the stack that request (this one or an
    earlier one) left in the slot. As in Dragonfly, the stack IDs each of the last 64
    accepted requests left behind are remembered to resolve these. Painting (drag
    splitting), double-click gathering and the stash on closing all depend on it
    (found live on 2026-09-27).
  - After a rejected request, the server also sends the whole inventory
    (InventoryContent for windows 0, 119 and 120, and InventorySlot for the cursor,
    slot 0 of UI window 124). The client undoes a rejection by itself, but a rejected
    drop once left a slot unusable. The same is sent for an item stack request that
    cannot be read and for any inventory transaction other than item use, since
    neither can be answered.
  - Containers: hotbar 28, inventory 29, combined 12 (all the main slots, by index),
    armour 6, offhand 34 (slot 1, or 0), cursor 59, created output 60 (slot 50), as
    gophertunnel and PocketMine number them. A 1.26.51 client used these on
    2026-09-27. Mojang's schema lists the names in declaration order (hotbar 31, cursor
    62), but RecipeFood, RecipeBlocks and RecipeFurnaceItems have the values 64 to 66.
  - A creative pick puts a full stack of the item in the created output, which lasts
    only for the request.
  - PlayerAuthInput can carry a request (a tool's durability while mining); it is read
    and answered like the others, and the block actions after it are no longer lost.
  - Saved in `players/<uuid>.json` under `inventory` (`main`, `armor`, `offhand`, each a
    list of `{slot, item, count, meta}` by item name). An item on the cursor is saved
    into the first free slot. Unknown items and bad slots are skipped with a warning;
    counts are capped at the item's largest stack. Files from before inventories start
    empty.
  - The session owns the inventory; each accepted change sends a snapshot to the
    player's `Players` entry, so the periodic saves include it.
  - Closing the screen with an item on the cursor and no free slot throws it out.
  - **Throwing from the HUD** (Q, Ctrl+Q) does not use item stack requests: it sends an
    InventoryTransaction of the normal type (0) with two actions, a world action (source
    2, slot 0) whose new item carries the thrown count, and the inventory slot (source
    0, window 0) whose old item is what it held. As in Dragonfly, the throw happens only
    if the slot holds at least that many of that item, and the client is always sent
    its inventory afterwards (found live on 2026-09-27).
  - **Held items:** the client sends MobEquipment (31) when the player picks another
    hotbar slot. Whenever the held stack changes (another slot, or the stack in it
    moved, thrown, picked up or used up), everyone who sees the player gets
    MobEquipment with it, and AddPlayer carries it for players who start seeing them.
    Other clients draw held items at the skin model's `rightItem` and `leftItem` bones,
    so the placeholder skin defines them as vanilla's humanoid model does; without them
    arms rose but items were invisible (found live on 2026-09-27). Throwing items
    swings the thrower's arm for everyone who sees them.
- **Item entities (implemented)** in `entities`: stacks lying in the world, held by
  `Server::items` and advanced by the game loop.
  - Physics, as Dragonfly: gravity 0.04 and drag 0.02 per tick, and ground friction
    0.6. The item falls first, then slides one axis at a time; any non-air block stops
    it. An item on the ground that has stopped is left alone.
  - Thrown items start 1.4 blocks above the thrower's feet and fly at 0.4 blocks a
    tick the way they look, with a 2-second pickup delay. Block drops pop up from the
    middle of the block with a little sideways scatter and a half-second delay.
  - Items vanish after 5 minutes, or 64 blocks below the world. They are not saved.
  - Each player's `Players` entry tracks the items their client has. Every tick,
    items in view that it lacks get AddItemActor (15; the stack, position, velocity,
    and metadata flags HasGravity and CanClimb with a 0.25 box). Items gone or out of
    view get RemoveActor. Those that moved get MoveActorAbsolute (18) and
    SetActorMotion (40). Positions are sent 0.125 higher, as clients draw items there.
  - **Pickup** happens once a tick in each session, which owns the real inventory: an
    item past its delay whose box, grown by 1 sideways and 0.5 vertically, touches
    the player's box gives the player as much as fits (onto matching stacks, then
    into empty slots, hotbar first). The rest stays; an item whose count changed is
    shown again. Everyone who sees it gets TakeItemActor (17), the item flying to the
    player (the client plays the pop), then RemoveActor on the next tick. The player
    is sent their inventory.
  - Not yet: merging nearby items, plugin events for dropping and picking up,
    `player.give`.
- **Falling blocks (implemented)** in `falling`, held by `Server::falling`: blocks
  falling as entities, until they land and are blocks again.
  - As vanilla and PocketMine: gravity 0.04 and drag 0.02 per tick, straight down from
    the bottom centre of the block, 0.98 wide and tall. It lands on the top of the
    highest collision box under it (`shape::collision`; a fence's top is at 1.5), and
    goes in where its bottom is if that block is replaceable and it stays up there
    (scaffolding working out its stability), with the place sound. Otherwise, as on a
    torch or a bottom slab, it breaks into its item, if `dotiledrops` is on. Still
    falling after 30 seconds, or 64 blocks below the world, it breaks too. They are
    not saved.
  - Shown like item entities, through the same per-player tracking: AddActor (13) of
    `minecraft:falling_block`, 0.49 above its bottom (half its height, as PocketMine
    sends it), with metadata Variant (2, an int) the block's network ID and a 0.98
    box, but no flags: with HasGravity, clients fell it on their own, out of its
    block while held and into the ground while it rested there, where it turned black
    (found live on 2026-10-08), so they only follow the positions sent; then MoveActorAbsolute and SetActorMotion every tick. When a
    block is found about to fall, its falling block is shown at once inside it, held
    still, and the block stays (`Server::about_to_fall`, `FallingBlocks::hold`). On the next check,
    if it still falls, the block's UpdateBlock (air) goes out and the entity is let go
    (`FallingBlocks::release`); it moves its first
    bit down that tick; if not, the held entity is removed. Clients take a frame or
    two to draw a new entity: added only as the block went, there was a gap of a few
    frames with neither, then the entity appeared already lower; added before the
    block was emptied in the same batch, it was the same; with the block going a few
    milliseconds after it was placed (12 ms in a log), clients had hardly drawn it
    (all found live on 2026-10-08, frame by frame). Right after its AddActor, a
    MoveActorAbsolute with the teleport flag puts it in place, so clients do not glide
    it in from wherever they would otherwise start it. Landing, it is moved onto the
    ground (on-ground flag) and stays there a tick before it becomes its block:
    clients draw entities a little behind, and it snapped down from where they still
    showed it when it became the block the tick it arrived. Landing, the entity is removed
    (`Players::hide_entity`) before its block goes in, for the same reason.
  - Writing this found grass blocks had no collision (`shape::is_soft` took any name
    with `grass` for a plant, and nether stems, mushroom blocks, mangrove roots, rooted
    dirt and flower pots likewise), so sand fell through the ground.
- **Block placing (implemented):** placing arrives in InventoryTransaction as a UseItem
  transaction with action ClickBlock; only that part is decoded.
  - The decode fails soft: an unreadable transaction is ignored.
  - The block goes against the clicked face, or into the clicked block if that is
    replaceable (air, liquids, fire, short grass, ferns, dead bushes, vines, glow
    lichen, roots, light blocks), as if on top of the block below. It is the block the
    server's stack in the hotbar slot places, which must be the item the client says
    it holds: a block item's own block, or for other items the block that picks as
    them (`Items::placed_block`, built from the pick rules): doors, beds, cake,
    cauldrons and hanging signs by their own name, signs and banners as standing or,
    against a wall, wall blocks, seeds as their crop, redstone as wire, string as
    tripwire, glow berries as cave vines, repeaters and comparators unpowered. A wall
    form goes on walls for block items too (coral fans). Creative placing uses nothing
    up. Its state comes from the placement rules (see Block states and placement).
    Each block placed must go somewhere reachable, in a loaded chunk, replaceable, and
    stay up (see Support), and none of its collision boxes (see Shapes) may be inside
    the placing player's own body (a quick check in the session). Kelp and seagrass
    only go into water.
  - What a block is fixed to is the clicked face if that holds it; otherwise, as
    vanilla tries each way a block can go, the same spot is tried on the floor, each
    wall, then the ceiling, and the first that holds wins. A torch clicked onto the
    side of a torch stands on the floor beside it, or is refused if nothing holds it
    there (found live on 2026-10-08: torches stacked onto torches). Ladders, wall signs,
    wall banners and tripwire hooks only go on walls.
  - **Doors, trapdoors and fence gates** open and close when used (UseItem on them),
    unless the player sneaks, which places against them instead; iron ones do not
    (redstone does not exist yet). Both halves of a door move together, a gate swings
    away from the player opening it (Dragonfly), and the `door.open`,
    `trapdoor.close`, `fence_gate.open`, and so on sound plays.
  - `Server::place_blocks` refuses a block whose collision boxes are inside **any**
    online player's body; a torch or flower at someone's feet, or a fence post or pane
    beside them, is fine, and so is scaffolding, which bodies climb through.
- **Shapes (implemented)** in `shape`: block collision boxes, by name and state as
  vanilla shapes them, for whether a block may go where a player stands. Torches,
  levers, buttons, plates, signs, banners, rails, redstone, plants, vines, frames and
  doors have none; fences, walls, panes and bars a post plus arms to their
  connections (1.5 tall for fences and walls); chains and rods a thin pillar along
  their axis; closed fence gates a bar across, open ones none; trapdoors a 3/16 slab,
  at the bottom, the top, or upright when open; slabs their half; ladders a 3/16 slab
  against their wall; carpets, snow layers, beds, repeaters and other low blocks their
  height; lanterns, candles, skulls, cake, cactus and chests a smaller box; anything
  else solid, and stairs, a full block. A player's body is 0.6 wide and 1.8 tall (1.5
  sneaking).
    `Players::occupies` uses a box 0.6 wide and 1.8 tall (1.5 while sneaking); touching a
    face is fine. A block inside another player traps them, and their client fights it
    with rapid snapping (found live on 2026-09-26).
  - Otherwise each block goes in only where what it replaces still is, checked under
    the world lock; the two halves of a door or bed go in together or not at all. It
    then sends viewers an UpdateBlock for each and a LevelSoundEvent `place` (sounds
    are named by string in 2193).
  - The client places a block before hearing back, so a refused placement is undone
    whatever the reason (places nothing, spot taken, no valid state, nothing holding
    it up): the placer gets an UpdateBlock with what is really there where the block
    would go, at the clicked block, where the client may have predicted a double slab,
    and above and beside the target, where it may have predicted a second half. Each
    placement and refusal is logged at debug level with the state and reason.
- **Support (implemented)** in `support`: what a block needs to stay where it is.
  Without shape data, shapes are judged by name, as for connections: `solid` blocks
  (fences and walls connect to them; plants and flowers are not), `full_cube`s (every
  face sturdy) and `sturdy` faces (also a top slab's top, a bottom slab's bottom, and a
  stairs block's back unless it is an outer corner).
  - Ladders, buttons, tripwire hooks, wall torches, levers and amethyst need a sturdy
    face behind them; wall signs, wall banners and item frames a solid block. Torches,
    lanterns, candles and pressure plates stand on a sturdy top or a fence or wall post;
    doors, rails, redstone wire, repeaters, comparators, snow and floor coral fans on a
    sturdy top; carpets, cake and turtle eggs on anything; standing signs and banners
    on a solid block. Hanging lanterns and signs, cave and weeping vines, spore
    blossoms and hanging roots need the block above; pointed dripstone the block it
    grows from; cocoa a jungle log; bells the block they stand on or hang from.
  - Plants need their soil: flowers, grass, saplings and bushes grass or dirt-likes;
    crops farmland; nether wart soul sand; fungi and roots nylium, soul soil or dirt;
    dead bushes and dry grass sand, terracotta or dirt; azaleas, propagules and
    dripleaves dirt or clay; mushrooms and leaf litter any full block; sugar cane sugar
    cane, or dirt or sand with water beside it; cactus sand with nothing solid beside
    it; bamboo bamboo, dirt, sand or gravel; kelp kelp or a sturdy top; lily pads and
    frogspawn water.
  - Blocks made of faces (`support::held_faces`, as PocketMine's): each face of glow
    lichen, sculk vein or resin clump needs a sturdy face behind it; each side of a vine
    a sturdy face, or the same side of a vine above it, so vines hang down walls. Faces
    nothing holds drop off; with none left the block breaks. Vines only go on walls.
  - Scaffolding keeps its `stability` as vanilla keeps its distance
    (`support::scaffolding_stability`): the fewest sideways steps through scaffolding
    to scaffolding standing on a sturdy floor, going down columns for free (so 0 on
    the floor, that of the column under it, one more beside). Vanilla works it out
    from the neighbours' stored values, so a bridge cut from its column counts up a
    step a tick before falling; a search through the structure (up to 4,096 blocks,
    then the stored values) knows at once, so it breaks block by block outwards from
    the cut (found live on 2026-10-08, when a whole layer went at once). It cannot be
    placed at 7, except by reaching out (below); at 7 it breaks with its item, or, if
    it was already at 7, falls.
  - **Reaching out with scaffolding** (`placement::scaffolding_extension`): placing
    scaffolding onto scaffolding grows it. Clicking a side of the scaffolding reaches
    out of that side; clicking its top, or the block it stands on (placing where the
    scaffolding is), stacks it on top of the column (found live on 2026-10-08: Java
    reaches out along the look when the top is clicked, which is not how Bedrock
    plays). Sneaking, it goes out of the clicked face. It goes past the scaffolding
    already there, up to 7 sideways, into the first replaceable spot, however far
    above (only the clicked scaffolding must be in reach). Placed too far out, it
    falls the next tick and lands. The client also gets the block beside the click,
    where it may have predicted it instead.
  - **Gravity blocks** (`support::falls`): sand, red sand, gravel, the suspicious
    ones, concrete powder, anvils and the dragon egg fall when the block under them
    is one a block replaces (air, liquids, grass). Not yet: concrete powder setting in
    water, anvils hurting what they land on.
  - Two-block blocks need their other half: a door's halves (the lower on a sturdy
    floor), a tall flower's or tall grass's, a pitcher plant's or small dripleaf's, and
    a bed's head and foot (which also need the floor when placed). Only one half drops
    the item: the lower one, or the bed's foot.
  - After every change (placing, breaking, opening), the six neighbours' connections
    and shapes are recomputed; a neighbour that changes has its own neighbours
    recomputed in turn, until nothing changes (a wall gaining a connection makes the
    wall under it tall: found live on 2026-10-08, when stacked walls kept stale short
    sides and posts until something beside them changed). The neighbours are then
    checked on the next tick (`Server::check_blocks`, which `Server::tick` runs, with
    `support::settled`): any no longer supported breaks, with its other half, with
    particles, and drops its item if the `dotiledrops` game rule is on; vines and lichen
    lose faces and scaffolding changes stability; gravity blocks over air, and
    scaffolding already too far out, start falling (see Falling blocks). Changed
    blocks are checked too, so sand placed over air falls. Its own neighbours are checked the
    tick after, so a column of cactus, carpet or scaffolding falls one block a tick from
    the bottom up, as in vanilla (found live on 2026-10-08: a whole stack used to go at
    once). Breaking
    either half of a two-block block breaks both, for one item. Broken blocks drop the
    item that places them (a torch for a wall torch, a door for either half, seeds for
    a crop), also only with `dotiledrops`.
  - A test places every item that places a block (over 1,400) in typical setups (on
    grass, farmland, sand, soul sand and stone, against a wall, under a ceiling, on a
    jungle log, on sand by water): each must place, with every part held up, except
    kelp, seagrass, lily pads and frogspawn, which need water.
- **Block states and placement (implemented)** in `blocks` and `placement`.
  - `data/blocks.json` (from Dragonfly's 1.26.50 palette via `tools/block_data.py`)
    lists every block's states and their values: 1,477 blocks, 22,091 states, exactly
    every combination. The world's table of known states is the whole palette.
  - A block's network ID hashes its exact states, so a state the client does not know
    is invisible to it: the server keeps a block the client shows as air, and the spot
    becomes unusable. PocketMine's 1.26.30 data lacked states 1.26.50 added (stairs
    `minecraft:corner`, `minecraft:connection_*` on fences, panes and iron bars), which
    did exactly this (found live on 2026-09-27). Now block items' states are upgraded
    to the palette's when the item data is generated, every placed state is checked
    against the palette, and chunks saved with older states are upgraded as they load
    (unknown states dropped, missing ones defaulted: `none` or `unknown` if the state
    has it, otherwise the smallest value).
  - Placement rules, by the states a block has (as Dragonfly places each):
    - `pillar_axis`: the clicked face's axis.
    - `weirdo_direction` (stairs): the way the player looks (east 0, west 1, south 2,
      north 3); `upside_down_bit` and `minecraft:vertical_half` (slabs): the upper half
      when placed on a ceiling or the upper half of a side.
    - `minecraft:cardinal_direction` (chests, furnaces, …): facing the player; doors
      the way the player looks turned right (Dragonfly), with `door_hinge_bit` set
      when a door is on their left so the two open as a pair; fence gates the way the
      player looks. `direction`: the way the player looks (south 0, west 1, north 2,
      east 3); cocoa faces its log, tripwire hooks and bells or grindstones on walls
      face out of the wall; trapdoors face away from the player, so they open towards
      them, numbered as Dragonfly does (3 minus its north 0, south 1, west 2, east 3,
      which is PocketMine's 5 minus the face; found live on 2026-10-08, the legacy
      numbering opened them away, a wrong guess at Dragonfly's order sideways).
      Glazed terracotta only turns around the vertical, facing the player. `ground_sign_direction`: the player's yaw in sixteenths.
    - `hanging`: lanterns, hanging signs and pointed dripstone hang when placed on a
      ceiling (hanging signs do not stand on floors); `attachment` (bells, grindstones):
      standing, hanging or side by the clicked face; `lever_direction`: the clicked face,
      with the axis the player looks along on floors and ceilings;
      `vine_direction_bits` (south 1, west 2, north 4, east 8) and
      `multi_face_direction_bits` (glow lichen and the like): the face fixed to, added
      to the faces of the same block already there (clicking one adds a face on the
      block the player looks towards most);
      `coral_direction` (wall coral fans: west 0, east 1, north 2, south 3, as
      PocketMine numbers it); `rail_direction`: along the look; `orientation`
      (crafters): facing the player, with their top up; placed leaves are
      `persistent_bit`; a lone big dripleaf is its head; dripstone is a `tip`. Hoppers
      point into the clicked block, or down; stems keep pointing at no fruit.
    - `facing_direction` and `minecraft:facing_direction`: the clicked face for things
      that attach (ladders, end rods, lightning rods, amethyst, wall signs and banners,
      tripwire hooks, buttons, item frames, skulls and heads, which do not hang from
      ceilings); otherwise towards the player, up or down when they look more than
      45° down or up; observers the other way. `minecraft:block_face`: the clicked face.
    - `torch_facing_direction`: `top` on floors, otherwise opposite the clicked face;
      torches cannot hang from ceilings.
    - Stairs corners from the stairs in front and behind (Dragonfly's rules).
    - Connections: fences (wooden with wooden, nether brick with nether brick) connect
      to fences, fence gates and solid blocks; panes and iron bars to each other, walls
      and solid blocks; walls to walls, panes, bars, fence gates and solid blocks. As
      Dragonfly does, a wall's connection is `tall` where the block above covers it (a
      full block, the bottom of a slab or stairs, a wall or thin block connecting that
      way) and otherwise `short`; it has a post unless it runs straight through, or
      when a standing torch, lantern or sign, or another wall's post, is on top. Fence
      gates between walls are lowered (`in_wall_bit`). Neighbours above and below are
      updated too, so a block placed on a wall makes it tall (found live on
      2026-10-08: a wall under a full block stopped short of it). Without shape data,
      "solid" rules out a list of known non-full blocks (see Support).
    - Slabs double up as Dragonfly does: a slab placed on the open half of a matching
      slab (the top of a bottom one, the bottom of a top one), or into a spot a
      matching half slab fills, turns that slab into its double slab in place
      (`minecraft:oak_slab` → `minecraft:oak_double_slab`; copper ones are
      `…double_cut_copper_slab`). The palette pairs all 101 slabs. The merge is placed
      over the exact slab it replaces, and refused if a player is inside it. Breaking
      a double slab drops two slabs.
    - Two-block blocks place both halves: doors and tall flowers and grass the upper
      half above, beds the head in the direction the player looks; both must fit.
      Signs, banners and beds are placed, but what block entities hold (sign text and
      its edit screen, banner patterns, bed colours) waits for block entities, so beds
      show the default colour.
  - After any block is placed or broken, the neighbours whose connections or corners
    depend on it are recomputed and sent to everyone with their chunks.
  - Breaking drops the item named like the block, whatever state it was in.
- **Swings and sneaking (implemented):** these come from input flags.
  - MissedSwing (bit 39), a break or a placement sends an Animate arm swing to everyone
    who sees the player. So do the client's own Animate swings (such as punching an
    entity, swing source `attack`), except those from building, mining and dropping,
    which are already shown.
  - StartSneaking and StopSneaking (bits 27 and 28) toggle the Sneaking entity flag and
    a 1.5-block height. The resulting SetActorData goes to viewers and to the player
    themselves, which Mojang's docs say the client expects.
  - AddPlayer carries the current sneaking state.

- **Persistence (implemented):** the world talks to storage only through the
  `storage::WorldStorage` trait. It lists saved chunks, loads and saves a chunk column,
  and loads and saves a player. `World::with_storage` takes any backend. `World::open(dir)`
  uses `BinStorage`, BedrockRS's own format; the binary reads the directory from
  `BEDROCKRS_WORLD_DIR` (default `worlds/world`, which is git-ignored). `World::new` stays in
  memory, for tests.
  - **Blocks cross the trait by name**, as a palette of block states (name and states)
    plus 4096 indices per sub-chunk, the way vanilla worlds store them. So a LevelDB
    backend needs no knowledge of network IDs.
  - The world maps its network IDs (state hashes) back to names through a table of every
    block it can hold: air, the superflat layers and every block item's state.
  - A block with no known name is stored as a raw placeholder,
    `bedrockrs:raw_network_id` with its ID as a state, so nothing is lost.
  - `BinStorage` writes one file per changed column, `chunks/c.<x>.<z>.bin`: the magic
    `MVCH`, a version byte, then zlib data. Format **version 2**, written now, stores per
    sub-chunk a palette of names and states and u16 indices; `storage.rs` documents the
    layout.
  - **Version 1** files (u32 hashes per block) still load, as raw placeholders, and are
    rewritten as version 2 the next time their chunk changes.
  - Writes go to a `.tmp` file that is renamed over the old one, so a crash never leaves
    half a chunk. There are no new dependencies: `flate2` was already in use, and SQLite
    (C) or `sled` (pre-1.0, dormant) were not needed for per-chunk blobs.
  - Opening lists the saved chunks without reading them. A saved chunk is loaded the
    first time it is read, changed or sent. A file that cannot be read is logged, and the
    chunk is generated instead.
  - Changes mark a column dirty. The tick loop saves dirty columns every 100 ticks (5 s),
    and once more when it stops (Ctrl+C). Columns are copied out under the lock and
    written after it is released, and a failed write stays dirty for the next save.
    Killing the process loses at most 5 s of changes.
  - **Players:** `players/<uuid>.json` holds the feet position, pitch, yaw, head yaw and
    whether the player was flying (`flying` defaults to false, so older files still
    load).
  - A player is saved when their session ends, however it ends; everyone online is also
    saved on every world save (5 s) and at shutdown.
  - At login the session loads the file, so StartGame, the first chunks and the entity
    others see all start there. A file that cannot be read, or a position outside the
    world's height, is ignored and the player starts at the spawn.
  - **Flying:** the session follows StartFlying and StopFlying (input bits 42 and 43) and
    answers each with UpdateAbilities, as Mojang's docs say the client expects. The
    abilities sent at spawn and in those answers include the Flying value while the
    player flies, so a player who left in the air stays in the air.
- **Storage format and vanilla worlds (decided 2026-09-26):** keep the `.bin` format for
  the MVP. The trait and the name-based palettes are in place; a LevelDB backend for
  vanilla worlds comes later. The reasoning is in §7.
- **Game modes (implemented)** in `game_mode`: survival, creative, adventure and
  spectator, per player. A player's mode is saved with them (by name, in the player
  file) and new players get `[players] default_game_mode`. The mode decides:
  - abilities (UpdateAbilities), as vanilla grants them: only creative and spectator
    may fly, spectators always fly and pass through blocks;
  - breaking: creative breaks on the first hit (StartBreak, CreativeDestroyBlock);
    survival breaks when the client predicts the block destroyed, without checking
    the breaking time yet; adventure and spectator change no blocks;
  - placing: outside creative one item is used up (an InventorySlot keeps the client
    in step); the creative inventory and its item stack actions are creative only;
  - drops: only survival and adventure breaks drop the block's item;
  - presence: spectators pick nothing up and nobody sees their entity.

  A change goes to the player as SetPlayerGameType plus UpdateAbilities, and to
  everyone else as UpdatePlayerGameType. Health, hunger and damage do not exist yet,
  so survival players cannot be hurt.
- **Health and damage (implemented)** in `damage`, modelled on vanilla so plugins
  can drive them:
  - `Health { value, max }` is the `minecraft:health` component (players: 20/20),
    shown to the client as the `minecraft:health` attribute, rounded up.
  - `DamageCause` is vanilla's full list, by the Script API's names (`fall`, `void`,
    `selfDestruct`, `entityAttack`, …). Creative players and spectators are spared
    everything but `selfDestruct` (`/kill`) and `override` (health set directly).
  - **Falls:** the session adds up how far the player drops between inputs; landing
    is the client's `VerticalCollision` input flag (50) while not rising. Damage is
    `ceil(distance - 3)` from half a point, as Dragonfly and vanilla work it out;
    rising, flying or an immune mode starts the count over. Breaking time and
    movement are still trusted.
  - **The void:** below y = -64, 4 damage every 10 ticks.
  - **Regeneration:** the world is peaceful (StartGame difficulty 0), where players
    regain a point every 20 ticks while `naturalregeneration` is on. Hunger comes
    with difficulties.
  - **Order:** the session proposes damage (`SessionEvent::Damage`); plugins hear
    `player_damage` and may cancel it (2 s at most, like chat); then
    `Session::apply_damage` deals it: UpdateAttributes and ActorEvent(Hurt) to the
    player, ActorEvent(Hurt) to whoever sees them.
  - **Death:** health 0, ActorEvent(Death) and DeathInfo (the death-screen message)
    to the player; others see them fall over and lose sight of them after 20 ticks.
    Unless `keepinventory`, everything they carry is dropped at their feet. The
    death message goes to everyone as a translation (`%death.attack.fall`, …) when
    `showdeathmessages` is on, and to the log in English. The dead ignore movement,
    breaking and placing.
  - **Respawn:** the death screen's button sends Respawn(ClientReadyToSpawn); the
    server answers Respawn(ReadyToSpawn) at the world spawn with full health, as
    Dragonfly does, and streams the chunks there. A player who left while dead
    comes back at the spawn.
- **Game rules (implemented)** in `game_rules`: `falldamage`, `keepinventory`,
  `naturalregeneration`, `showcoordinates` and `showdeathmessages`, saved as
  `game_rules.json` in the world directory (defaults as vanilla, except
  `showcoordinates`, which the server has always turned on). They go to clients in
  StartGame and, after `/gamerule`, in GameRulesChanged.
- **Session loop:** packets, the player's tick and server controls all produce
  `Reply`s; `Link::deliver` sends their packets and carries out their events in one
  place, including events that lead to further replies (authentication, commands,
  damage after plugins).
- **Session controls (implemented)** in `logins`: the per-UUID channel that kicked
  duplicate logins carries a `Control` enum: `Kick`, `SetGameMode`, `SetOperator`,
  `RefreshCommands`, `SetHealth`, `Damage` and `GameRules`. Commands and plugin actions reach a player's session through it,
  and the session applies them between packets.
- **Commands (implemented)** in `commands`. Every command, built-in or from a plugin,
  is a `CommandSpec`: a name, description and aliases, and a tree of `CommandNode`s.
  A node may run, with one or more overloads of typed `ArgSpec`s (string, text, int,
  number, bool, player, enum) tried in order, and may lead to named subcommands, to
  any depth; each node has a permission (any or operator), which subcommands can only
  narrow. Plugin nodes have one overload each, since each has one handler.
  - **Parsing** (`commands::parse`): words, with double quotes for names with
    spaces. Subcommand names win over arguments; the node reached must run; each
    argument is parsed to its type (players by name, any case, or `@s`), and every
    mistake is answered with what was wrong and the usage line.
  - **Clients** (`commands::client`): each runnable path becomes an overload of
    AvailableCommands, its subcommands as one-value enums (as Dragonfly does) and
    then its arguments. Players only get the commands and subcommands they may run.
    It is sent after joining and again when their operator status or the plugin
    commands change.
  - **Running:** CommandRequest → `Server::run_command` → a `CommandReply`, sent back
    as CommandOutput with the request's origin. Built-in commands run in place;
    plugin commands go to the plugin thread (`Dispatcher::run_command`) and wait at
    most 2 s for its reply.
  - **Built-in** (`commands::builtin`): `help [command]`, `list`, `version`, and for
    operators `gamemode <gameMode> [player]`, `gamerule [rule] [value]`,
    `kill [target]`, `op <player>`, `deop <player>`, `stop`.
    `gamemode` matches vanilla: the client merges our `GameMode` enum with its own,
    so it carries exactly vanilla's names (`survival`, `creative`, `adventure`,
    `spectator`, `s`, `c`, `a`, `default`, `d`), and 0, 1 and 2 work through a
    second, whole-number overload rather than being listed.
  - **Names:** built-in commands win; plugin commands follow in plugin name order, and
    a command or alias whose name is taken is left out with a warning.
- **Operators (implemented)** in `ops`: `ops.json` in the working directory lists
  them by UUID (with their name at the time, for people reading it). Operators get
  operator commands and player permission level 2 (ability `OperatorCommands`). The
  console makes the first one with `op <player>`; players must be online to be made
  operators, since only their session knows their UUID.
- **Server console (implemented)** in `main.rs`: lines typed into the console run as
  commands with every permission, with or without their `/`; output is logged.
  `stop` (or `/stop` from an operator) shuts down as Ctrl+C does.

  Generation beyond superflat and entities are not started yet.

### 4.6 `bedrockrs_plugins`

- **Threading:** each engine runs on its own OS thread, because the VMs are `!Send`, with
  one isolated VM per plugin. Luau's thread is named `luau-plugins`. A `ScriptEngine`
  trait will be extracted when the second engine arrives, so its shape comes from real
  needs.
- **Messaging only (implemented):** game → plugins for `Event`s, plugins → game for
  `Action`s. Plugins have no direct access to the world.
  - `Dispatcher::dispatch(Event)` queues an event for the plugin thread, which calls
    every handler plugin by plugin in name order. Each handler call gets the full
    execution limit, and a failing handler is logged without stopping the others.
  - `Dispatcher::dispatch_cancellable(Event)` does the same and resolves to whether a
    handler cancelled the event. Core waits at most 2 s for the verdict, then goes
    ahead, so a stuck plugin cannot silence chat. Only `player_chat` is cancellable.
  - Actions go through a bounded channel (1024) that core drains. When it is full,
    the calling API function raises a Lua error instead of blocking.
  - Players are named by UUID string: the persistent identity. The XUID is never
    exposed. Every table a handler receives is read-only.
  - Reload debouncing uses its own deadline, so a steady stream of events cannot
    postpone reloads.
- **Events (implemented):**

  | Event | Handler receives | When |
  |---|---|---|
  | `player_join` | `{ player, cancel(), is_cancelled() }` | 750 ms after SetLocalPlayerAsInitialized, so once the client is past the loading screen and its HUD shows messages once; unless cancelled, everyone then sees vanilla's `§e%multiplayer.player.joined` |
  | `player_quit` | `{ player, cancel(), is_cancelled() }` | when the session of a player whose join plugins heard ends, however it ends; unless cancelled, everyone sees `§e%multiplayer.player.left` |
  | `player_chat` | `{ player, message, cancel(), is_cancelled() }` | before a chat message is relayed; `cancel()` stops it, and later handlers still run and can check |
  | `block_break` | `{ player, position = { x, y, z }, block }` | after a player broke a block; `block` is the broken block's name |
  | `block_place` | `{ player, position = { x, y, z }, block }` | after a player placed a block |
  | `player_damage` | `{ player, cause, amount, health, cancel(), is_cancelled() }` | before damage is dealt |
  | `player_death` | `{ player, cause, message }` | after a player died; `message` is the death message in English |
  | `player_respawn` | the player | after a dead player respawned |

  Block events report what already happened and cannot be cancelled yet. Cancellable
  events wait at most 2 s for the plugins; past that, what they describe goes ahead.
  The server sends the join and quit messages itself, so cancelling one and
  broadcasting another is how plugins replace them.
- **Luau API (implemented):**
  - `server.on(event, handler)`: unknown event names are an error. Handlers live in
    the VM's registry, so a reload drops the old ones with the old VM.
  - `server.broadcast(message)`: System chat to every player.
  - `server.player(uuid)`: the online player with that UUID (any case) as a player
    table, or `nil`. The engine keeps its own roster from the events it delivers,
    so no round trip to the game thread is needed: a player can be looked up from
    their `player_join` until their `player_quit` handlers finish, and plugins
    loaded later see everyone already online.
  - `server.command(definition)`: a slash command, with subcommands at any depth
    (`bedrockrs_plugins::luau_commands` documents the table). The definition is
    checked when the plugin loads, so a typo (an unknown field, a misnamed type, a
    required argument after an optional one) stops the load with the reason. The
    handlers stay in the VM's registry; the specs go to the server as
    `Action::SetCommands` whenever the plugins' commands change, including on hot
    reload. A handler gets a `ctx` with `sender` (nil for the console), `console`,
    `command`, `path`, `args` (parsed values; players as player tables) and
    `reply`/`error`; a string it returns is a reply. Replies made after it returned
    reach the player as chat. A handler that fails gives the player a generic
    error, and the details go to the log.
  - The `server` table is read-only after sandboxing.
  - Every player in an event is a read-only table `{ name, uuid, send_message,
    set_game_mode, kick }`.
    The methods work with `.` and `:` alike and keep working after the event; acting
    on a player who has left does nothing.
    - `player.send_message(message)`: System chat to that player only.
    - `player.set_game_mode(mode)`: survival, creative, adventure or spectator (or
      vanilla's short forms, or `default`); anything else is a Lua error.
    - `player.set_health(health)`: within 0 and 20; 0 kills them (cause `override`).
    - `player.damage(amount, cause?)`: hurts them as `cause` (a vanilla cause name,
      `none` by default) would, game mode and `player_damage` handlers permitting.
    - `player.kick(reason?)`: disconnects them with reason 55 (Kicked), showing
      `reason` or "You were kicked from the server.". Core delivers it through the
      same per-UUID channel as the duplicate-login kick.
    - A missing, empty or non-string message and a non-string reason are Lua errors.
- **Console (implemented)** in `bedrockrs_core::console`. `ConsoleFormat` prints
  `<YY/MM/DD HH:MM:SS.SSS> LEVEL [target] message key=value…`, for example
  `<26/09/26 14:30:05.123> INF [hello] Hello from Luau!`:
  - local time (chrono) in grey, to the millisecond;
  - `INF`, `WRN`, `ERR` in green, yellow, red (`DBG` blue, `TRC` magenta);
  - the target in cyan brackets: the plugin's manifest name for plugin output
    (`tracing` targets are fixed at compile time, so plugins log under `plugin` with
    a `plugin` field), `bedrockrs` for the core, otherwise the logging crate
    (`bedrockrs_net`, `bedrockrs_plugins`, `chat`, libraries);
  - the message in the default colour, then other fields as grey `key=` and value;
  - Bedrock's `§` codes as 24-bit ANSI colours (the material colours included; `§l`
    bold, `§o` italic, `§r` reset, `§k` dropped), or stripped when stdout is not a
    terminal or `NO_COLOR` is set.

  Chat (`<name> message`), broadcasts, private messages and cancelled chat are
  logged at info under the `chat` target, and each join and leave as "<name> joined
  the game" / "left the game". Connections, logins, key fetches, chunk streaming,
  saves and refused actions are debug-level system noise.
- **Plugin folders and manifests (implemented)** in `bedrockrs_plugins::manifest`. Each
  plugin is a folder in `plugins/` holding a `plugin.json`:

  ```json
  {
    "name": "hello",
    "description": "Welcomes players",
    "version": "1.1.0",
    "author": "BedrockRS",
    "main": "main.luau"
  }
  ```

  - All five fields are required and unknown fields are refused, so typos surface.
  - `name` is 1–64 letters, digits, `-` and `_`, and unique: a second folder with the
    same name is not loaded. Logs and the VM use the name, not the folder.
  - `main` is a `.luau` path inside the folder (no `..`, not absolute).
  - A folder without `plugin.json` and loose `*.luau` files in `plugins/` are skipped
    with a warning, once each.
- **Hot reload (implemented):** a recursive `notify` watcher on `plugins/`, debounced by
  200 ms. Once changes settle, the host rescans every folder: a plugin whose manifest
  or entry script changed is reloaded into a fresh VM, and one whose folder or manifest
  is gone is unloaded. A reload that fails, including an invalid manifest, logs the
  error and keeps the running version. State-handoff hooks are not built yet.
- **Luau (default, implemented)** in `bedrockrs_plugins::{host, luau}`. `PluginHost::start`
  returns once every plugin has loaded, and a broken plugin never stops startup.
  Each VM is set up in this order:
  1. A memory limit (64 MiB by default).
  2. `print` and a `log.{trace,debug,info,warn,error}` table, installed before
     sandboxing so scripts cannot replace them. Output goes to `tracing` under the
     `plugin` target through a swappable `Output` sink.
  3. `require`, confined to the plugin's folder (`luau_require::PluginRequirer`), in
     place of mlua's default, which follows paths and `.luaurc` aliases anywhere on
     disk. Modules are tracked as names below the folder, so `../` stops at its top;
     every file is checked after following links (`canonicalize`) to be inside it;
     configuration files are never read. Chunk names stay `@plugins/<folder>/<path>`,
     which `reset` maps back to a module (a folder's `init.luau` being the folder).
     Modules are cached per VM, and load inside the calling script's deadline.
  4. `Lua::sandbox(true)`: read-only libraries and globals, with script writes kept local.
  5. An interrupt that aborts any call running past the execution limit (1 s by default).

  `PluginSource` carries every `.luau`/`.lua` file in the folder (up to 1024, links
  skipped), so saving a required module reloads the plugin like its entry script.
- **JS/TS (`js` feature):** a `deno_core` `JsRuntime`. TypeScript is transpiled on load
  with `deno_ast`, so there is still no build step. Pulls a prebuilt V8 of more than
  100 MB.
- **Python (`python` feature):** RustPython 0.5. It is much slower than CPython and its
  stdlib is incomplete.

### 4.7 Cross-cutting

- **Safety:** `unsafe_code = "forbid"` across the workspace. FFI `unsafe` stays inside
  mlua, rusty_v8 and RustPython.
- **Errors:** `thiserror` in libraries, `anyhow` in the binary.
- **Logging:** `tracing`, filtered with `RUST_LOG`.
- **Profiles:** the dev profile optimizes dependencies (opt-level 2), because pure-Rust
  crypto, compression and the Luau VM are very slow unoptimized. Release builds use
  thin LTO with line-table debug info.
- **Configuration (implemented)** in `bedrockrs_core::config`: `bedrockrs.toml` in the
  working directory, written with every setting at its default and comments the
  first time the server starts (git-ignored). Missing settings keep their defaults;
  unknown keys and wrong types stop startup with the line at fault.

  ```toml
  [logs]
  level = "info"  # error, warn, info, debug or trace: our crates and plugins at it,
                  # libraries a step quieter (warn up to info, then info, then debug)
  chat = true     # the `chat` target at info, or off

  [players]
  default_game_mode = "creative"  # for players joining for the first time
  ```

  `[logs]` becomes the log filter, e.g. `warn,bedrockrs=info,plugin=info,chat=info`
  (targets match by prefix, so `bedrockrs` covers every crate). `RUST_LOG`, when set,
  replaces it. `system_noise = true`, from before `level`, still means `debug`, with a
  warning to switch.
- **Console lines (2026-10-07):** info, warnings and errors are plain sentences that
  stand on their own ("Opened world: world", "Listening on 0.0.0.0:19132", "Steve
  joined the game"). Structured fields only show on DEBUG and TRACE lines. The
  server's own lines are tagged `[BedrockRS]` in Minecraft's gold, the plugin system's
  `[Plugins]`; plugin output (`[hello]`), chat and libraries keep their own tags, in
  cyan. Routine activity and the details behind a line go to debug. Network settings and the world directory are still environment
  variables.

## 5. Approved decisions (2026-09-25)

**1. WebRTC engine: str0m 0.23 via `DirectApi`.** `webrtc` 0.21 is the fallback, behind
the same `rtc` interface.
- It is sans-IO, so one UDP socket can serve all peers and tests are deterministic.
- `DirectApi` gives exact control of the SDP and the DTLS role. str0m's SDP path
  hard-codes `passive` for an `actpass` offer, while Mojang's example answer uses `active`.
- Crypto backend: str0m's default `aws-lc-rs` (approved 2026-09-25). We started on
  `rust-crypto`, but it also compiled AWS-LC through dimpl's `rcgen` feature, so it built
  two crypto stacks. AWS-LC builds on the dev machine without CMake or NASM.
- Pumpkin uses `webrtc`, whose new architecture only went stable in July 2026.

**2. Segment size = negotiated `max-message-size` − 1.** This matches Mojang's guide and
go-nethernet, and the receiver accepts any segment size.

**3. `keys/identity.pem` (P-384) is auto-generated on first run and git-ignored.**
- `a=identity` requires it.
- A stable key avoids repeated TOFU prompts.
- HTTPS with a real certificate removes the prompt entirely.

**4. `main.rs` lives in `bedrockrs_core` (bin `bedrockrs`).** This keeps exactly four crates.

**5. JS (`js`) and Python (`python`) sit behind cargo features, off by default.**
`deno_core` pulls a prebuilt V8 of more than 100 MB and links slowly. Keeping them off
keeps everyday builds fast while the Luau bridge comes first.

## 6. Build plan

Each step starts only after explicit confirmation.

| Step | Scope | Done when | Status |
|---|---|---|---|
| 1 | Workspace, dependencies, four crate skeletons, git | `cargo check --workspace` passes cleanly | ✅ done 2026-09-25 |
| 2 | `bedrockrs_net` draft: signaling server, SDP, identity, segmenter/reassembler with tests, str0m peer driver | Compiles, tests pass, and `curl` against `/v1/join` works. A live 26.51 client handshake is the real test and may need iteration. | ✅ done 2026-09-25; a vanilla 1.26.51 client connected over LAN |
| 3 | `bedrockrs_plugins` Luau bridge: load `plugins/*.luau` into a sandbox and route `print` to the log | The `plugins/hello.luau` smoke test works | ✅ done 2026-09-25 (prints on boot; hot reload verified live) |
| 4 | Protocol handshake: batch codec, NetworkSettings → Login → resource packs; ICE-lite so sends only use paths the client proved | A live client gets past RequestNetworkSettings and receives BedrockRS's disconnect message | ✅ done 2026-09-25; a vanilla 1.26.51 client over ICE-lite showed the disconnect message |
| 5 | World spawning: StartGame, empty registries, hashed block IDs, flat chunks, PlayerSpawn → SetLocalPlayerAsInitialized | A live client leaves "Building terrain" and stands on grass | ✅ done 2026-09-25; a vanilla 1.26.51 client spawned on the grass at (8, -60, 8) |
| 6 | Plugins meet the world: `player_join` event, Text packet and chat relay, Luau `server.on` / `server.broadcast`, welcome message in `hello.luau` | A live client sees the welcome message, and chat is echoed | ✅ done 2026-09-25; the yellow welcome appeared for a vanilla 1.26.51 client |
| 7 | Tick loop and visibility: 20 TPS game loop, PlayerAuthInput decoding, per-player position, rotation and head yaw, PlayerList / AddPlayer / MovePlayer / RemoveActor between players | Two live clients see each other move | ✅ done 2026-09-26; two clients (PC and Android) saw each other move after the skin geometry fix |
| 8 | Chunk streaming: track each player's chunk, recentre on crossing a boundary, send the chunks newly in range (radius ≤ 8) with a NetworkChunkPublisherUpdate; always-visible name tags | Walking or flying far keeps loading terrain | ✅ done 2026-09-26; streaming worked live |
| 9 | Entity tracker (AddPlayer and RemoveActor as players enter and leave each other's view), and own-entity metadata with HasGravity so players stop floating | A player returning to a stationary one reappears; players fall after flying | ✅ done 2026-09-26; returning players reappear and players fall after flying |
| 10 | Vanilla movement speed: the player's own UpdateAttributes (`minecraft:movement` 0.1, underwater and lava 0.02, health 20) and UpdateAbilities (creative abilities; walk 0.1, fly 0.05, vertical fly 1.0) during spawn | Walking feels like vanilla | ✅ done 2026-09-26; walking feels like vanilla |
| 11 | Mutable world and block breaking: `World` keeps changed columns; breaks from PlayerAuthInput block actions (StartBreak, PredictDestroyBlock) and PlayerAction (CreativeDestroyBlock), checked for height, reach and loaded chunk; UpdateBlock to every player with the chunk | Broken blocks stay broken, for everyone, and after walking away and back | ✅ done 2026-09-26; breaks persist and sync across clients |
| 12 | Block placing and feedback: a hotbar of 9 vanilla blocks (a partial ItemRegistry with vanilla item IDs, InventoryContent at spawn), ClickBlock from InventoryTransaction placed against the clicked face (reach, air, not inside the placer; refusals undone with UpdateBlock), UpdateBlock plus the `place` sound; break particles (LevelEvent 2001); arm swings (Animate); sneaking (SetActorData) | Blocks can be placed and everyone sees and hears building; swings and crouching show | ✅ done 2026-09-26; hotbar, placing, particles, sounds, swings and sneaking work |
| 13 | Placement never overlaps a player: every online player's box is checked, and refusals are rolled back | A block cannot be placed where another player stands | ✅ done 2026-09-26; blocks vanish when placed inside another player |
| 14 | World persistence: changed chunks saved as one compressed file each under `world/chunks/`, every 5 s from the tick loop and on shutdown; loaded the first time a chunk is used, generated otherwise | Builds survive a server restart | ✅ done 2026-09-26; builds survive restarts |
| 15 | Player persistence and spawn: `players/<uuid>.json` (feet position and rotation) saved on leave, every 5 s and at shutdown, restored at login; new players spawn at (0, -60, 0); broadcasts logged under the `chat` target | Rejoining puts you where you left; new players start at 0, 0 | ✅ done 2026-09-26; position and view direction restore on rejoin |
| 16 | Flying state saved and restored (UpdateAbilities answers StartFlying and StopFlying); storage behind the `WorldStorage` trait with chunk format v2 (palettes of block names and states; v1 still read); server broadcasts as System text | Leaving while flying, you rejoin in the air; old and new chunk files load | ✅ done 2026-09-26; flying, spawn and old chunk files all work |
| 17 | Authentication: Login tokens verified (RS256 against the authorization service's published keys; issuer, audience and lifetime checked) before the login continues; the verified UUID is used everywhere; a second login for the same UUID kicks the older session (reason 43, "logged in from another location") | A signed-in client joins; a second device on the same account kicks the first; a forged token is refused | ✅ done 2026-09-26; a signed-in PC and Android client joined, and a second login on the same account showed the first "logged in from another location" |
| 18 | Plugin API: plugins in folders with a `plugin.json` manifest (name, description, version, author, main); events `player_quit`, `player_chat` (cancellable), `block_break`, `block_place`; actions `server.send_message(player, message)` and `server.kick(player, reason?)` | The sample plugin greets a player privately, blocks a filtered word, kicks on `!kickme`, logs block changes and announces leaving | ✅ done 2026-09-26; folders, manifests, every event and action, and hot reload all worked live |
| 19 | Plugin API and console polish: `player.send_message(message)` and `player.kick(reason?)` methods replace the `server.*` versions; plugin output labelled with the plugin's name; `§` colour codes shown as terminal colours; chat logs moved to debug | The sample plugin works as before; the console shows `hello:` lines and the welcome in yellow, and no chat lines | ✅ done 2026-09-26; methods, plugin names and colours all worked live |
| 20 | `bedrockrs.toml` created with defaults; `[logs] chat` and `system_noise` set the console filter; chat back at info; connection, login and key-fetch lines moved to debug; one "joined/left the game" line per player; `server.player(uuid)` | A fresh start writes the file; chat shows or hides with `chat`; debug lines appear with `system_noise`; `!wave` reaches the newest player | ✅ done 2026-09-26; config, log toggles and lookup all worked live |
| 21 | Console lines as `<YY/MM/DD HH:MM:SS.SSS> LEVEL [target] message` in local time, with grey time, coloured three-letter levels and cyan targets | The console shows `INF [hello]` and `INF [bedrockrs]` lines in colour | ✅ done 2026-09-26; the format and colours look right (seconds added after the first test) |
| 22 | Inventories, part A: every vanilla item in the ItemRegistry and the vanilla creative inventory (generated from BedrockData); a server-owned inventory per player (main, armour, offhand, cursor), empty for new players, saved in the player file; the inventory screen opened on request (Interact → ContainerOpen, ContainerClose echoed); item stack requests (take, place, swap, destroy, creative pick) answered with ItemStackResponse; placing uses the held stack | The creative menu is full; items can be picked, moved, split, merged and swapped; everything is where it was after rejoining | ✅ done 2026-09-27 after three live tests: the screen would not open (no ContainerOpen); then painting, gathering, drops and the cursor stash failed (request-ID references; resync after rejections); now all work, and rejected drops snap back cleanly |
| 23 | Inventories, part B: drops accepted (any slot, cursor included, a cursor with nowhere to go on closing, and Q on the HUD as a normal inventory transaction); held items shown to others (MobEquipment); item entities with physics, shown to the players in view; breaking a block drops its item; picking up items in reach every tick, with the pickup animation | Items thrown with Q or out of the screen fly forward and land; broken blocks pop their item; walking over items picks them up, and a full inventory leaves the rest; others see what a player holds | 🧪 live tests (2026-09-27): physics, pickup and screen drops worked; Q on the HUD and held items did not (fixed); then held items were invisible to others (no item bones in the placeholder skin) and throws did not swing the arm (fixed); then held items only appeared after switching slots (fixed with the skins step); ✅ done 2026-09-27 |
| 24 | Real skins: each player's skin (image, geometry, cape, animations, arm size) read from their login's client data, checked, and sent in PlayerList and PlayerSkin; AddPlayer followed by PlayerSkin and MobEquipment | Players see each other's real skins and capes, including character-creator skins; a joining player's held item shows at once | ✅ done 2026-09-27 after two live tests: character-creator skins needed their persona pieces, and a slot chosen while loading was lost |
| 25 | Block states and placement (Priority 2, part A): the 1.26.50 block palette; placed states checked and upgraded (items, saved chunks); facing, axis, torch, slab, stairs (with corners) and sign rules; fence, pane, bar and wall connections, updated around every change; rollback for every refused placement; clients' own swings (punching) shown to others | Stairs, torches, iron bars, fences, panes and logs place facing the right way and connect; nothing leaves a dead spot; punching swings the arm | 🧪 live test (2026-09-27): rotations, connections, rollback and swings worked, and old dead spots were repaired; slabs stacked a block too high and would not merge (double slabs added); ready for another test |
| 26 | Per-player game modes (saved; abilities, breaking, placing, drops and visibility follow them) and slash commands: AvailableCommands, CommandRequest and CommandOutput; command trees with subcommands, typed arguments and permissions shared by built-in and plugin commands; `server.command` for Luau plugins; operators in `ops.json`; console commands; `help`, `list`, `version`, `gamemode`, `op`, `deop`, `stop` | Players switch game modes with `/gamemode`, and a plugin's subcommands autocomplete and run | 🟡 2026-10-07: tested end to end with the console and the sample plugin; not yet with a live client |
| 27 | Health and death: the `minecraft:health` component and vanilla's damage causes; fall damage, the void and peaceful regeneration; the death screen, death messages, drops and respawning; game rules (`falldamage`, `keepinventory`, `naturalregeneration`, `showcoordinates`, `showdeathmessages`) with `/gamerule`, and `/kill`; `player_damage` (cancellable), `player_death` and `player_respawn` events and `player.set_health` / `player.damage` for plugins. The session loop now delivers every reply in one place | Survival players get hurt, die and respawn | 🟡 2026-10-07: tested in the session and from the console; not yet with a live client |

Later steps are proposed but not yet scheduled:
- `player.give` and item events for plugins; then block interactions and containers,
  then health, damage and survival (the Top 3 adopted on 2026-09-26)
- vanilla biome data (BiomeDefinitionList)
- skin changes in game (the client's PlayerSkin)
- movement validation (speed and teleport checks) and server corrections
- cancellable block events (undoing the change on the client) and more plugin actions
- the JS/TS and Python engines

## 7. Risks and open questions

- **The death screen is untested with a live client.** The flow is Dragonfly's
  (health 0 and ActorEvent(Death), then Respawn(ReadyToSpawn) once the client sends
  Respawn(ClientReadyToSpawn)), plus vanilla's DeathInfo. DeathInfo's cause is sent
  as a bare translation key (`death.attack.fall`); if the death screen shows the key
  itself, it wants a `%` in front, as Text translations do.
- **Fall damage trusts the client's collision flag.** Landing is the
  `VerticalCollision` input flag; a client that never sends it takes no fall damage.
  Server-side collision against the world would make it authoritative.

- **Command packets are untested with a live client.** AvailableCommands,
  CommandRequest and CommandOutput follow gophertunnel for 2193 (fixed 32-bit enum
  indices and offsets, string permission levels and origins, a version string in
  CommandRequest). If the client rejects AvailableCommands it disconnects at join,
  which would point at the layout; subcommands as one-value enums are how Dragonfly
  does it.

- **Live-client interop: session setup verified.** On 2026-09-25 a vanilla Bedrock
  1.26.51 client joined over LAN. Signaling, our identity assertion, ICE, DTLS (server
  active), SCTP and the client's reliable data channel all worked. The same day, over
  ICE-lite, the client completed the step 4 login handshake: compressed batches both ways,
  and BedrockRS's Disconnect message was displayed. Still unverified:
  - NAT'd or public deployments using advertised addresses
- **Minimal registries work for spawning.** The spawn sends an empty ItemRegistry and no
  BiomeDefinitionList, mirroring gophertunnel's minimal server. A live 1.26.51 client
  spawned with them on 2026-09-25. Vanilla item data now comes from Dragonfly (step
  22, moved from PocketMine's BedrockData on 2026-10-08).
- **str0m's SDP candidate parser is strict.** It expects the `ufrag` extension after
  `network-id`, while libwebrtc (and so our answer) writes it before. It then silently
  drops the candidate. This only affects str0m acting as a client, as in the loopback
  test, which normalizes the lines.
- **Visibility works live after a skin fix.** The first two-client test (2026-09-26)
  disconnected both clients the moment the second one joined. The generated skin had an
  empty geometry string, which Mojang's schema says must be valid JSON. The skin now
  carries a full `geometry.humanoid.custom` definition (format 1.12.0, engine version
  `0.0.0`), the way vanilla clients send classic skins. AddPlayer and its ability layer
  were re-checked against Mojang's schema and match, and the retest succeeded. Movement is
  still trusted as the client reports it. Sneaking and arm swings are now shown, because
  the flag numbering question is settled: the bit numbers in Mojang's PlayerActionType
  descriptions match gophertunnel's input flags (StartSneaking is bit 27, MissedSwing
  bit 39). Mojang's enum just omits some entries.
- **Vanilla world compatibility.** Vanilla Bedrock worlds are LevelDB databases (Mojang's
  fork, with raw-zlib compression), keyed by chunk, dimension and record type. Sub-chunks
  are stored with little-endian NBT palettes of block names and states. Reading them
  natively is the better end state, so owners can drop in a world, but most of the cost
  is not the database:
  - A pure-Rust LevelDB (`rusty-leveldb`, with a custom compressor) or C bindings to
    Mojang's fork. Both are workable.
  - Little-endian NBT *decoding*; BedrockRS only encodes network NBT today.
  - Upgrading old block states: older worlds use renamed or merged blocks (`stone` with
    `stone_type`, and so on), which need Mojang's upgrade tables.
  - Block entities, entities, biomes, and the level.dat and player records, or keeping
    them intact when writing.

  Palettes store names and states, and BedrockRS's network IDs are hashes of exactly
  those, so blocks themselves map cleanly. The plan is to move the `.bin` format to name
  palettes first (it has a version byte), then add LevelDB as a second backend: first
  read-only import, then native read and write.
- **Saved chunks store block names (resolved 2026-09-26).** Version 1 stored state
  hashes, which change when Mojang renames a block or changes its states. Version 2
  stores names and states. A rename across versions will still need an upgrade table,
  as for vanilla worlds, but a saved block can no longer silently become a different
  one. Blocks outside the world's known table are kept as raw placeholders. Changed
  columns still stay in memory until shutdown; unloading is not needed at this scale
  yet.
- **The full item registry and inventory protocol are untested with a live client.**
  The wire layouts follow gophertunnel for 2193, which differs from PocketMine's 1.26.30
  (the action header gained a varint type and slot stack IDs became fixed 32-bit).
  Mojang's schema omits optional-field markers, so it was used only as a cross-check.
  The container IDs are the biggest risk: if every request is rejected with "no slot",
  the numbering is off. Item data is from 1.26.50, matching the client.
- **Players are authenticated (resolved 2026-09-26).** Login tokens
  are verified against the Minecraft authorization service (§4.5, Authentication). The
  offer's `cpk` signing its DTLS fingerprints binds the connection to the verified key.
  Remaining gaps:
  - The NetherNet signaling token itself is not verified separately; the Login token
    covers the same identity and must carry the same key.
  - With `BEDROCKRS_AUTHENTICATION=false` nothing is verified, and anyone can join as
    anyone.
  - Issuer and key URLs are constants, not read from the discovery document gophertunnel
    uses. If Mojang moves the service, they need updating.
- **Send segment size.** str0m's direct API caps what we send at 64 KiB per SCTP message
  (§3.4). That is valid NetherNet, but smaller than BDS's 256 KiB. It could be lifted
  upstream or by switching engines.
- **UDP reachability.** The media port must be reachable through NAT and firewalls, which
  needs an advertise-address setting.
- **Data licensing.** Block palette, item and creative data come from BDS dumps, and
  Mojang's schemas are under the EULA.
- **Optional engines.** `deno_core` is large and slow to compile; RustPython performs
  poorly.
- **26.60** will bump the protocol (2216+ in preview).

## 8. Sources

- [Mojang — NetherNet HTTP Signaling Partner Onboarding Guide](https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/)
- [Mojang/bedrock-protocol-docs](https://github.com/Mojang/bedrock-protocol-docs) · [releases](https://github.com/Mojang/bedrock-protocol-docs/releases)
- [df-mc/nethernet-spec](https://github.com/df-mc/nethernet-spec) (1.20.50-era)
- [df-mc/go-nethernet](https://github.com/df-mc/go-nethernet) (`conn.go`, `listener.go`, `identity.go`, `endpoint/handler.go`)
- [Sandertv/gophertunnel](https://github.com/Sandertv/gophertunnel) (`minecraft/protocol/packet` encoder/decoder, `minecraft/service`)
- [Bedrock Edition 26.51 — Minecraft Wiki](https://minecraft.wiki/w/Bedrock_Edition_26.51)
- [WaterdogPE — NetherNet Configuration](https://docs.waterdog.dev/waterdogpe-setup/nethernet-configuration)
- [Dragonfly PR #1290 — first-class NetherNet support](https://github.com/df-mc/dragonfly/pull/1290) · [world package](https://pkg.go.dev/github.com/df-mc/dragonfly/server/world)
- [Pumpkin PR #3472 — 26.51](https://github.com/Pumpkin-MC/Pumpkin/pull/3472) · [PR #2825 — LAN discovery](https://github.com/Pumpkin-MC/Pumpkin/pull/2825)
- [playit.gg — NetherNet vs RakNet](https://playit.gg/support/minecraft-bedrock-nethernet/)
- [str0m](https://github.com/algesten/str0m) · [webrtc v0.20 announcement](https://webrtc.rs/blog/2026/07/31/announcing-webrtc-v0.20.0.html)
