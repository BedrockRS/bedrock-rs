//! Getting hurt, dying and respawning.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};
use crate::types::Vec3;

/// Something an entity does that clients animate, such as flinching when hurt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActorEvent {
    pub entity_runtime_id: u64,
    /// One of [`ActorEvent::HURT`], [`ActorEvent::DEATH`], …
    pub event: u8,
    pub data: i32,
}

impl ActorEvent {
    /// The entity flinches and turns red, and the hurt sound plays.
    pub const HURT: u8 = 2;
    /// The entity falls over.
    pub const DEATH: u8 = 3;

    pub fn new(entity_runtime_id: u64, event: u8) -> Self {
        Self {
            entity_runtime_id,
            event,
            data: 0,
        }
    }
}

impl Packet for ActorEvent {
    const ID: u32 = id::ACTOR_EVENT;
}

impl Encode for ActorEvent {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        writer.u8(self.event);
        writer.var_i32(self.data);
        writer.bool(false); // no position to fire at
    }
}

/// Where a dead player respawns, and the steps of getting there. The client
/// answers the death screen's Respawn button with
/// [`RespawnState::ClientReadyToSpawn`]; the server then sends
/// [`RespawnState::ReadyToSpawn`] with where the player now stands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Respawn {
    /// The player's eyes.
    pub position: Vec3,
    pub state: RespawnState,
    pub entity_runtime_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RespawnState {
    SearchingForSpawn = 0,
    ReadyToSpawn = 1,
    ClientReadyToSpawn = 2,
}

impl Packet for Respawn {
    const ID: u32 = id::RESPAWN;
}

impl Encode for Respawn {
    fn encode_payload(&self, writer: &mut Writer) {
        self.position.write(writer);
        writer.u8(self.state as u8);
        writer.var_u64(self.entity_runtime_id);
    }
}

impl Decode for Respawn {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let position = Vec3::read(reader)?;
        let value = reader.u8()?;
        let state = match value {
            0 => RespawnState::SearchingForSpawn,
            1 => RespawnState::ReadyToSpawn,
            2 => RespawnState::ClientReadyToSpawn,
            _ => {
                return Err(DecodeError::InvalidValue {
                    field: "respawn state",
                    value: value.into(),
                });
            }
        };
        Ok(Self {
            position,
            state,
            entity_runtime_id: reader.var_u64()?,
        })
    }
}

/// What the death screen says killed the player: a translation key and its
/// parameters, such as `death.attack.fall` and the player's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeathInfo {
    pub cause: String,
    pub messages: Vec<String>,
}

impl Packet for DeathInfo {
    const ID: u32 = id::DEATH_INFO;
}

impl Encode for DeathInfo {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.string(&self.cause);
        writer.var_u32(u32::try_from(self.messages.len()).expect("a few parameters"));
        for message in &self.messages {
            writer.string(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    #[test]
    fn actor_event_layout() {
        assert_eq!(
            ActorEvent::new(5, ActorEvent::HURT).encode(),
            [0x1B, 0x05, 0x02, 0x00, 0x00]
        );
    }

    #[test]
    fn respawn_round_trips() {
        let respawn = Respawn {
            position: Vec3 {
                x: 0.5,
                y: -58.38,
                z: 0.5,
            },
            state: RespawnState::ClientReadyToSpawn,
            entity_runtime_id: 1,
        };
        let bytes = respawn.encode();
        assert_eq!(bytes[0], 0x2D);
        assert_eq!(bytes.len(), 1 + 12 + 1 + 1);
        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::RESPAWN);
        assert_eq!(decode::<Respawn>(payload).unwrap(), respawn);

        let mut bad = bytes.clone();
        bad[13] = 7;
        let (_, payload) = read_header(&bad).unwrap();
        assert!(decode::<Respawn>(payload).is_err());
    }

    #[test]
    fn death_info_layout() {
        let info = DeathInfo {
            cause: "death.attack.fall".into(),
            messages: vec!["Steve".into()],
        };
        let mut expected = vec![0xBD, 0x01, 0x11];
        expected.extend(b"death.attack.fall");
        expected.extend([0x01, 0x05]);
        expected.extend(b"Steve");
        assert_eq!(info.encode(), expected);
    }
}
