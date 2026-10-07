use super::*;

fn place(slot: i32, block_position: BlockPos, face: u8) -> Vec<u8> {
    let held_item =
        Inventory::with_hotbar(&TEST_KIT).content()[0].content[slot.clamp(0, 35) as usize];
    InventoryTransaction::UseItem(UseItem {
        action: use_item_action::CLICK_BLOCK,
        trigger: 1,
        block_position,
        face,
        hotbar_slot: slot,
        held_item,
        player_position: Vec3::default(),
        clicked_position: Vec3::default(),
        block_runtime_id: 0,
        client_prediction: 1,
    })
    .encode()
}

#[test]
fn places_hotbar_blocks_against_the_clicked_face() {
    let mut session = in_game_session();
    // Click the top (face 1) of the grass two blocks east of the player.
    let grass = BlockPos { x: 2, y: -61, z: 0 };
    let reply = session.handle(&place(0, grass, 1)).unwrap();
    let stone = bedrockrs_protocol::block::BlockState::new("minecraft:stone").network_id();
    assert_eq!(
        reply.events,
        [
            SessionEvent::PlacedBlock {
                pos: BlockPos { x: 2, y: -60, z: 0 },
                block: stone,
                replacing: None,
            },
            SessionEvent::Swing
        ]
    );
    assert!(reply.packets.is_empty());
}

fn use_on_block(
    session: &Session,
    slot: i32,
    block_position: BlockPos,
    face: u8,
    click_y: f32,
) -> Vec<u8> {
    let held_item = session.inventory().hotbar(slot).unwrap().instance();
    InventoryTransaction::UseItem(UseItem {
        action: use_item_action::CLICK_BLOCK,
        trigger: 1,
        block_position,
        face,
        hotbar_slot: slot,
        held_item,
        player_position: Vec3::default(),
        clicked_position: Vec3 {
            x: 0.5,
            y: click_y,
            z: 0.5,
        },
        block_runtime_id: 0,
        client_prediction: 1,
    })
    .encode()
}

#[test]
fn placed_blocks_take_the_state_the_player_placed_them_in() {
    let mut session = in_game_session();
    session.inventory = Inventory::with_hotbar(&["minecraft:oak_stairs", "minecraft:torch"]);
    // Facing south (yaw 0), on top of the grass two blocks east.
    let grass = BlockPos { x: 2, y: -61, z: 0 };
    let reply = session
        .handle(&use_on_block(&session, 0, grass, 1, 1.0))
        .unwrap();
    let stairs = BlockState::new("minecraft:oak_stairs")
        .with("minecraft:corner", StateValue::String("none".into()))
        .with("upside_down_bit", StateValue::Byte(0))
        .with("weirdo_direction", StateValue::Int(2));
    assert_eq!(
        reply.events[0],
        SessionEvent::PlacedBlock {
            pos: BlockPos { x: 2, y: -60, z: 0 },
            block: stairs.network_id(),
            replacing: None,
        }
    );

    // A torch cannot hang from the bottom of a block: the client's
    // prediction is undone with what is really there, air.
    let reply = session
        .handle(&use_on_block(&session, 1, grass, 0, 0.0))
        .unwrap();
    assert!(reply.events.is_empty());
    assert_eq!(ids(&reply), [id::UPDATE_BLOCK, id::UPDATE_BLOCK]);
}

#[test]
fn clicking_the_top_of_a_bottom_slab_doubles_it_in_place() {
    let mut session = in_game_session();
    session.inventory = Inventory::with_hotbar(&["minecraft:oak_slab"]);
    let slab_at = BlockPos { x: 2, y: -60, z: 0 };
    let bottom = BlockState::new("minecraft:oak_slab").with(
        "minecraft:vertical_half",
        StateValue::String("bottom".into()),
    );
    session.world.set_block(slab_at, bottom.network_id());

    let reply = session
        .handle(&use_on_block(&session, 0, slab_at, 1, 1.0))
        .unwrap();
    let double = BlockState::new("minecraft:oak_double_slab").with(
        "minecraft:vertical_half",
        StateValue::String("bottom".into()),
    );
    assert_eq!(
        reply.events[0],
        SessionEvent::PlacedBlock {
            pos: slab_at,
            block: double.network_id(),
            replacing: Some(bottom.network_id()),
        }
    );
}

