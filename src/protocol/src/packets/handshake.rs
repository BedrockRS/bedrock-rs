//! Packets of the login handshake.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};

/// First packet a client sends, before anything is compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestNetworkSettings {
    pub client_protocol: i32,
}

impl Packet for RequestNetworkSettings {
    const ID: u32 = id::REQUEST_NETWORK_SETTINGS;
}

impl Decode for RequestNetworkSettings {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            client_protocol: reader.i32_be()?,
        })
    }
}

impl Encode for RequestNetworkSettings {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.i32_be(self.client_protocol);
    }
}

/// The server's answer to [`RequestNetworkSettings`]; compression starts after it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetworkSettings {
    /// Smallest batch that is compressed; 0 disables compression.
    pub compression_threshold: u16,
    pub compression_algorithm: u16,
    pub client_throttle: bool,
    pub client_throttle_threshold: u8,
    pub client_throttle_scalar: f32,
}

impl Packet for NetworkSettings {
    const ID: u32 = id::NETWORK_SETTINGS;
}

impl Encode for NetworkSettings {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.u16_le(self.compression_threshold);
        writer.u16_le(self.compression_algorithm);
        writer.bool(self.client_throttle);
        writer.u8(self.client_throttle_threshold);
        writer.f32_le(self.client_throttle_scalar);
    }
}

impl Decode for NetworkSettings {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            compression_threshold: reader.u16_le()?,
            compression_algorithm: reader.u16_le()?,
            client_throttle: reader.bool()?,
            client_throttle_threshold: reader.u8()?,
            client_throttle_scalar: reader.f32_le()?,
        })
    }
}

/// The client's login: its protocol and the connection request that
/// [`crate::login::ConnectionRequest`] parses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    /// Superseded by [`RequestNetworkSettings::client_protocol`].
    pub client_protocol: i32,
    pub connection_request: Vec<u8>,
}

impl Packet for Login {
    const ID: u32 = id::LOGIN;
}

impl Decode for Login {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            client_protocol: reader.i32_be()?,
            connection_request: reader.byte_array()?.to_vec(),
        })
    }
}

impl Encode for Login {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.i32_be(self.client_protocol);
        writer.byte_array(&self.connection_request);
    }
}

/// Values of [`PlayStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayStatusCode {
    LoginSuccess = 0,
    /// The client is older than the server.
    LoginFailedClient = 1,
    /// The server is older than the client.
    LoginFailedServer = 2,
    PlayerSpawn = 3,
    LoginFailedInvalidTenant = 4,
    LoginFailedVanillaEdu = 5,
    LoginFailedEduVanilla = 6,
    LoginFailedServerFull = 7,
    LoginFailedEditorVanilla = 8,
    LoginFailedVanillaEditor = 9,
}

impl PlayStatusCode {
    const ALL: [Self; 10] = [
        Self::LoginSuccess,
        Self::LoginFailedClient,
        Self::LoginFailedServer,
        Self::PlayerSpawn,
        Self::LoginFailedInvalidTenant,
        Self::LoginFailedVanillaEdu,
        Self::LoginFailedEduVanilla,
        Self::LoginFailedServerFull,
        Self::LoginFailedEditorVanilla,
        Self::LoginFailedVanillaEditor,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayStatus {
    pub status: PlayStatusCode,
}

impl Packet for PlayStatus {
    const ID: u32 = id::PLAY_STATUS;
}

impl Encode for PlayStatus {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.i32_be(self.status as i32);
    }
}

impl Decode for PlayStatus {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let value = reader.i32_be()?;
        let status = PlayStatusCode::ALL
            .into_iter()
            .find(|status| *status as i32 == value)
            .ok_or(DecodeError::InvalidValue {
                field: "play status",
                value: value.into(),
            })?;
        Ok(Self { status })
    }
}

/// Why a client was disconnected; selects the error shown on its disconnect screen.
/// The protocol defines around 150 reasons, so only the ones used are named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisconnectReason(pub i32);

impl DisconnectReason {
    pub const UNKNOWN: Self = Self(0);
    pub const OUTDATED_SERVER: Self = Self(34);
    pub const OUTDATED_CLIENT: Self = Self(35);
    pub const DISCONNECTED: Self = Self(41);
    pub const NOT_AUTHENTICATED: Self = Self(46);
    pub const UNEXPECTED_PACKET: Self = Self(49);
    pub const KICKED: Self = Self(55);
    pub const LOGGED_IN_OTHER_LOCATION: Self = Self(43);
    pub const RESOURCE_PACK_PROBLEM: Self = Self(58);
    pub const BAD_PACKET: Self = Self(90);
}

/// Text shown on the client's disconnect screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisconnectMessage {
    pub message: String,
    /// Used instead of `message` when the player filters profanity; empty means `message`.
    pub filtered_message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disconnect {
    pub reason: DisconnectReason,
    /// `None` skips the disconnect screen and returns the player to the menu.
    pub message: Option<DisconnectMessage>,
}

