use super::*;

#[test]
fn joins_once_initialized_then_relays_chat() {
    let mut session = spawning_session();
    let request = RequestChunkRadius {
        radius: 2,
        max_radius: 32,
    };
    session.handle(&request.encode()).unwrap();

    // Chat before spawning finishes is ignored.
    let chat = |message: &str| Text {
        text_type: TextType::Chat,
        source_name: "Spoofed".into(),
        ..Text::raw(message)
    };
    assert_eq!(
        session.handle(&chat("too early").encode()).unwrap().events,
        []
    );

    let initialized = SetLocalPlayerAsInitialized {
        entity_runtime_id: PLAYER_ENTITY_ID,
    };
    let reply = session.handle(&initialized.encode()).unwrap();
    let [
        SessionEvent::Joined {
            profile,
            movement,
            view,
            inventory,
            held,
            ..
        },
    ] = &reply.events[..]
    else {
        panic!("expected a join, got {:?}", reply.events);
    };
    assert_eq!(profile.name, session.player());
    assert!(!profile.uuid.is_nil());
    // A new player carries nothing.
    assert_eq!(*inventory, SavedInventory::default());
    assert_eq!(*held, (ItemInstance::EMPTY, 0));
    // Their client shows the chunks around the spawn.
    assert_eq!(
        *view,
        View {
            centre: ChunkPos::new(0, 0),
            radius: 2
        }
    );
    // The player joins where StartGame put them: eyes above the spawn block.
    assert_eq!(
        movement.position,
        Vec3 {
            x: 0.5,
            y: -60.0 + EYE_HEIGHT,
            z: 0.5
        }
    );
    // Initializing again does not join twice.
    assert!(
        session
            .handle(&initialized.encode())
            .unwrap()
            .events
            .is_empty()
    );

    // The author comes from the session, never the packet; control
    // characters cannot start a fake line.
    let reply = session
        .handle(&chat("  hi\n<Admin> op me ").encode())
        .unwrap();
    assert_eq!(
        reply.events,
        [SessionEvent::Chat("hi <Admin> op me".into())]
    );
    assert!(reply.packets.is_empty());
}

#[test]
fn rejects_overlong_chat_with_a_warning() {
    let mut session = spawning_session();
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

    let long = Text {
        text_type: TextType::Chat,
        ..Text::raw("a".repeat(MAX_CHAT_LENGTH + 1))
    };
    let reply = session.handle(&long.encode()).unwrap();
    assert!(reply.events.is_empty());
    let warning: Text = decode_only(&reply.packets[0]);
    assert!(
        warning.message.contains("at most 512"),
        "{}",
        warning.message
    );

    // Other text types from a client are ignored.
    let tip = Text {
        text_type: TextType::Tip,
        ..Text::raw("hi")
    };
    let reply = session.handle(&tip.encode()).unwrap();
    assert!(reply.events.is_empty() && reply.packets.is_empty());
}

#[test]
fn changing_game_mode_tells_the_client_and_is_saved() {
    let mut session = in_game_session();
    session
        .handle(&flags_input(vec![input_flag::START_FLYING]))
        .unwrap();

    let reply = session.set_game_mode(GameMode::Survival);
    assert_eq!(
        ids(&reply),
        [id::SET_PLAYER_GAME_TYPE, id::UPDATE_ABILITIES]
    );
    assert_eq!(
        reply.events,
        [
            SessionEvent::GameModeChanged(GameMode::Survival),
            SessionEvent::Flying(false),
        ],
        "survival players cannot fly"
    );
    let (_, saved) = session.saved_player().unwrap();
    assert_eq!(saved.game_mode.as_deref(), Some("survival"));
    assert!(!saved.flying);

    // Survival clients cannot start flying either.
    let reply = session
        .handle(&flags_input(vec![input_flag::START_FLYING]))
        .unwrap();
    assert!(reply.events.contains(&SessionEvent::Flying(false)));

    // Spectators always fly.
    let reply = session.set_game_mode(GameMode::Spectator);
    assert!(reply.events.contains(&SessionEvent::Flying(true)));
}

#[test]
fn players_changed_before_spawning_start_in_their_mode() {
    let mut session = session_with_mode(GameMode::Survival);
    assert_eq!(session.game_mode(), GameMode::Survival);
    let reply = session.set_game_mode(GameMode::Adventure);
    assert!(
        reply.packets.is_empty() && reply.events.is_empty(),
        "not in the world yet"
    );
    assert_eq!(session.game_mode(), GameMode::Adventure);
}

fn session_with_mode(mode: GameMode) -> Session {
    Session::new(None, Arc::new(World::new()), PLAYER_ENTITY_ID, mode)
}

fn command_request(line: &str) -> Vec<u8> {
    let mut bytes = vec![id::COMMAND_REQUEST as u8];
    let mut writer = bedrockrs_protocol::io::Writer::new();
    writer.string(line);
    writer.string("player");
    writer.uuid([3; 16]);
    writer.string("");
    writer.i64_le(0);
    writer.bool(false);
    writer.string("52");
    bytes.extend(writer.into_bytes());
    bytes
}

#[test]
fn slash_commands_are_passed_on_and_answered() {
    let mut session = in_game_session();
    let reply = session.handle(&command_request("/gamemode\ns")).unwrap();
    let [SessionEvent::Command { line, origin }] = &reply.events[..] else {
        panic!("expected a command, got {:?}", reply.events);
    };
    assert_eq!(line, "/gamemode s", "control characters become spaces");
    assert_eq!(origin.uuid, [3; 16]);

    let output = session.command_output(origin.clone(), &CommandReply::error("No."));
    assert_eq!(
        packet::read_header(&output).unwrap().0.id,
        id::COMMAND_OUTPUT
    );

    // The player is who runs it, with their operator status.
    let Sender::Player(sender) = session.command_sender() else {
        panic!("a player sends commands");
    };
    assert!(!sender.operator);
    let reply = session.permission_changed(Permission::Operator);
    assert_eq!(ids(&reply), [id::UPDATE_ABILITIES, id::TEXT]);
    let Sender::Player(sender) = session.command_sender() else {
        panic!("a player sends commands");
    };
    assert!(sender.operator);
}

#[test]
fn commands_and_plugins_reach_every_game_mode() {
    let mut session = in_game_session();
    assert!(session.damage(DamageCause::Lava, 5.0).events.is_empty());
    assert_eq!(
        damage_events(&session.damage(DamageCause::SelfDestruct, f32::MAX)),
        [(DamageCause::SelfDestruct, f32::MAX)]
    );
    let reply = session.set_health(0.0);
    assert!(
        reply.events.iter().any(|event| matches!(
            event,
            SessionEvent::Died {
                cause: DamageCause::Override,
                ..
            }
        )),
        "{:?}",
        reply.events
    );
}
