use super::*;

#[test]
fn unverified_players_are_disconnected() {
    let (identity, token) = client();
    let mut session = session(Some(identity));
    session
        .handle(
            &RequestNetworkSettings {
                client_protocol: PROTOCOL_VERSION,
            }
            .encode(),
        )
        .unwrap();
    session.handle(&login_packet(token)).unwrap();
    let reply = session.authenticated(Err(AuthError::BadSignature));
    assert!(reply.close);
    let disconnect: Disconnect = decode_only(&reply.packets[0]);
    assert_eq!(disconnect.reason, DisconnectReason::NOT_AUTHENTICATED);
    let message = disconnect.message.unwrap().message;
    assert!(message.contains("signature"), "{message}");
    assert!(reply.events.is_empty(), "no identity is claimed");
}

#[test]
fn a_verified_login_claims_its_uuid() {
    let (identity, token) = client();
    let mut session = session(Some(identity));
    session
        .handle(
            &RequestNetworkSettings {
                client_protocol: PROTOCOL_VERSION,
            }
            .encode(),
        )
        .unwrap();
    let reply = log_in(&mut session, token);
    let [SessionEvent::LoggedIn(uuid)] = reply.events[..] else {
        panic!("expected a login, got {:?}", reply.events);
    };
    assert_eq!(uuid, session.uuid);
    assert!(!uuid.is_nil());
}

#[test]
fn walks_the_login_handshake() {
    let (identity, token) = client();
    let mut session = session(Some(identity));

    let reply = session
        .handle(
            &RequestNetworkSettings {
                client_protocol: PROTOCOL_VERSION,
            }
            .encode(),
        )
        .unwrap();
    assert_eq!(ids(&reply), [id::NETWORK_SETTINGS]);
    assert_eq!(reply.enable_compression, Some(COMPRESSION));
    let settings: NetworkSettings = decode_only(&reply.packets[0]);
    assert_eq!(
        settings.compression_algorithm,
        CompressionAlgorithm::Flate.id()
    );

    let reply = log_in(&mut session, token);
    assert_eq!(ids(&reply), [id::PLAY_STATUS, id::RESOURCE_PACKS_INFO]);
    let status: PlayStatus = decode_only(&reply.packets[0]);
    assert_eq!(status.status, PlayStatusCode::LoginSuccess);
    assert_eq!(session.stage(), Stage::ResourcePacks);

    // Clients send their blob cache support at some point; it is ignored.
    let cache = [0x81, 0x01, 0x00];
    assert!(session.handle(&cache).unwrap().packets.is_empty());

    let reply = session
        .handle(&pack_response(PackResponse::DownloadingFinished))
        .unwrap();
    assert_eq!(ids(&reply), [id::RESOURCE_PACK_STACK]);

    let reply = session
        .handle(&pack_response(PackResponse::StackFinished))
        .unwrap();
    assert_eq!(
        ids(&reply),
        [
            id::JIGSAW_STRUCTURE_DATA,
            id::VOXEL_SHAPES,
            id::START_GAME,
            id::ITEM_REGISTRY
        ]
    );
    assert!(!reply.close);
    assert_eq!(session.stage(), Stage::Spawning);
}

#[test]
fn spawns_after_sending_the_chunks_in_view() {
    let mut session = spawning_session();

    let request = RequestChunkRadius {
        radius: 4,
        max_radius: 32,
    };
    let reply = session.handle(&request.encode()).unwrap();
    let sent = ids(&reply);
    assert_eq!(
        sent[..2],
        [id::CHUNK_RADIUS_UPDATED, id::NETWORK_CHUNK_PUBLISHER_UPDATE]
    );
    // The chunks within a circle of radius 4: 49 of them.
    let chunks = sent.iter().filter(|id| **id == id::LEVEL_CHUNK).count();
    assert_eq!(chunks, 49);
    // The player's own entity data (gravity), attributes and abilities
    // (speeds), then their inventory, offhand and armour, come just
    // before PlayerSpawn and the creative inventory.
    assert_eq!(
        sent[sent.len() - 8..],
        [
            id::SET_ACTOR_DATA,
            id::UPDATE_ATTRIBUTES,
            id::UPDATE_ABILITIES,
            id::INVENTORY_CONTENT,
            id::INVENTORY_CONTENT,
            id::INVENTORY_CONTENT,
            id::PLAY_STATUS,
            id::CREATIVE_CONTENT,
        ]
    );
    let attributes = &reply.packets[sent.len() - 7];
    let movement = b"minecraft:movement";
    let at = attributes
        .windows(movement.len())
        .position(|window| window == movement)
        .expect("the movement attribute is sent");
    // The six floats before the name end with the default: vanilla's 0.1.
    assert_eq!(attributes[at - 5..at - 1], 0.1f32.to_le_bytes());
    let spawn: PlayStatus = decode_only(&reply.packets[sent.len() - 2]);
    assert_eq!(spawn.status, PlayStatusCode::PlayerSpawn);
    assert_eq!(session.stage(), Stage::Initializing);

    // Movement input arrives every tick and is ignored for now.
    let auth_input = [0x90, 0x01, 0x00];
    assert!(session.handle(&auth_input).unwrap().packets.is_empty());

    let initialized = SetLocalPlayerAsInitialized {
        entity_runtime_id: PLAYER_ENTITY_ID,
    };
    assert!(
        session
            .handle(&initialized.encode())
            .unwrap()
            .packets
            .is_empty()
    );
    assert_eq!(session.stage(), Stage::InGame);

    // Changing the render distance later resends chunks without respawning.
    let reply = session.handle(&request.encode()).unwrap();
    assert!(!ids(&reply).contains(&id::PLAY_STATUS));
}

