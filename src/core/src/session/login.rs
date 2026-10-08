//! The login handshake, from RequestNetworkSettings until the player is
//! past the loading screen.

use std::sync::Arc;

use bedrockrs_protocol::login::{ConnectionRequest, IdentityClaims};
use bedrockrs_protocol::nbt::Compound;
use bedrockrs_protocol::packet::Encode;
use bedrockrs_protocol::packets::{
    AbilityData, AbilityLayer, Attribute, ChunkRadiusUpdated, DisconnectReason, EXEMPTED_PACKS,
    ItemInstance, JigsawStructureData, Login, NetworkSettings, PackResponse, PlayStatus,
    PlayStatusCode, PlayerMovementSettings, RequestChunkRadius, RequestNetworkSettings,
    ResourcePackClientResponse, ResourcePackStack, ResourcePacksInfo, SetActorData,
    SetLocalPlayerAsInitialized, StackPack, StartGame, UpdateAbilities, UpdateAttributes,
    VoxelShapes, ability,
};
use bedrockrs_protocol::skin::client_skin;
use bedrockrs_protocol::{GAME_VERSION, PROTOCOL_VERSION};
use uuid::Uuid;

use crate::auth::AuthError;
use crate::blocks::palette;
use crate::game_mode::GameMode;
use crate::game_rules;
use crate::inventory::Inventory;
use crate::permissions::{Permission, PlayerIds};
use crate::players::{Movement, Profile, View, placeholder_skin, player_metadata};
use crate::world::{OVERWORLD, World};

use super::{
    COMPRESSION, CREATIVE_CONTENT, ITEM_REGISTRY, MAX_VIEW_DISTANCE, Reply, Session, SessionError,
    SessionEvent, Stage,
};

impl Session {
    pub(super) fn request_network_settings(
        &mut self,
        request: RequestNetworkSettings,
    ) -> Result<Reply, SessionError> {
        if request.client_protocol != PROTOCOL_VERSION {
            tracing::info!(
                "Turned away a client on protocol {}; this server runs protocol {PROTOCOL_VERSION} ({GAME_VERSION})",
                request.client_protocol
            );
            let status = if request.client_protocol < PROTOCOL_VERSION {
                PlayStatusCode::LoginFailedClient
            } else {
                PlayStatusCode::LoginFailedServer
            };
            return Ok(Reply {
                packets: vec![PlayStatus { status }.encode()],
                close: true,
                ..Reply::default()
            });
        }

        self.stage = Stage::Login;
        let settings = NetworkSettings {
            compression_threshold: COMPRESSION.threshold,
            compression_algorithm: COMPRESSION.algorithm.id(),
            client_throttle: false,
            client_throttle_threshold: 0,
            client_throttle_scalar: 0.0,
        };
        Ok(Reply {
            packets: vec![settings.encode()],
            enable_compression: Some(COMPRESSION),
            ..Reply::default()
        })
    }

    /// Reads the connection request and asks for its multiplayer token to be
    /// verified; [`Session::authenticated`] continues with the result.
    pub(super) fn login(&mut self, login: Login) -> Result<Reply, SessionError> {
        let request = ConnectionRequest::parse(&login.connection_request)?;
        // The player's own skin, unless it is unusable: then a placeholder.
        self.skin = match client_skin(&request.client_data) {
            Ok(skin) => Some(Arc::new(skin)),
            Err(err) => {
                tracing::warn!("Couldn't use a player's skin, so they get the default one: {err}");
                None
            }
        };
        self.stage = Stage::Authenticating;
        Ok(Reply {
            events: vec![SessionEvent::Authenticate(request.token)],
            ..Reply::default()
        })
    }