#[test]
fn client_swings_reach_others_unless_already_shown() {
    let mut session = in_game_session();
    let swing = |source: Option<&str>| Animate {
        action: Animate::SWING_ARM,
        entity_runtime_id: PLAYER_ENTITY_ID,
        swing_source: source.map(str::to_owned),
    };
    for source in [Some("attack"), None] {
        let reply = session.handle(&swing(source).encode()).unwrap();
        assert_eq!(reply.events, [SessionEvent::Swing], "{source:?}");
    }
    for source in ["build", "mine", "dropitem"] {
        assert!(
            session
                .handle(&swing(Some(source)).encode())
                .unwrap()
                .events
                .is_empty()
        );
    }
}

#[test]
fn placing_uses_what_the_server_says_is_held() {
    let mut session = in_game_session();
    session
        .handle(&swap_request(-1, 0, 2, &session).encode())
        .unwrap();
    // The client still claims to hold stone in slot 0: refused.
    let grass = BlockPos { x: 2, y: -61, z: 0 };
    let reply = session.handle(&place(0, grass, 1)).unwrap();
    assert!(reply.events.is_empty());
    assert_eq!(ids(&reply), [id::UPDATE_BLOCK, id::UPDATE_BLOCK]);

    // Holding what the server has there, dirt, places dirt.
    let held = session.inventory().hotbar(0).unwrap().instance();
    let transaction = InventoryTransaction::UseItem(UseItem {
        action: use_item_action::CLICK_BLOCK,
        trigger: 1,
        block_position: grass,
        face: 1,
        hotbar_slot: 0,
        held_item: held,
        player_position: Vec3::default(),
        clicked_position: Vec3::default(),
        block_runtime_id: 0,
        client_prediction: 1,
    });
    let reply = session.handle(&transaction.encode()).unwrap();
    let dirt = bedrockrs_protocol::block::BlockState::new("minecraft:dirt").network_id();
    assert!(reply.events.contains(&SessionEvent::PlacedBlock {
        pos: BlockPos { x: 2, y: -60, z: 0 },
        block: dirt,
        replacing: None,
    }));
}

#[test]
fn refused_placements_are_undone_on_the_client() {
    let mut session = in_game_session();
    // The player stands on (0, -61, 0): placing on top of it would be
    // inside them. The client already shows the block, so it gets air back.
    for refused in [
        place(0, BlockPos { x: 0, y: -61, z: 0 }, 1),
        // Clicking the side of grass targets grass: not air.
        place(0, BlockPos { x: 2, y: -61, z: 0 }, 5),
        // Out of reach, and an empty slot.
        place(
            0,
            BlockPos {
                x: 32,
                y: -61,
                z: 0,
            },
            1,
        ),
        place(20, BlockPos { x: 2, y: -61, z: 0 }, 1),
    ] {
        let reply = session.handle(&refused).unwrap();
        assert!(reply.events.is_empty(), "{:?}", reply.events);
    }
    let reply = session
        .handle(&place(0, BlockPos { x: 0, y: -61, z: 0 }, 1))
        .unwrap();
    assert_eq!(
        ids(&reply),
        [id::UPDATE_BLOCK, id::UPDATE_BLOCK],
        "the target and the clicked block"
    );
}

