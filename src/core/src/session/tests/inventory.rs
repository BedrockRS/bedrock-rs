use bedrockrs_protocol::packets::{
    FullContainerName, LegacyRequest, StackAction, StackRequest, StackSlot, container,
};

use super::*;

#[test]
fn a_slot_chosen_while_loading_goes_in_with_the_player() {
    let mut session = spawning_session();
    session.inventory = Inventory::with_hotbar(&TEST_KIT);
    let request = RequestChunkRadius {
        radius: 4,
        max_radius: 4,
    };
    session.handle(&request.encode()).unwrap();
    // The client picks slot 2 before it has finished loading.
    let switch = MobEquipment {
        entity_runtime_id: PLAYER_ENTITY_ID,
        item: ItemInstance::EMPTY,
        inventory_slot: 2,
        hotbar_slot: 2,
        window_id: 0,
    };
    session.handle(&switch.encode()).unwrap();
    let initialized = SetLocalPlayerAsInitialized {
        entity_runtime_id: PLAYER_ENTITY_ID,
    };
    let reply = session.handle(&initialized.encode()).unwrap();
    let dirt = session.inventory().hotbar(2).unwrap().instance();
    assert!(matches!(
        &reply.events[0],
        SessionEvent::Joined { held, .. } if *held == (dirt, 2)
    ));
}

#[test]
fn switching_slots_shows_others_the_new_item() {
    let mut session = in_game_session();
    let switch = |slot: u8| MobEquipment {
        entity_runtime_id: PLAYER_ENTITY_ID,
        item: ItemInstance::EMPTY,
        inventory_slot: slot,
        hotbar_slot: slot,
        window_id: 0,
    };
    let reply = session.handle(&switch(2).encode()).unwrap();
    let dirt = session.inventory().hotbar(2).unwrap().instance();
    assert_eq!(
        reply.events,
        [SessionEvent::Holding {
            item: dirt,
            slot: 2
        }]
    );
    // The same slot again changes nothing; a slot off the hotbar is ignored.
    assert!(
        session
            .handle(&switch(2).encode())
            .unwrap()
            .events
            .is_empty()
    );
    assert!(
        session
            .handle(&switch(20).encode())
            .unwrap()
            .events
            .is_empty()
    );
}

#[test]
fn q_on_the_hud_throws_the_held_item() {
    use bedrockrs_protocol::packets::InventoryAction;
    let mut session = in_game_session();
    let stone = *session.inventory().hotbar(0).unwrap();
    let throw = |count: u16, slot: u32| {
        InventoryTransaction::from(TransactionData::Normal(vec![
            InventoryAction {
                source: action_source::WORLD,
                window_id: None,
                source_flags: Some(0),
                slot: 0,
                old_item: ItemInstance::EMPTY,
                new_item: ItemInstance {
                    count,
                    ..stone.instance()
                },
            },
            InventoryAction {
                source: action_source::CONTAINER,
                window_id: Some(0),
                source_flags: None,
                slot,
                old_item: stone.instance(),
                new_item: ItemInstance {
                    count: 64 - count,
                    ..stone.instance()
                },
            },
        ]))
        .encode()
    };
    let reply = session.handle(&throw(1, 0)).unwrap();
    // The client is always shown its inventory, as Dragonfly does.
    assert_eq!(ids(&reply)[0], id::INVENTORY_CONTENT);
    assert!(matches!(
        &reply.events[..],
        [
            SessionEvent::InventoryChanged(_),
            SessionEvent::Dropped { stacks, .. },
            SessionEvent::Holding { .. },
        ] if stacks[0].count == 1
    ));
    assert_eq!(session.inventory().hotbar(0).unwrap().count, 63);

    // Claiming the stone is in another slot throws nothing.
    let reply = session.handle(&throw(1, 1)).unwrap();
    assert!(reply.events.is_empty());
    assert_eq!(ids(&reply)[0], id::INVENTORY_CONTENT);
}

