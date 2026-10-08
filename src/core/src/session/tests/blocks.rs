use super::*;

/// A refused placement shows the client what is really at the target, the
/// clicked block, and above and beside the target, where a second half might
/// be: six or seven blocks, as the clicked one is beside it or below.
fn assert_rolled_back(reply: &Reply) {
    let ids = ids(reply);
    assert!(
        (6..=7).contains(&ids.len()) && ids.iter().all(|id| *id == id::UPDATE_BLOCK),
        "{ids:?}"
    );
}

fn place(slot: i32, block_position: BlockPos, face: u8) -> Vec<u8> {
    let held_item =
        Inventory::with_hotbar(&TEST_KIT).content()[0].content[slot.clamp(0, 35) as usize];
    InventoryTransaction::from(TransactionData::UseItem(UseItem {
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
    }))
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
                other_half: None,
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
    InventoryTransaction::from(TransactionData::UseItem(UseItem {
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
    }))
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
            other_half: None,
        }
    );

    // A torch cannot hang from the bottom of a block: the client's
    // prediction is undone with what is really there, air.
    let reply = session
        .handle(&use_on_block(&session, 1, grass, 0, 0.0))
        .unwrap();
    assert!(reply.events.is_empty());
    assert_rolled_back(&reply);
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
            other_half: None,
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
    assert_rolled_back(&reply);

    // Holding what the server has there, dirt, places dirt.
    let held = session.inventory().hotbar(0).unwrap().instance();
    let transaction = InventoryTransaction::from(TransactionData::UseItem(UseItem {
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
    }));
    let reply = session.handle(&transaction.encode()).unwrap();
    let dirt = bedrockrs_protocol::block::BlockState::new("minecraft:dirt").network_id();
    assert!(reply.events.contains(&SessionEvent::PlacedBlock {
        pos: BlockPos { x: 2, y: -60, z: 0 },
        block: dirt,
        replacing: None,
        other_half: None,
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
    assert_rolled_back(&reply);
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

fn pick(position: BlockPos, add_block_nbt: bool) -> Vec<u8> {
    BlockPickRequest {
        position,
        add_block_nbt,
        hotbar_slot: 0,
    }
    .encode()
}

#[test]
fn picking_a_block_brings_its_item_into_the_hotbar() {
    let mut session = in_game_session();
    // The grass under the spawn is already in hotbar slot 1: it is held.
    let grass = BlockPos { x: 0, y: -61, z: 0 };
    let reply = session.handle(&pick(grass, false)).unwrap();
    assert_eq!(ids(&reply), [id::PLAYER_HOTBAR]);
    assert_eq!(session.held_slot, 1);
    assert!(matches!(
        reply.events[..],
        [SessionEvent::Holding { slot: 1, .. }]
    ));

    // A double slab gives its slab. The hotbar is full, so what is held
    // moves into the inventory and the slab takes its place.
    let slab_at = BlockPos { x: 2, y: -60, z: 0 };
    let double = BlockState::new("minecraft:oak_double_slab").with(
        "minecraft:vertical_half",
        StateValue::String("bottom".into()),
    );
    session.world.set_block(slab_at, double.network_id());
    let reply = session.handle(&pick(slab_at, true)).unwrap();
    assert_eq!(ids(&reply).last(), Some(&id::PLAYER_HOTBAR));
    assert!(ids(&reply).contains(&id::INVENTORY_CONTENT));
    let slab = items().by_name("minecraft:oak_slab").unwrap().network_id;
    assert_eq!(session.inventory().hotbar(1).unwrap().item, slab);
    let saved = session.inventory().saved();
    assert!(
        saved
            .main
            .iter()
            .any(|stack| stack.slot == 9 && stack.item == "minecraft:grass_block")
    );

    // Air, and blocks out of reach, give nothing.
    let air = BlockPos { x: 0, y: -50, z: 0 };
    assert!(ids(&session.handle(&pick(air, false)).unwrap()).is_empty());
    let far = BlockPos {
        x: 40,
        y: -61,
        z: 0,
    };
    assert!(ids(&session.handle(&pick(far, false)).unwrap()).is_empty());
}

#[test]
fn survival_players_only_pick_what_they_carry() {
    let mut session = in_game_session();
    session.set_game_mode(GameMode::Survival);
    // Logs are not in their inventory.
    let log_at = BlockPos { x: 2, y: -60, z: 0 };
    let log =
        BlockState::new("minecraft:oak_log").with("pillar_axis", StateValue::String("y".into()));
    session.world.set_block(log_at, log.network_id());
    assert!(ids(&session.handle(&pick(log_at, false)).unwrap()).is_empty());
    let grass = BlockPos { x: 0, y: -61, z: 0 };
    assert_eq!(
        ids(&session.handle(&pick(grass, false)).unwrap()),
        [id::PLAYER_HOTBAR]
    );
}

#[test]
fn doors_place_both_halves_and_open_unless_sneaking() {
    let mut session = in_game_session();
    session.inventory = Inventory::with_hotbar(&["minecraft:wooden_door", "minecraft:stone"]);
    let grass = BlockPos { x: 2, y: -61, z: 0 };
    let reply = session
        .handle(&use_on_block(&session, 0, grass, 1, 1.0))
        .unwrap();
    let Some(SessionEvent::PlacedBlock {
        pos,
        block,
        other_half: Some((upper_at, upper, _)),
        ..
    }) = reply.events.first().cloned()
    else {
        panic!("expected a door, got {:?}", reply.events);
    };
    let door_at = BlockPos { x: 2, y: -60, z: 0 };
    assert_eq!(pos, door_at);
    assert_eq!(upper_at, BlockPos { x: 2, y: -59, z: 0 });
    session.world.set_block(pos, block);
    session.world.set_block(upper_at, upper);

    // Using it, even holding stone, opens it.
    let reply = session
        .handle(&use_on_block(&session, 1, door_at, 4, 0.5))
        .unwrap();
    assert!(matches!(
        reply.events[0],
        SessionEvent::Toggled { pos, .. } if pos == door_at
    ));
    // Sneaking places the stone against it instead.
    session.sneaking = true;
    let reply = session
        .handle(&use_on_block(&session, 1, door_at, 4, 0.5))
        .unwrap();
    assert!(matches!(
        reply.events[0],
        SessionEvent::PlacedBlock { pos, .. } if pos == BlockPos { x: 1, y: -60, z: 0 }
    ));
}

#[test]
fn visitors_only_look_around() {
    use bedrockrs_protocol::packets::ability;

    use crate::permissions::Permission;

    let mut session = in_game_session();
    session.set_game_mode(GameMode::Survival);
    session.inventory = Inventory::with_hotbar(&["minecraft:wooden_door", "minecraft:stone"]);
    let grass = BlockPos { x: 2, y: -61, z: 0 };
    let reply = session
        .handle(&use_on_block(&session, 0, grass, 1, 1.0))
        .unwrap();
    let Some(SessionEvent::PlacedBlock {
        pos: door_at,
        block,
        other_half: Some((upper_at, upper, _)),
        ..
    }) = reply.events.first().cloned()
    else {
        panic!("expected a door, got {:?}", reply.events);
    };
    session.world.set_block(door_at, block);
    session.world.set_block(upper_at, upper);

    // Their client is told they may not build, mine or use doors.
    let reply = session.permission_changed(Permission::Visitor);
    assert_eq!(ids(&reply), [id::UPDATE_ABILITIES, id::TEXT]);
    let abilities = session.own_abilities().0;
    assert_eq!(abilities.player_permissions, 0);
    let values = abilities.layers[0].values;
    for denied in [ability::BUILD, ability::MINE, ability::DOORS_AND_SWITCHES] {
        assert_eq!(values & denied, 0, "{denied:#x}");
    }

    // And the server refuses it anyway.
    let done = break_action(player_action::PREDICT_DESTROY_BLOCK, grass);
    let reply = session
        .handle(&breaking_input(spawn_eyes(), 0.0, vec![done]))
        .unwrap();
    assert!(!reply.events.contains(&SessionEvent::BrokeBlock(grass)));
    let reply = session
        .handle(&use_on_block(&session, 1, door_at, 4, 0.5))
        .unwrap();
    assert!(
        !reply.events.iter().any(|event| matches!(
            event,
            SessionEvent::Toggled { .. } | SessionEvent::PlacedBlock { .. }
        )),
        "{:?}",
        reply.events
    );

    // Members may again.
    session.permission_changed(Permission::Member);
    assert_ne!(
        session.own_abilities().0.layers[0].values & ability::BUILD,
        0
    );
    let reply = session
        .handle(&breaking_input(spawn_eyes(), 0.0, vec![done]))
        .unwrap();
    assert!(reply.events.contains(&SessionEvent::BrokeBlock(grass)));
}

#[test]
fn ladders_go_on_full_blocks_only_and_grass_is_placed_into() {
    let mut session = in_game_session();
    session.inventory = Inventory::with_hotbar(&["minecraft:ladder", "minecraft:stone"]);
    let stone_at = BlockPos { x: 2, y: -60, z: 0 };
    let stone = items().by_name("minecraft:stone").unwrap();
    session
        .world
        .set_block(stone_at, stone.block_network_id.unwrap());
    // On the stone's east face: a ladder.
    let reply = session
        .handle(&use_on_block(&session, 0, stone_at, 5, 0.5))
        .unwrap();
    let Some(SessionEvent::PlacedBlock { pos, block, .. }) = reply.events.first().cloned() else {
        panic!("expected a ladder, got {:?}", reply.events);
    };
    session.world.set_block(pos, block);
    // On that ladder's east face: refused, it would float.
    let reply = session
        .handle(&use_on_block(&session, 0, pos, 5, 0.5))
        .unwrap();
    assert!(reply.events.is_empty(), "{:?}", reply.events);

    // Clicking short grass places into it, replacing it.
    let grass_at = BlockPos { x: 4, y: -60, z: 0 };
    let short_grass = crate::blocks::palette()
        .upgrade(&BlockState::new("minecraft:short_grass"))
        .unwrap()
        .network_id();
    session.world.set_block(grass_at, short_grass);
    let reply = session
        .handle(&use_on_block(&session, 1, grass_at, 4, 0.5))
        .unwrap();
    assert!(matches!(
        reply.events[0],
        SessionEvent::PlacedBlock { pos, replacing: Some(replaced), .. }
            if pos == grass_at && replaced == short_grass
    ));
}

#[test]
fn a_torch_clicked_onto_a_torch_stands_beside_it_if_it_can() {
    let mut session = in_game_session();
    session.inventory = Inventory::with_hotbar(&["minecraft:torch", "minecraft:trapdoor"]);
    let torch_at = BlockPos { x: 2, y: -60, z: 0 };
    let standing = crate::blocks::palette()
        .upgrade(
            &BlockState::new("minecraft:torch")
                .with("torch_facing_direction", StateValue::String("top".into())),
        )
        .unwrap();
    session.world.set_block(torch_at, standing.network_id());
    // Its east side: no wall to hang on, so it stands on the grass there.
    let reply = session
        .handle(&use_on_block(&session, 0, torch_at, 5, 0.5))
        .unwrap();
    let Some(SessionEvent::PlacedBlock { pos, block, .. }) = reply.events.first() else {
        panic!("expected a torch, got {:?}", reply.events);
    };
    assert_eq!(*pos, BlockPos { x: 3, y: -60, z: 0 });
    let placed = session.world.state_of(*block).unwrap();
    assert_eq!(placed.name, "minecraft:torch");
    assert_eq!(
        crate::support::text(placed, "torch_facing_direction"),
        Some("top")
    );
    // On top of the torch, nothing holds one: refused.
    let reply = session
        .handle(&use_on_block(&session, 0, torch_at, 1, 1.0))
        .unwrap();
    assert!(reply.events.is_empty(), "{:?}", reply.events);

    // A trapdoor faces away from the player (looking south, yaw 0: it faces
    // north), so it opens towards them: Dragonfly's 3 minus north's 0.
    let grass = BlockPos { x: 2, y: -61, z: 2 };
    let reply = session
        .handle(&use_on_block(&session, 1, grass, 1, 1.0))
        .unwrap();
    let Some(SessionEvent::PlacedBlock { block, .. }) = reply.events.first() else {
        panic!("expected a trapdoor, got {:?}", reply.events);
    };
    let trapdoor = session.world.state_of(*block).unwrap();
    assert_eq!(crate::support::int(trapdoor, "direction"), Some(3));
}

#[test]
fn players_cannot_place_what_would_trap_them_but_can_what_misses_them() {
    let mut session = in_game_session();
    session.inventory = Inventory::with_hotbar(&["minecraft:stone", "minecraft:torch"]);
    // The player stands at the spawn, feet in the block above this grass.
    let grass = BlockPos { x: 0, y: -61, z: 0 };
    let reply = session.handle(&place(0, grass, 1)).unwrap();
    assert!(reply.events.is_empty(), "stone would trap them");
    let reply = session
        .handle(&use_on_block(&session, 1, grass, 1, 1.0))
        .unwrap();
    assert!(
        matches!(reply.events[0], SessionEvent::PlacedBlock { .. }),
        "a torch at their feet is fine: {:?}",
        reply.events
    );
}

#[test]
fn clicking_scaffolding_with_scaffolding_climbs_the_column() {
    let mut session = in_game_session();
    session.inventory = Inventory::with_hotbar(&["minecraft:scaffolding"]);
    let scaffolding = crate::blocks::palette()
        .upgrade(&BlockState::new("minecraft:scaffolding"))
        .unwrap();
    let column = [
        BlockPos { x: 2, y: -60, z: 0 },
        BlockPos { x: 2, y: -59, z: 0 },
    ];
    for pos in column {
        session.world.set_block(pos, scaffolding.network_id());
    }
    // Clicking the ground where the column stands puts it on top, and the
    // client hears what is really where it clicked.
    let ground = BlockPos { x: 2, y: -61, z: 0 };
    let reply = session
        .handle(&use_on_block(&session, 0, ground, 1, 1.0))
        .unwrap();
    let Some(SessionEvent::PlacedBlock { pos, block, .. }) = reply.events.first() else {
        panic!("expected scaffolding, got {:?}", reply.events);
    };
    assert_eq!(*pos, BlockPos { x: 2, y: -58, z: 0 });
    let placed = session.world.state_of(*block).unwrap();
    assert_eq!(placed.name, "minecraft:scaffolding");
    assert_eq!(crate::support::int(placed, "stability"), Some(0));
    assert!(!reply.packets.is_empty());

    // Its top stacks too; a side reaches out of that side.
    let reply = session
        .handle(&use_on_block(&session, 0, column[1], 1, 1.0))
        .unwrap();
    assert!(matches!(
        reply.events.first(),
        Some(SessionEvent::PlacedBlock { pos, .. }) if *pos == BlockPos { x: 2, y: -58, z: 0 }
    ));
    let reply = session
        .handle(&use_on_block(&session, 0, column[0], 5, 0.5))
        .unwrap();
    assert!(matches!(
        reply.events.first(),
        Some(SessionEvent::PlacedBlock { pos, .. }) if *pos == BlockPos { x: 3, y: -60, z: 0 }
    ));
}