    /// Continues the login with the verified identity from the multiplayer
    /// token, or disconnects a player who could not be verified.
    pub fn authenticated(&mut self, result: Result<IdentityClaims, AuthError>) -> Reply {
        if self.stage != Stage::Authenticating {
            return Reply::default();
        }
        let claims = match result {
            Ok(claims) => claims,
            Err(err) => {
                tracing::info!(
                    "Turned away a player whose Microsoft account couldn't be verified: {err}"
                );
                return Reply::disconnect(
                    DisconnectReason::NOT_AUTHENTICATED,
                    format!("BedrockRS: could not verify your Microsoft account.\n{err}"),
                );
            }
        };

        // NetherNet has no game-level encryption, so a captured Login could be
        // replayed on another connection. The Login must carry the key this
        // connection proved during signaling.
        if let Some(identity) = &self.identity {
            let matches = claims
                .public_key
                .as_ref()
                .and_then(|key| bedrockrs_net::identity::parse_public_key(key).ok())
                .is_some_and(|key| key == identity.public_key);
            if !matches {
                tracing::warn!("Turned away a login that doesn't match its connection");
                return Reply::disconnect(
                    DisconnectReason::NOT_AUTHENTICATED,
                    "BedrockRS: your login does not match this connection.",
                );
            }
        }

        // Verified tokens always name the player; the fallbacks only apply to
        // unchecked tokens in offline mode.
        self.uuid = claims.identity.unwrap_or_else(|| {
            tracing::debug!("login has no persistent identity; using a random UUID");
            Uuid::new_v4()
        });
        self.player = claims
            .display_name
            .unwrap_or_else(|| String::from("Player"));
        self.ids = PlayerIds {
            pfid: claims.pfid,
            xuid: claims.xuid,
        };
        // Returning players start where they left.
        if let Some(saved) = self.world.load_player(self.uuid) {
            self.movement = Movement::from_saved(&saved);
            self.flying = saved.flying;
            match saved.health {
                // Someone who left while dead comes back at the spawn.
                Some(health) if health <= 0.0 => self.movement = self.spawn_movement(),
                Some(health) => self.health.set(health),
                None => {}
            }
            if let Some(name) = &saved.game_mode {
                match GameMode::from_name(name) {
                    Some(mode) => self.game_mode = mode,
                    None => {
                        tracing::warn!(
                            "Ignored {}'s saved game mode {name:?}, which doesn't exist",
                            self.player
                        )
                    }
                }
            }
            if let Some(inventory) = &saved.inventory {
                self.inventory = Inventory::from_saved(inventory);
            }
            tracing::debug!(uuid = %self.uuid, ?saved, "restored the player's position");
        }
        tracing::debug!(name = %self.player, uuid = %self.uuid, "player logged in");
        self.stage = Stage::ResourcePacks;
        Reply {
            packets: vec![
                PlayStatus {
                    status: PlayStatusCode::LoginSuccess,
                }
                .encode(),
                ResourcePacksInfo::default().encode(),
            ],
            events: vec![SessionEvent::LoggedIn {
                uuid: self.uuid,
                ids: self.ids.clone(),
            }],
            ..Reply::default()
        }
    }

    pub(super) fn pack_response(
        &mut self,
        response: ResourcePackClientResponse,
    ) -> Result<Reply, SessionError> {
        match response.response {
            PackResponse::DownloadingFinished => {
                let stack = ResourcePackStack {
                    texture_pack_required: false,
                    packs: EXEMPTED_PACKS
                        .iter()
                        .map(|(uuid, version)| StackPack {
                            id: (*uuid).to_owned(),
                            version: (*version).to_owned(),
                            sub_pack_name: String::new(),
                        })
                        .collect(),
                    base_game_version: GAME_VERSION.to_owned(),
                    experiments: Vec::new(),
                    experiments_previously_toggled: false,
                    include_editor_packs: false,
                };
                Ok(Reply::send(vec![stack.encode()]))
            }
            PackResponse::StackFinished => {
                self.stage = Stage::Spawning;
                Ok(Reply::send(vec![
                    JigsawStructureData::empty().encode(),
                    VoxelShapes.encode(),
                    start_game(
                        &self.world,
                        self.entity_id,
                        &self.movement,
                        self.game_mode,
                        self.default_game_mode,
                        self.permission,
                        &self.rules,
                    )
                    .encode(),
                    // Every vanilla item the server knows.
                    ITEM_REGISTRY.clone(),
                ]))
            }
            PackResponse::Downloading(_) => Ok(Reply::disconnect(
                DisconnectReason::RESOURCE_PACK_PROBLEM,
                "BedrockRS: this server has no resource packs to download.",
            )),
            PackResponse::Cancel => Ok(Reply {
                close: true,
                ..Reply::default()
            }),
        }
    }