#[test]
fn the_inventory_screen_opens_when_asked_and_closes_confirmed() {
    let mut session = in_game_session();
    let open = Interact {
        action: interact_action::OPEN_INVENTORY,
        target_entity_runtime_id: PLAYER_ENTITY_ID,
        position: None,
    };
    let reply = session.handle(&open.encode()).unwrap();
    assert_eq!(ids(&reply), [id::CONTAINER_OPEN]);
    // Window 0 of type inventory, at the block the player stands in.
    assert_eq!(
        reply.packets[0],
        ContainerOpen::own_inventory(BlockPos { x: 0, y: -60, z: 0 }).encode()
    );
    // A second request while open would crash the client if answered.
    assert!(session.handle(&open.encode()).unwrap().packets.is_empty());

    // Closing with an item on the cursor puts it back.
    session
        .inventory
        .cursor_for_test(session.inventory.hotbar(0).copied());
    let close = ContainerClose {
        window_id: OWN_INVENTORY_WINDOW,
        container_type: 0,
        server_side: false,
    };
    let reply = session.handle(&close.encode()).unwrap();
    assert_eq!(
        ids(&reply),
        [
            id::CONTAINER_CLOSE,
            id::INVENTORY_CONTENT,
            id::INVENTORY_CONTENT,
            id::INVENTORY_CONTENT,
            // The cursor and the four slots of the crafting grid.
            id::INVENTORY_SLOT,
            id::INVENTORY_SLOT,
            id::INVENTORY_SLOT,
            id::INVENTORY_SLOT,
            id::INVENTORY_SLOT
        ]
    );
    assert!(matches!(
        reply.events[..],
        [SessionEvent::InventoryChanged(_)]
    ));
    // It can open again.
    assert_eq!(
        ids(&session.handle(&open.encode()).unwrap()),
        [id::CONTAINER_OPEN]
    );
}

#[test]
fn item_stack_requests_are_answered_and_saved() {
    let mut session = in_game_session();
    let dirt = *session.inventory().hotbar(2).unwrap();
    let reply = session
        .handle(&swap_request(-1, 0, 2, &session).encode())
        .unwrap();
    assert_eq!(ids(&reply), [id::ITEM_STACK_RESPONSE]);
    assert_eq!(session.inventory().hotbar(0), Some(&dirt));
    // Slot 0, which the player holds, now has dirt: others see it.
    let [
        SessionEvent::InventoryChanged(saved),
        SessionEvent::Holding { slot: 0, .. },
    ] = &reply.events[..]
    else {
        panic!("expected an inventory change, got {:?}", reply.events);
    };
    assert_eq!(saved.main[0].item, "minecraft:dirt");
    let (_, player) = session.saved_player().unwrap();
    assert_eq!(player.inventory.as_ref(), Some(saved));

    // A stale request is rejected and changes nothing.
    let mut stale = swap_request(-3, 0, 2, &session);
    if let bedrockrs_protocol::packets::StackAction::Swap { source, .. } =
        &mut stale.requests[0].actions[0]
    {
        source.stack_id += 1000;
    }
    let reply = session.handle(&stale.encode()).unwrap();
    assert_eq!(
        ids(&reply),
        [
            id::ITEM_STACK_RESPONSE,
            id::INVENTORY_CONTENT,
            id::INVENTORY_CONTENT,
            id::INVENTORY_CONTENT,
            // The cursor and the four slots of the crafting grid.
            id::INVENTORY_SLOT,
            id::INVENTORY_SLOT,
            id::INVENTORY_SLOT,
            id::INVENTORY_SLOT,
            id::INVENTORY_SLOT
        ]
    );
    assert!(reply.events.is_empty());
    assert_eq!(session.inventory().hotbar(0), Some(&dirt));
}

