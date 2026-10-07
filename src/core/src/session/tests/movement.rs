use super::*;

#[test]
fn flying_is_acknowledged_and_remembered() {
    let mut session = in_game_session();
    let here = Vec3 {
        x: 0.5,
        y: -58.0 + EYE_HEIGHT,
        z: 0.5,
    };
    let with_flags = |flags: Vec<i32>| {
        let mut input = PlayerAuthInput::decode_payload(&mut bedrockrs_protocol::io::Reader::new(
            &breaking_input(here, 0.0, Vec::new())[2..],
        ))
        .unwrap();
        input.input_flags = flags;
        input.encode()
    };
    let reply = session
        .handle(&with_flags(vec![input_flag::START_FLYING]))
        .unwrap();
    assert!(reply.events.contains(&SessionEvent::Flying(true)));
    // The client is answered with abilities that include flying.
    assert_eq!(ids(&reply), [id::UPDATE_ABILITIES]);
    assert!(session.saved_player().unwrap().1.flying);

    let reply = session
        .handle(&with_flags(vec![input_flag::STOP_FLYING]))
        .unwrap();
    assert!(reply.events.contains(&SessionEvent::Flying(false)));
    assert!(!session.saved_player().unwrap().1.flying);
}

#[test]
fn swings_and_sneaking_come_from_input_flags() {
    let mut session = in_game_session();
    let here = Vec3 {
        x: 0.5,
        y: -60.0 + EYE_HEIGHT,
        z: 0.5,
    };
    let with_flags = |flags: Vec<i32>| {
        let mut input = PlayerAuthInput::decode_payload(&mut bedrockrs_protocol::io::Reader::new(
            &breaking_input(here, 0.0, Vec::new())[2..],
        ))
        .unwrap();
        input.input_flags = flags;
        input.encode()
    };
    let reply = session
        .handle(&with_flags(vec![input_flag::MISSED_SWING]))
        .unwrap();
    assert_eq!(reply.events, [SessionEvent::Swing]);
    let reply = session
        .handle(&with_flags(vec![input_flag::START_SNEAKING]))
        .unwrap();
    assert_eq!(reply.events, [SessionEvent::Sneaking(true)]);
    let reply = session
        .handle(&with_flags(vec![input_flag::STOP_SNEAKING]))
        .unwrap();
    assert_eq!(reply.events, [SessionEvent::Sneaking(false)]);
}

/// The chunk coordinates of the LevelChunks in a reply.
fn chunks_in(reply: &Reply) -> Vec<(i32, i32)> {
    reply
        .packets
        .iter()
        .filter_map(|packet| {
            let (header, mut payload) = packet::read_header(packet).unwrap();
            (header.id == id::LEVEL_CHUNK)
                .then(|| (payload.var_i32().unwrap(), payload.var_i32().unwrap()))
        })
        .collect()
}

#[test]
fn streams_chunks_when_crossing_into_another_chunk() {
    let mut session = spawning_session();
    let request = RequestChunkRadius {
        radius: 8,
        max_radius: 8,
    };
    let reply = session.handle(&request.encode()).unwrap();
    let first = chunks_in(&reply);
    assert_eq!(
        first.len(),
        197,
        "the circle of radius 8 around chunk (0, 0)"
    );
    session
        .handle(
            &SetLocalPlayerAsInitialized {
                entity_runtime_id: PLAYER_ENTITY_ID,
            }
            .encode(),
        )
        .unwrap();

    // Walking within the spawn chunk sends nothing new.
    let eyes = -60.0 + EYE_HEIGHT;
    let within = Vec3 {
        x: 15.5,
        y: eyes,
        z: 8.5,
    };
    let reply = session.handle(&auth_input(within, 0.0)).unwrap();
    assert!(reply.packets.is_empty());

    // Stepping east into chunk (1, 0) recentres the view and sends the new edge.
    let across = Vec3 {
        x: 16.2,
        y: eyes,
        z: 8.5,
    };
    let reply = session.handle(&auth_input(across, 0.0)).unwrap();
    assert_eq!(
        packet::read_header(&reply.packets[0]).unwrap().0.id,
        id::NETWORK_CHUNK_PUBLISHER_UPDATE
    );
    let update: NetworkChunkPublisherUpdate = decode_only(&reply.packets[0]);
    assert_eq!(
        update.position,
        BlockPos {
            x: 16,
            y: -60,
            z: 8
        }
    );
    assert_eq!(update.radius, 8 << 4);
    let streamed = chunks_in(&reply);
    assert!(streamed.contains(&(9, 0)), "{streamed:?}");
    assert!(streamed.iter().all(|chunk| !first.contains(chunk)));
    // The rest of the server hears about the new view, for entity tracking.
    assert!(reply.events.contains(&SessionEvent::Viewing(View {
        centre: ChunkPos::new(1, 0),
        radius: 8
    })));

    // Coming back sends the west edge again, which the client unloaded.
    let reply = session.handle(&auth_input(within, 0.0)).unwrap();
    assert!(chunks_in(&reply).contains(&(-8, 0)));
}

#[test]
fn reports_movement_only_in_game_and_when_it_changes() {
    let mut session = spawning_session();
    let here = Vec3 {
        x: 10.0,
        y: -58.38,
        z: 3.0,
    };
    // Input arrives before the player is initialized; it is ignored.
    assert!(
        session
            .handle(&auth_input(here, 0.0))
            .unwrap()
            .events
            .is_empty()
    );

    session
        .handle(
            &RequestChunkRadius {
                radius: 1,
                max_radius: 1,
            }
            .encode(),
        )
        .unwrap();
    session
        .handle(
            &SetLocalPlayerAsInitialized {
                entity_runtime_id: PLAYER_ENTITY_ID,
            }
            .encode(),
        )
        .unwrap();

    let reply = session.handle(&auth_input(here, 90.0)).unwrap();
    let [SessionEvent::Moved(movement)] = &reply.events[..] else {
        panic!("expected a move, got {:?}", reply.events);
    };
    assert_eq!((movement.position, movement.yaw), (here, 90.0));
    assert!(movement.on_ground);

    // Standing still sends the same input every tick.
    assert!(
        session
            .handle(&auth_input(here, 90.0))
            .unwrap()
            .events
            .is_empty()
    );
    // Positions that are not numbers are ignored.
    let nowhere = Vec3 {
        x: f32::NAN,
        ..here
    };
    assert!(
        session
            .handle(&auth_input(nowhere, 90.0))
            .unwrap()
            .events
            .is_empty()
    );
}