#[test]
fn breaks_blocks_in_reach_from_either_packet() {
    let mut session = in_game_session();
    let spawn_eyes = Vec3 {
        x: 0.5,
        y: -60.0 + EYE_HEIGHT,
        z: 0.5,
    };
    let grass = BlockPos { x: 1, y: -61, z: 0 };

    // Creative clients start breaking in PlayerAuthInput, while standing still.
    let start = BlockAction {
        action: player_action::START_BREAK,
        position: grass,
        face: 1,
    };
    let reply = session
        .handle(&breaking_input(spawn_eyes, 0.0, vec![start]))
        .unwrap();
    assert!(reply.events.contains(&SessionEvent::BrokeBlock(grass)));
    // Aborting a break breaks nothing.
    let abort = BlockAction {
        action: player_action::ABORT_BREAK,
        ..start
    };
    let reply = session
        .handle(&breaking_input(spawn_eyes, 0.0, vec![abort]))
        .unwrap();
    assert!(reply.events.is_empty());

    // And report it in a PlayerAction.
    let destroy = PlayerAction {
        entity_runtime_id: PLAYER_ENTITY_ID,
        action: player_action::CREATIVE_DESTROY_BLOCK,
        block_position: grass,
        result_position: BlockPos::default(),
        face: 1,
    };
    let reply = session.handle(&destroy.encode()).unwrap();
    assert_eq!(reply.events, [SessionEvent::BrokeBlock(grass)]);

    // Out of reach, below the world or in an unloaded chunk: refused.
    for pos in [
        BlockPos {
            x: 22,
            y: -61,
            z: 0,
        },
        BlockPos { x: 0, y: -65, z: 0 },
        BlockPos {
            x: 0,
            y: -61,
            z: 900,
        },
    ] {
        let reply = session
            .handle(
                &PlayerAction {
                    block_position: pos,
                    ..destroy
                }
                .encode(),
            )
            .unwrap();
        assert!(reply.events.is_empty(), "{pos:?}");
    }
}

fn break_action(action: i32, position: BlockPos) -> BlockAction {
    BlockAction {
        action,
        position,
        face: 1,
    }
}

#[test]
fn game_modes_decide_how_blocks_break() {
    let mut session = in_game_session();
    session.set_game_mode(GameMode::Survival);
    let grass = BlockPos { x: 1, y: -61, z: 0 };

    // Outside creative, starting to break is not breaking: the client
    // says when it is done.
    let start = break_action(player_action::START_BREAK, grass);
    let reply = session
        .handle(&breaking_input(spawn_eyes(), 0.0, vec![start]))
        .unwrap();
    assert!(!reply.events.contains(&SessionEvent::BrokeBlock(grass)));
    let done = break_action(player_action::PREDICT_DESTROY_BLOCK, grass);
    let reply = session
        .handle(&breaking_input(spawn_eyes(), 0.0, vec![done]))
        .unwrap();
    assert!(reply.events.contains(&SessionEvent::BrokeBlock(grass)));

    // Instant breaks are for creative players only.
    let destroy = PlayerAction {
        entity_runtime_id: PLAYER_ENTITY_ID,
        action: player_action::CREATIVE_DESTROY_BLOCK,
        block_position: grass,
        result_position: BlockPos::default(),
        face: 1,
    };
    assert!(session.handle(&destroy.encode()).unwrap().events.is_empty());

    // Adventure players and spectators change no blocks at all.
    for mode in [GameMode::Adventure, GameMode::Spectator] {
        session.set_game_mode(mode);
        let reply = session
            .handle(&breaking_input(spawn_eyes(), 0.0, vec![done]))
            .unwrap();
        assert!(
            !reply.events.contains(&SessionEvent::BrokeBlock(grass)),
            "{mode:?}"
        );
        let reply = session
            .handle(&place(0, BlockPos { x: 2, y: -61, z: 0 }, 1))
            .unwrap();
        assert!(
            !reply
                .events
                .iter()
                .any(|event| matches!(event, SessionEvent::PlacedBlock { .. })),
            "{mode:?}"
        );
    }
}

#[test]
fn survival_placing_uses_up_the_held_block() {
    let mut session = in_game_session();
    let before = session.inventory().hotbar(0).unwrap().count;

    // Creative players keep their blocks.
    session
        .handle(&place(0, BlockPos { x: 2, y: -61, z: 0 }, 1))
        .unwrap();
    assert_eq!(session.inventory().hotbar(0).unwrap().count, before);

    session.set_game_mode(GameMode::Survival);
    let reply = session
        .handle(&place(0, BlockPos { x: 3, y: -61, z: 0 }, 1))
        .unwrap();
    assert_eq!(session.inventory().hotbar(0).unwrap().count, before - 1);
    assert_eq!(ids(&reply), [id::INVENTORY_SLOT]);
    assert!(
        reply
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::PlacedBlock { .. }))
    );
    assert!(
        reply
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::InventoryChanged(_))),
        "the change is saved"
    );
}
