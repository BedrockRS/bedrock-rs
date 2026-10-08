//! The session's tests, by area, and the helpers they share.

mod blocks;
mod commands;
mod health;
mod inventory;
mod login;
mod movement;

use std::sync::Arc;

use bedrockrs_net::identity::verify_client;
use bedrockrs_net::sdp::SdpFingerprint;
use bedrockrs_net::{ClientIdentity, ServerIdentity};
use bedrockrs_plugins::CommandReply;
use bedrockrs_protocol::PROTOCOL_VERSION;
use bedrockrs_protocol::batch::CompressionAlgorithm;
use bedrockrs_protocol::block::{BlockState, StateValue};
use bedrockrs_protocol::login::{ConnectionRequest, IdentityClaims};
use bedrockrs_protocol::packet::{self, Decode, Encode, id};
use bedrockrs_protocol::packets::{
    Animate, BlockAction, BlockPickRequest, ContainerClose, ContainerOpen, Disconnect,
    DisconnectReason, Interact, InventoryTransaction, ItemInstance, ItemStackRequest, Login,
    MobEquipment, NetworkChunkPublisherUpdate, NetworkSettings, OWN_INVENTORY_WINDOW, PackResponse,
    PlayStatus, PlayStatusCode, PlayerAction, PlayerAuthInput, RequestChunkRadius,
    RequestNetworkSettings, ResourcePackClientResponse, Respawn, RespawnState,
    SetLocalPlayerAsInitialized, Text, TextType, TransactionData, UseItem, action_source,
    input_flag, interact_action, player_action, use_item_action,
};
use bedrockrs_protocol::types::{BlockPos, ChunkPos, Vec3};

use super::*;
use crate::auth::AuthError;
use crate::commands::Sender;
use crate::damage::DamageCause;
use crate::entities::{ItemEntities, ItemStack, PickedUp};
use crate::game_mode::GameMode;
use crate::game_rules;
use crate::inventory::{Inventory, TEST_KIT};
use crate::players::{EYE_HEIGHT, View};
use crate::storage::SavedInventory;
use crate::world::World;

/// The entity ID tests give the player.
const PLAYER_ENTITY_ID: u64 = 1;

/// A client identity and a Login token carrying the same key, as a vanilla
/// client produces them. Built from a server-style assertion, whose token
/// has the `cpk` claim a Login token needs.
fn client() -> (ClientIdentity, String) {
    let key = ServerIdentity::generate("test").unwrap();
    let fingerprints = [SdpFingerprint {
        algorithm: "sha-256".into(),
        digest: "AA".into(),
    }];
    let identity = verify_client(&key.assertion(&fingerprints).unwrap(), &fingerprints).unwrap();
    let token = identity.token.clone();
    (identity, token)
}

fn session(identity: Option<ClientIdentity>) -> Session {
    Session::new(
        identity,
        Arc::new(World::new()),
        PLAYER_ENTITY_ID,
        GameMode::Creative,
    )
}

fn login_packet(token: String) -> Vec<u8> {
    let request = ConnectionRequest {
        authentication_type: 0,
        chain: Vec::new(),
        token,
        client_data: String::new(),
    };
    Login {
        client_protocol: PROTOCOL_VERSION,
        connection_request: request.encode(),
    }
    .encode()
}

/// Sends a Login and answers its authentication request as offline
/// verification would, with the token's own claims. Returns the reply
/// that continues the login.
fn log_in(session: &mut Session, token: String) -> Reply {
    let reply = session.handle(&login_packet(token.clone())).unwrap();
    assert!(
        reply.packets.is_empty(),
        "nothing is sent before verification"
    );
    assert_eq!(reply.events, [SessionEvent::Authenticate(token.clone())]);
    assert_eq!(session.stage(), Stage::Authenticating);
    let claims = bedrockrs_protocol::login::Jwt::parse(&token)
        .unwrap()
        .claims;
    session.authenticated(Ok(IdentityClaims::from_token_claims(&claims)))
}

