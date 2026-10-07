//! What handling a packet leads to.

use bedrockrs_protocol::batch::Compression;
use bedrockrs_protocol::packet::Encode;
use bedrockrs_protocol::packets::{
    CommandOrigin, Disconnect, DisconnectMessage, DisconnectReason, ItemInstance,
};
use bedrockrs_protocol::types::{BlockPos, Vec3};
use uuid::Uuid;

use crate::damage::DamageCause;
use crate::entities::{ItemStack, PickedUp};
use crate::game_mode::GameMode;
use crate::players::{Movement, Profile, View};
use crate::storage::SavedInventory;

/// What to do after handling a packet.
#[derive(Debug, Default)]
pub struct Reply {
    /// Encoded packets to send together, in order.
    pub packets: Vec<Vec<u8>>,
    /// Compression both sides use once `packets` has been sent.
    pub enable_compression: Option<Compression>,
    /// Close the connection once `packets` has been sent.
    pub close: bool,
    /// What the rest of the server should hear about, once `packets` has been sent.
    pub events: Vec<SessionEvent>,
}

/// Something a session did that matters beyond its own client.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// The Login's multiplayer token needs verifying; the result goes to
    /// [`Session::authenticated`](super::Session::authenticated).
    Authenticate(String),
    /// The player's verified identity: they hold this UUID from now on.
    LoggedIn(Uuid),
    /// The player finished spawning and is in the world.
    Joined {
        profile: Profile,
        movement: Movement,
        view: View,
        inventory: SavedInventory,
        /// What they hold and in which hotbar slot.
        held: (ItemInstance, u8),
    },
    /// The player moved or looked around.
    Moved(Movement),
    /// The player's client now shows a different set of chunks.
    Viewing(View),
    /// The player broke the block at this position.
    BrokeBlock(BlockPos),
    /// The player placed `block` (a network ID) at `pos`, which was air.
    /// `replacing` is the block it goes over (a slab becoming a double slab),
    /// or `None` to go into air.
    PlacedBlock {
        pos: BlockPos,
        block: u32,
        replacing: Option<u32>,
    },
    /// The player swung their arm.
    Swing,
    /// The player started or stopped sneaking.
    Sneaking(bool),
    /// The player started or stopped flying.
    Flying(bool),
    /// The player's inventory changed; this is it now, for saving.
    InventoryChanged(SavedInventory),
    /// The player threw items out of their inventory, standing at `feet` and
    /// looking along `pitch` and `yaw`.
    Dropped {
        stacks: Vec<ItemStack>,
        feet: Vec3,
        pitch: f32,
        yaw: f32,
    },
    /// The player picked up items lying in the world.
    PickedUp(Vec<PickedUp>),
    /// What the player holds changed: another slot, or the stack in it.
    Holding { item: ItemInstance, slot: u8 },
    /// The player said something in chat.
    Chat(String),
    /// The player typed a slash command; its output goes back with `origin`.
    Command { line: String, origin: CommandOrigin },
    /// The player's game mode changed.
    GameModeChanged(GameMode),
    /// The player is about to take damage, unless plugins cancel it; then
    /// [`Session::apply_damage`](super::Session::apply_damage) deals it.
    Damage { cause: DamageCause, amount: f32 },
    /// The player was hurt; others see them flinch.
    Hurt,
    /// The player's health is now this, for saving.
    HealthChanged(f32),
    /// The player died, standing at `feet`, dropping `drops`.
    Died {
        cause: DamageCause,
        drops: Vec<ItemStack>,
        feet: Vec3,
    },
    /// The dead player respawned.
    Respawned,
}

impl Reply {
    pub(super) fn send(packets: Vec<Vec<u8>>) -> Self {
        Self {
            packets,
            ..Self::default()
        }
    }

    /// Disconnects the client, showing `message` on its disconnect screen.
    pub(super) fn disconnect(reason: DisconnectReason, message: impl Into<String>) -> Self {
        let disconnect = Disconnect {
            reason,
            message: Some(DisconnectMessage {
                message: message.into(),
                filtered_message: String::new(),
            }),
        };
        Self {
            packets: vec![disconnect.encode()],
            close: true,
            ..Self::default()
        }
    }
}