#[test]
fn drops_leave_the_inventory_and_pickups_come_back() {
    use bedrockrs_protocol::packets::{
        FullContainerName, StackAction, StackRequest, StackSlot, container,
    };
    let mut session = in_game_session();
    let stone = *session.inventory().hotbar(0).unwrap();
    let drop = ItemStackRequest {
        requests: vec![StackRequest {
            id: -1,
            actions: vec![StackAction::Drop {
                count: 5,
                source: StackSlot {
                    container: FullContainerName::new(container::HOTBAR),
                    slot: 0,
                    stack_id: stone.id,
                },
                randomly: false,
            }],
            filter_strings: Vec::new(),
            filter_cause: 0,
        }],
    };
    let reply = session.handle(&drop.encode()).unwrap();
    let [
        SessionEvent::InventoryChanged(_),
        SessionEvent::Dropped { stacks, feet, .. },
        SessionEvent::Holding { .. },
    ] = &reply.events[..]
    else {
        panic!("expected a drop, got {:?}", reply.events);
    };
    assert_eq!(
        stacks,
        &[ItemStack {
            count: 5,
            ..stone.stack()
        }]
    );
    assert_eq!(*feet, session.movement.feet());
    assert_eq!(session.inventory().hotbar(0).unwrap().count, 59);

    // The five land at the player's feet and come straight back.
    let items = ItemEntities::new();
    items.spawn(500, stacks[0], *feet, Vec3::default(), 0);
    let reply = session.pick_up(&items);
    assert!(
        reply
            .events
            .contains(&SessionEvent::PickedUp(vec![PickedUp {
                entity_id: 500,
                taken: stacks[0],
            }]))
    );
    assert_eq!(session.inventory().hotbar(0).unwrap().count, 64);
    assert_eq!(items.count(), 0);
    assert!(session.pick_up(&items).events.is_empty());
}

/// Using the held item in the air, as a client putting on armour does: it
/// moves the item itself and names the two slots it changed.
fn use_in_air(session: &Session) -> Vec<u8> {
    let slot = i32::from(session.held_slot);
    let data = TransactionData::UseItem(UseItem {
        action: use_item_action::CLICK_AIR,
        trigger: 1,
        block_position: BlockPos::default(),
        face: 255,
        hotbar_slot: slot,
        held_item: session.inventory().hotbar(slot).unwrap().instance(),
        player_position: Vec3::default(),
        clicked_position: Vec3::default(),
        block_runtime_id: 0,
        client_prediction: 1,
    });
    InventoryTransaction {
        legacy_request: Some(LegacyRequest {
            id: -5,
            slots: vec![
                (container::ARMOR, vec![1]),
                (container::INVENTORY, vec![slot as u8]),
            ],
        }),
        data,
    }
    .encode()
}

#[test]
fn using_armour_puts_it_on_with_its_sound() {
    let mut session = in_game_session();
    session.inventory = Inventory::with_hotbar(&["minecraft:iron_chestplate"]);
    let reply = session.handle(&use_in_air(&session)).unwrap();
    assert!(session.inventory().hotbar(0).is_none(), "it left the hand");
    let worn = session.inventory().armor();
    let chestplate = items().by_name("minecraft:iron_chestplate").unwrap();
    assert_eq!(worn[1].network_id, chestplate.network_id);
    assert!(reply.events.contains(&SessionEvent::ArmorChanged(worn)));
    assert!(
        reply
            .events
            .contains(&SessionEvent::Equipped("armor.equip_iron"))
    );
    // The client hears what the slots it changed hold, before anything else.
    assert_eq!(ids(&reply)[0], id::ITEM_STACK_RESPONSE);
    assert!(ids(&reply).contains(&id::INVENTORY_CONTENT));

    // So it can take the chestplate off again, naming it, as it names what
    // any request left behind, by the request's ID.
    let take_off = ItemStackRequest {
        requests: vec![StackRequest {
            id: -7,
            actions: vec![StackAction::Take {
                count: 1,
                source: StackSlot {
                    container: FullContainerName::new(container::ARMOR),
                    slot: 1,
                    stack_id: -5,
                },
                destination: StackSlot {
                    container: FullContainerName::new(container::CURSOR),
                    slot: 0,
                    stack_id: 0,
                },
            }],
            filter_strings: Vec::new(),
            filter_cause: 0,
        }],
    };
    let reply = session.handle(&take_off.encode()).unwrap();
    assert_eq!(ids(&reply), [id::ITEM_STACK_RESPONSE], "not rejected");
    assert!(session.inventory().armor()[1].is_empty(), "taken off");

    // Using something else in the air changes nothing, but the client still
    // hears about the slots it named.
    session.inventory = Inventory::with_hotbar(&TEST_KIT);
    assert_eq!(
        ids(&session.handle(&use_in_air(&session)).unwrap()),
        [id::ITEM_STACK_RESPONSE]
    );
}