fn pack_response(response: PackResponse) -> Vec<u8> {
    ResourcePackClientResponse { response }.encode()
}

fn ids(reply: &Reply) -> Vec<u32> {
    reply
        .packets
        .iter()
        .map(|packet| packet::read_header(packet).unwrap().0.id)
        .collect()
}

fn decode_only<P: Decode>(packet: &[u8]) -> P {
    let (header, payload) = packet::read_header(packet).unwrap();
    assert_eq!(header.id, P::ID);
    packet::decode(payload).unwrap()
}

/// Runs a session up to the point where StartGame has been sent.
fn spawning_session() -> Session {
    let (identity, token) = client();
    let mut session = session(Some(identity));
    let request = RequestNetworkSettings {
        client_protocol: PROTOCOL_VERSION,
    };
    session.handle(&request.encode()).unwrap();
    log_in(&mut session, token);
    session
        .handle(&pack_response(PackResponse::DownloadingFinished))
        .unwrap();
    session
        .handle(&pack_response(PackResponse::StackFinished))
        .unwrap();
    assert_eq!(session.stage(), Stage::Spawning);
    session
}

fn auth_input(position: Vec3, yaw: f32) -> Vec<u8> {
    breaking_input(position, yaw, Vec::new())
}

fn breaking_input(position: Vec3, yaw: f32, block_actions: Vec<BlockAction>) -> Vec<u8> {
    PlayerAuthInput {
        pitch: 0.0,
        yaw,
        position,
        move_vector: Default::default(),
        head_yaw: yaw,
        input_flags: Vec::new(),
        input_mode: 1,
        play_mode: 0,
        interaction_model: 0,
        interact_rotation: Default::default(),
        tick: 1,
        delta: Vec3::default(),
        block_actions,
        block_actions_unread: false,
        item_stack_request: None,
    }
    .encode()
}

/// A session whose player is in the world, standing at the spawn.
fn in_game_session() -> Session {
    let mut session = spawning_session();
    let request = RequestChunkRadius {
        radius: 4,
        max_radius: 4,
    };
    session.handle(&request.encode()).unwrap();
    let initialized = SetLocalPlayerAsInitialized {
        entity_runtime_id: PLAYER_ENTITY_ID,
    };
    session.handle(&initialized.encode()).unwrap();
    // Blocks to build with; new players start empty. Others already see
    // the stone in slot 0, as they would after joining.
    session.inventory = Inventory::with_hotbar(&TEST_KIT);
    session.shown_held = session.inventory.hotbar(0).unwrap().instance();
    session
}

fn swap_request(id: i32, first: u8, second: u8, session: &Session) -> ItemStackRequest {
    use bedrockrs_protocol::packets::{
        FullContainerName, StackAction, StackRequest, StackSlot, container,
    };
    let slot = |slot: u8| StackSlot {
        container: FullContainerName::new(container::HOTBAR),
        slot,
        stack_id: session.inventory().hotbar(slot.into()).unwrap().id,
    };
    ItemStackRequest {
        requests: vec![StackRequest {
            id,
            actions: vec![StackAction::Swap {
                source: slot(first),
                destination: slot(second),
            }],
            filter_strings: Vec::new(),
            filter_cause: 0,
        }],
    }
}

fn spawn_eyes() -> Vec3 {
    Vec3 {
        x: 0.5,
        y: -60.0 + EYE_HEIGHT,
        z: 0.5,
    }
}

/// PlayerAuthInput at the spawn with these input flags.
fn flags_input(flags: Vec<i32>) -> Vec<u8> {
    let mut input = PlayerAuthInput::decode_payload(&mut bedrockrs_protocol::io::Reader::new(
        &breaking_input(spawn_eyes(), 0.0, Vec::new())[2..],
    ))
    .unwrap();
    input.input_flags = flags;
    input.encode()
}

fn damage_events(reply: &Reply) -> Vec<(DamageCause, f32)> {
    reply
        .events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Damage { cause, amount } => Some((*cause, *amount)),
            _ => None,
        })
        .collect()
}