#[test]
fn view_distance_is_capped() {
    let mut session = spawning_session();
    let request = RequestChunkRadius {
        radius: 64,
        max_radius: 64,
    };
    let reply = session.handle(&request.encode()).unwrap();
    let (_, mut payload) = packet::read_header(&reply.packets[0]).unwrap();
    assert_eq!(payload.var_i32().unwrap(), MAX_VIEW_DISTANCE);
}

#[test]
fn rejects_other_protocol_versions_before_compression() {
    for (client_protocol, expected) in [
        (PROTOCOL_VERSION - 1, PlayStatusCode::LoginFailedClient),
        (PROTOCOL_VERSION + 1, PlayStatusCode::LoginFailedServer),
    ] {
        let mut session = session(None);
        let reply = session
            .handle(&RequestNetworkSettings { client_protocol }.encode())
            .unwrap();
        assert!(reply.close);
        assert_eq!(reply.enable_compression, None);
        let status: PlayStatus = decode_only(&reply.packets[0]);
        assert_eq!(status.status, expected);
    }
}

#[test]
fn login_must_carry_the_key_proven_during_signaling() {
    let (identity, _) = client();
    let (_, other_token) = client();
    let mut session = session(Some(identity));
    session
        .handle(
            &RequestNetworkSettings {
                client_protocol: PROTOCOL_VERSION,
            }
            .encode(),
        )
        .unwrap();

    let reply = log_in(&mut session, other_token);
    assert!(reply.close);
    let disconnect: Disconnect = decode_only(&reply.packets[0]);
    assert_eq!(disconnect.reason, DisconnectReason::NOT_AUTHENTICATED);
    assert_eq!(session.stage(), Stage::Authenticating);
}

#[test]
fn out_of_order_packets_disconnect_with_a_reason() {
    let (_, token) = client();
    let mut session = session(None);
    let err = session.handle(&login_packet(token)).unwrap_err();
    assert!(matches!(
        err,
        SessionError::UnexpectedPacket {
            id: id::LOGIN,
            stage: Stage::RequestNetworkSettings
        }
    ));

    let reply = err.into_reply();
    assert!(reply.close);
    let disconnect: Disconnect = decode_only(&reply.packets[0]);
    assert_eq!(disconnect.reason, DisconnectReason::UNEXPECTED_PACKET);
}

#[test]
fn new_players_start_centred_on_the_spawn_block() {
    let mut session = session(None);
    session
        .handle(
            &RequestNetworkSettings {
                client_protocol: PROTOCOL_VERSION,
            }
            .encode(),
        )
        .unwrap();
    let (_, token) = client();
    log_in(&mut session, token);
    session
        .handle(&pack_response(PackResponse::DownloadingFinished))
        .unwrap();
    let reply = session
        .handle(&pack_response(PackResponse::StackFinished))
        .unwrap();
    // StartGame's player position: eyes above the middle of block (0, -60, 0).
    let start_game = &reply.packets[2];
    let mut position = Vec::new();
    for value in [0.5f32, -60.0 + EYE_HEIGHT, 0.5] {
        position.extend(value.to_le_bytes());
    }
    assert!(
        start_game
            .windows(position.len())
            .any(|window| window == position),
        "StartGame places the player at (0.5, {}, 0.5)",
        -60.0 + EYE_HEIGHT
    );
}