impl Packet for Disconnect {
    const ID: u32 = id::DISCONNECT;
}

impl Encode for Disconnect {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_i32(self.reason.0);
        writer.bool(self.message.is_none());
        if let Some(message) = &self.message {
            writer.string(&message.message);
            writer.string(&message.filtered_message);
        }
    }
}

impl Decode for Disconnect {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let reason = DisconnectReason(reader.var_i32()?);
        let hide_screen = reader.bool()?;
        let message = if hide_screen {
            None
        } else {
            Some(DisconnectMessage {
                message: reader.string()?.to_owned(),
                filtered_message: reader.string()?.to_owned(),
            })
        };
        Ok(Self { reason, message })
    }
}

/// Resource packs the client needs. BedrockRS does not serve packs yet, so the
/// pack list is always empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourcePacksInfo {
    pub resource_pack_required: bool,
    pub has_addon_packs: bool,
    pub has_scripts: bool,
    pub force_disable_vibrant_visuals: bool,
    pub world_template_id: [u8; 16],
    pub world_template_version: String,
}

impl Packet for ResourcePacksInfo {
    const ID: u32 = id::RESOURCE_PACKS_INFO;
}

impl Encode for ResourcePacksInfo {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.bool(self.resource_pack_required);
        writer.bool(self.has_addon_packs);
        writer.bool(self.has_scripts);
        writer.bool(self.force_disable_vibrant_visuals);
        writer.uuid(self.world_template_id);
        writer.string(&self.world_template_version);
        writer.var_u32(0);
    }
}

/// A pack in the [`ResourcePackStack`], by UUID and version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackPack {
    pub id: String,
    pub version: String,
    pub sub_pack_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Experiment {
    pub name: String,
    pub enabled: bool,
}

/// The order packs apply in, plus world experiments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePackStack {
    pub texture_pack_required: bool,
    pub packs: Vec<StackPack>,
    pub base_game_version: String,
    pub experiments: Vec<Experiment>,
    pub experiments_previously_toggled: bool,
    pub include_editor_packs: bool,
}

impl Packet for ResourcePackStack {
    const ID: u32 = id::RESOURCE_PACK_STACK;
}

impl Encode for ResourcePackStack {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.bool(self.texture_pack_required);
        writer.var_u32(len_u32(self.packs.len()));
        for pack in &self.packs {
            writer.string(&pack.id);
            writer.string(&pack.version);
            writer.string(&pack.sub_pack_name);
        }
        writer.string(&self.base_game_version);
        // Experiments are the one list here with a fixed-size u32 count.
        writer.u32_le(len_u32(self.experiments.len()));
        for experiment in &self.experiments {
            writer.string(&experiment.name);
            writer.bool(experiment.enabled);
        }
        writer.bool(self.experiments_previously_toggled);
        writer.bool(self.include_editor_packs);
    }
}

/// Built-in vanilla packs that go servers (gophertunnel) always list in the
/// resource pack stack for this protocol: (UUID, version).
pub const EXEMPTED_PACKS: [(&str, &str); 8] = [
    ("d34cfa4b-2ad1-453d-a0db-668b429a3ea0", "1.26.40"),
    ("b41c2785-c512-4a49-af56-3a87afd47c57", "1.21.30"),
    ("a4df0cb3-17be-4163-88d7-fcf7002b935d", "1.21.20"),
    ("d19adffe-a2e1-4b02-8436-ca4583368c89", "1.21.10"),
    ("85d5603d-2824-4b21-8044-34f441f4fce1", "1.21.0"),
    ("e977cd13-0a11-4618-96fb-03dfe9c43608", "1.20.60"),
    ("0674721c-a0aa-41a1-9ba8-1ed33ea3e7ed", "1.20.50"),
    ("0fba4063-dba1-4281-9b89-ff9390653530", "1.0.0"),
];

/// How the client answers [`ResourcePacksInfo`] and [`ResourcePackStack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackResponse {
    /// The player declined required packs.
    Cancel,
    /// The client wants these packs (`uuid_version`) downloaded first.
    Downloading(Vec<String>),
    /// The client has every pack; the server sends the stack next.
    DownloadingFinished,
    /// The client applied the stack; the server starts the game next.
    StackFinished,
}