    /// Grants a view distance and sends the chunks in it the client lacks,
    /// nearest first. The first time, this also lets the client spawn.
    pub(super) fn chunk_radius(&mut self, request: RequestChunkRadius) -> Reply {
        let radius = request.radius.clamp(1, MAX_VIEW_DISTANCE);
        let mut packets = vec![ChunkRadiusUpdated { radius }.encode()];
        packets.extend(self.stream_chunks(radius));
        if self.stage == Stage::Spawning {
            // The player's own entity data: without HasGravity the client
            // does not pull its player down, and they float.
            packets.push(
                SetActorData {
                    entity_runtime_id: self.entity_id,
                    metadata: player_metadata(&self.player, false),
                    tick: 0,
                }
                .encode(),
            );
            // Speeds: the client moves its own player, using the movement
            // attribute and the walk and fly speeds of its ability layer.
            packets.push(self.own_attributes().encode());
            packets.push(self.own_abilities().encode());
            // What the player carries.
            packets.extend(self.inventory.content().iter().map(Encode::encode));
            packets.push(
                PlayStatus {
                    status: PlayStatusCode::PlayerSpawn,
                }
                .encode(),
            );
            packets.push(CREATIVE_CONTENT.clone());
            self.stage = Stage::Initializing;
        }
        Reply {
            packets,
            events: self.view_event(),
            ..Reply::default()
        }
    }

    /// Vanilla defaults for the attributes that govern how the client moves
    /// its player, plus health.
    pub(super) fn own_attributes(&self) -> UpdateAttributes {
        UpdateAttributes {
            entity_runtime_id: self.entity_id,
            attributes: vec![
                Attribute::at_default("minecraft:movement", 0.0, f32::MAX, ability::WALK_SPEED),
                Attribute::at_default("minecraft:underwater_movement", 0.0, f32::MAX, 0.02),
                Attribute::at_default("minecraft:lava_movement", 0.0, f32::MAX, 0.02),
                self.health_value(),
            ],
            tick: 0,
        }
    }

    /// What the player's game mode and permission let them do, at vanilla
    /// walk and fly speeds, and their permission.
    pub(super) fn own_abilities(&self) -> UpdateAbilities {
        // Flying is an ability value too: granting it keeps a player who
        // left in the air flying when they return.
        let mut values = self.game_mode.abilities(self.flying);
        if !self.permission.may_build() {
            // Visitors only look around.
            values &= !(ability::BUILD
                | ability::MINE
                | ability::DOORS_AND_SWITCHES
                | ability::OPEN_CONTAINERS
                | ability::ATTACK_PLAYERS
                | ability::ATTACK_MOBS);
        }
        if self.permission.is_operator() {
            values |= ability::OPERATOR_COMMANDS | ability::TELEPORT;
        }
        UpdateAbilities(AbilityData {
            entity_unique_id: i64::try_from(self.entity_id)
                .expect("entity IDs stay far below i64::MAX"),
            player_permissions: self.permission.id(),
            // Commands at the "any" or operator level.
            command_permissions: u8::from(self.permission.is_operator()),
            layers: vec![AbilityLayer::base(values)],
        })
    }

    pub(super) fn initialized(&mut self, packet: SetLocalPlayerAsInitialized) -> Reply {
        if packet.entity_runtime_id != self.entity_id {
            tracing::debug!(
                runtime_id = packet.entity_runtime_id,
                "client initialized an unexpected entity"
            );
        }
        if self.stage == Stage::InGame {
            return Reply::default();
        }
        self.stage = Stage::InGame;
        tracing::info!("{} joined the game", self.player);
        tracing::debug!(uuid = %self.uuid, "joined");
        // What they hold goes in with them: a slot chosen while the world
        // was loading had nobody to tell yet.
        self.shown_held = self
            .inventory
            .hotbar(self.held_slot.into())
            .map_or(ItemInstance::EMPTY, |stack| stack.instance());
        Reply {
            events: vec![SessionEvent::Joined {
                profile: Profile {
                    name: self.player.clone(),
                    uuid: self.uuid,
                    skin: self
                        .skin
                        .clone()
                        .unwrap_or_else(|| placeholder_skin(self.uuid)),
                },
                movement: self.movement,
                view: View {
                    centre: self.view.centre().unwrap_or_else(|| self.movement.chunk()),
                    radius: self.view.radius(),
                },
                inventory: self.inventory.saved(),
                held: (self.shown_held, self.held_slot),
                armor: Box::new(self.inventory.armor()),
            }]
            .into_iter()
            // A returning player still flying: remembered for the next save.
            .chain(self.flying.then_some(SessionEvent::Flying(true)))
            .collect(),
            ..Reply::default()
        }
    }
}

/// StartGame for the flat world, with the player in their game mode where
/// they stand.
fn start_game(
    world: &World,
    entity_id: u64,
    movement: &Movement,
    game_mode: GameMode,
    default_game_mode: GameMode,
    permission: Permission,
    rules: &game_rules::Values,
) -> StartGame {
    let spawn = world.spawn();
    StartGame {
        entity_unique_id: i64::try_from(entity_id).expect("entity IDs stay far below i64::MAX"),
        entity_runtime_id: entity_id,
        player_game_mode: game_mode.id(),
        player_position: movement.position,
        pitch: movement.pitch,
        yaw: movement.yaw,
        world_seed: 0,
        spawn_biome_type: 0,
        user_defined_biome_name: "plains".into(),
        dimension: OVERWORLD,
        generator: 2,
        world_game_mode: default_game_mode.id(),
        hardcore: false,
        difficulty: 0,
        world_spawn: spawn,
        achievements_disabled: true,
        editor_world_type: 0,
        created_in_editor: false,
        exported_from_editor: false,
        day_cycle_lock_time: 0,
        education_edition_offer: 0,
        education_features_enabled: false,
        education_product_id: String::new(),
        rain_level: 0.0,
        lightning_level: 0.0,
        confirmed_platform_locked_content: false,
        multiplayer_game: true,
        lan_broadcast_enabled: true,
        xbl_broadcast_mode: 0,
        platform_broadcast_mode: 0,
        commands_enabled: true,
        texture_pack_required: false,
        game_rules: rules.packet_rules(),
        experiments: Vec::new(),
        experiments_previously_toggled: false,
        bonus_chest_enabled: false,
        start_with_map_enabled: false,
        player_permissions: permission.id(),
        server_chunk_tick_radius: 4,
        has_locked_behaviour_pack: false,
        has_locked_texture_pack: false,
        from_locked_world_template: false,
        msa_gamertags_only: false,
        from_world_template: false,
        world_template_settings_locked: false,
        only_spawn_v1_villagers: false,
        persona_disabled: false,
        custom_skins_disabled: false,
        emote_chat_muted: false,
        base_game_version: GAME_VERSION.into(),
        limited_world_width: 0,
        limited_world_depth: 0,
        new_nether: false,
        force_experimental_gameplay: None,
        chat_restriction_level: 0,
        disable_player_interactions: false,
        server_editor_connection_policy: 0,
        allow_anonymous_block_drops_in_editor_worlds: false,
        level_id: String::new(),
        world_name: world.name().to_owned(),
        template_content_identity: String::new(),
        trial: false,
        player_movement_settings: PlayerMovementSettings {
            rewind_history_size: 0,
            server_authoritative_block_breaking: true,
        },
        // Noon.
        time: 6000,
        enchantment_seed: 0,
        blocks: palette().data_driven().to_vec(),
        multiplayer_correlation_id: Uuid::new_v4().to_string(),
        server_authoritative_inventory: true,
        game_version: GAME_VERSION.into(),
        property_data: Compound::new(),
        server_block_state_checksum: 0,
        world_template_id: [0; 16],
        client_side_generation: false,
        use_block_network_id_hashes: true,
        server_authoritative_sound: false,
        server_id: String::new(),
        scenario_id: String::new(),
        world_id: String::new(),
        owner_id: String::new(),
    }
}