impl PackResponse {
    /// The tag and the name sent after it.
    fn tag(&self) -> (u32, &'static str) {
        match self {
            Self::Cancel => (0, "cancel"),
            Self::Downloading(_) => (1, "downloading"),
            Self::DownloadingFinished => (2, "downloadingfinished"),
            Self::StackFinished => (3, "resourcepackstackfinished"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePackClientResponse {
    pub response: PackResponse,
}

impl Packet for ResourcePackClientResponse {
    const ID: u32 = id::RESOURCE_PACK_CLIENT_RESPONSE;
}

impl Decode for ResourcePackClientResponse {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let tag = reader.var_u32()?;
        // The tag's name follows it; the tag alone decides the response.
        reader.string()?;
        let response = match tag {
            0 => PackResponse::Cancel,
            1 => {
                let count = reader.var_u32()?;
                let packs = (0..count)
                    .map(|_| reader.string().map(str::to_owned))
                    .collect::<Result<_, _>>()?;
                PackResponse::Downloading(packs)
            }
            2 => PackResponse::DownloadingFinished,
            3 => PackResponse::StackFinished,
            other => {
                return Err(DecodeError::InvalidValue {
                    field: "resource pack response",
                    value: other.into(),
                });
            }
        };
        Ok(Self { response })
    }
}

impl Encode for ResourcePackClientResponse {
    fn encode_payload(&self, writer: &mut Writer) {
        let (tag, name) = self.response.tag();
        writer.var_u32(tag);
        writer.string(name);
        if let PackResponse::Downloading(packs) = &self.response {
            writer.var_u32(len_u32(packs.len()));
            for pack in packs {
                writer.string(pack);
            }
        }
    }
}

/// Whether the client supports the blob cache; sent early in the login.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientCacheStatus {
    pub enabled: bool,
}

impl Packet for ClientCacheStatus {
    const ID: u32 = id::CLIENT_CACHE_STATUS;
}

impl Decode for ClientCacheStatus {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        Ok(Self {
            enabled: reader.bool()?,
        })
    }
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("lists are shorter than 4 billion entries")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    fn round_trip<P: Encode + Decode + PartialEq + std::fmt::Debug>(packet: &P) {
        let bytes = packet.encode();
        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, P::ID);
        assert_eq!(&decode::<P>(payload).unwrap(), packet);
    }

    #[test]
    fn request_network_settings_matches_the_client_bytes() {
        let bytes = [0xC1, 0x01, 0x00, 0x00, 0x08, 0x91];
        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::REQUEST_NETWORK_SETTINGS);
        assert_eq!(
            decode::<RequestNetworkSettings>(payload).unwrap(),
            RequestNetworkSettings {
                client_protocol: 2193
            }
        );
    }

    #[test]
    fn network_settings_layout() {
        let settings = NetworkSettings {
            compression_threshold: 256,
            compression_algorithm: 0,
            client_throttle: false,
            client_throttle_threshold: 0,
            client_throttle_scalar: 0.0,
        };
        assert_eq!(
            settings.encode(),
            [
                0x8F, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00
            ]
        );
        round_trip(&settings);
    }

    #[test]
    fn play_status_is_big_endian() {
        let status = PlayStatus {
            status: PlayStatusCode::LoginFailedServer,
        };
        assert_eq!(status.encode(), [0x02, 0x00, 0x00, 0x00, 0x02]);
        round_trip(&status);
    }

    #[test]
    fn disconnect_writes_messages_only_when_shown() {
        let hidden = Disconnect {
            reason: DisconnectReason::KICKED,
            message: None,
        };
        assert_eq!(hidden.encode(), [0x05, 110, 0x01]);
        round_trip(&hidden);
        round_trip(&Disconnect {
            reason: DisconnectReason::DISCONNECTED,
            message: Some(DisconnectMessage {
                message: "bye".into(),
                filtered_message: String::new(),
            }),
        });
    }

    #[test]
    fn empty_resource_pack_info_layout() {
        let mut expected = vec![0x06, 0, 0, 0, 0];
        expected.extend([0; 16]);
        expected.extend([0, 0]);
        assert_eq!(ResourcePacksInfo::default().encode(), expected);
    }

    #[test]
    fn resource_pack_stack_layout() {
        let stack = ResourcePackStack {
            texture_pack_required: false,
            packs: vec![StackPack {
                id: "a".into(),
                version: "1".into(),
                sub_pack_name: String::new(),
            }],
            base_game_version: "1.26.51".into(),
            experiments: vec![Experiment {
                name: "x".into(),
                enabled: true,
            }],
            experiments_previously_toggled: false,
            include_editor_packs: false,
        };
        let mut expected = vec![0x07, 0, 1, 1, b'a', 1, b'1', 0, 7];
        expected.extend(b"1.26.51");
        expected.extend([1, 0, 0, 0, 1, b'x', 1, 0, 0]);
        assert_eq!(stack.encode(), expected);
    }

    #[test]
    fn pack_responses_round_trip() {
        for response in [
            PackResponse::Cancel,
            PackResponse::Downloading(vec!["uuid_1.0.0".into()]),
            PackResponse::DownloadingFinished,
            PackResponse::StackFinished,
        ] {
            round_trip(&ResourcePackClientResponse { response });
        }
        assert_eq!(
            ResourcePackClientResponse {
                response: PackResponse::DownloadingFinished
            }
            .encode(),
            [&[0x08, 2, 19][..], b"downloadingfinished"].concat()
        );
    }

    #[test]
    fn login_round_trips() {
        round_trip(&Login {
            client_protocol: 2193,
            connection_request: b"request".to_vec(),
        });
    }
}
