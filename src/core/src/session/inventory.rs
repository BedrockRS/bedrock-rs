//! The player's inventory: the inventory screen, item stack requests, what
//! they hold, and dropping and picking up items.

use bedrockrs_protocol::packet::Encode;
use bedrockrs_protocol::packets::{
    ContainerClose, ContainerOpen, INVENTORY_WINDOW, Interact, InventoryAction, ItemInstance,
    ItemStackRequest, ItemStackResponse, MobEquipment, NO_WINDOW, OWN_INVENTORY_WINDOW,
    StackResponse, action_source, interact_action,
};
use bedrockrs_protocol::types::BlockPos;

use crate::entities::ItemEntities;
use crate::inventory::HOTBAR_SLOTS;
use crate::items::{equip_sound, items};
use crate::players::STANDING_HEIGHT;

use super::{Reply, Session, SessionEvent, Stage};

impl Session {
    /// Opens the player's inventory screen when they ask. Other interactions
    /// (hovering over entities, leaving vehicles) need nothing yet.
    pub(super) fn interact(&mut self, interact: Interact) -> Reply {
        if interact.action != interact_action::OPEN_INVENTORY || self.inventory_open {
            return Reply::default();
        }
        self.inventory_open = true;
        let feet = BlockPos::containing(self.movement.feet());
        Reply::send(vec![ContainerOpen::own_inventory(feet).encode()])
    }

    /// The player closed a window: confirmed, as the client waits for it.
    /// Closing the inventory puts what is on the cursor and in the crafting
    /// grid back into it.
    pub(super) fn container_close(&mut self, close: ContainerClose) -> Reply {
        match close.window_id {
            OWN_INVENTORY_WINDOW => {
                self.inventory_open = false;
                let mut reply = Reply::send(vec![
                    ContainerClose {
                        window_id: OWN_INVENTORY_WINDOW,
                        container_type: 0,
                        server_side: false,
                    }
                    .encode(),
                ]);
                if self.inventory.return_screen_items() {
                    // The screen's slots too, or the client keeps showing the
                    // items in them.
                    reply.packets.extend(self.inventory_sync());
                    reply
                        .events
                        .push(SessionEvent::InventoryChanged(self.inventory.saved()));
                    // With nowhere to go, it was thrown out.
                    reply.events.extend(self.dropped_event());
                    reply.events.extend(self.held_event());
                }
                reply
            }
            // Sent when the inventory and chat open together; nothing to confirm.
            NO_WINDOW => {
                self.inventory_open = false;
                Reply::default()
            }
            other => Reply::send(vec![
                ContainerClose {
                    window_id: other,
                    container_type: close.container_type,
                    server_side: false,
                }
                .encode(),
            ]),
        }
    }

    /// The player chose another hotbar slot.
    pub(super) fn equipment(&mut self, equipment: MobEquipment) -> Reply {
        if equipment.window_id != INVENTORY_WINDOW as u8
            || usize::from(equipment.hotbar_slot) >= HOTBAR_SLOTS
        {
            return Reply::default();
        }
        self.held_slot = equipment.hotbar_slot;
        Reply {
            events: self.held_event().into_iter().collect(),
            ..Reply::default()
        }
    }

    /// Using an item in the air: armour is put on, swapping with what was
    /// worn, as Dragonfly does. The client already shows it on, so it is
    /// only confirmed.
    pub(super) fn use_in_air(&mut self) -> Reply {
        let armor_before = self.inventory.armor();
        if self.is_dead() || self.inventory.equip_held(self.held_slot.into()).is_none() {
            return Reply::default();
        }
        let mut reply = Reply::send(self.inventory_sync());
        reply
            .events
            .push(SessionEvent::InventoryChanged(self.inventory.saved()));
        reply.events.extend(self.armor_events(armor_before));
        reply.events.extend(self.held_event());
        reply
    }

    /// What to tell others now that the player wears something other than
    /// `before`: what they wear, and the sound of what was put on.
    pub(super) fn armor_events(&self, before: [ItemInstance; 4]) -> Vec<SessionEvent> {
        let after = self.inventory.armor();
        if after == before {
            return Vec::new();
        }
        let put_on = after
            .iter()
            .zip(&before)
            .find(|(now, was)| !now.is_empty() && now != was)
            .and_then(|(now, _)| items().get(now.network_id));
        std::iter::once(SessionEvent::ArmorChanged(after))
            .chain(put_on.map(|item| SessionEvent::Equipped(equip_sound(&item.name))))
            .collect()
    }

    /// What the player now holds, if others have not been shown it yet.
    pub(super) fn held_event(&mut self) -> Option<SessionEvent> {
        let held = self
            .inventory
            .hotbar(self.held_slot.into())
            .map_or(ItemInstance::EMPTY, |stack| stack.instance());
        (held != self.shown_held).then(|| {
            self.shown_held = held;
            SessionEvent::Holding {
                item: held,
                slot: self.held_slot,
            }
        })
    }

    /// A normal inventory transaction: with server-authoritative inventories,
    /// only throwing from the HUD (Q) sends one. It pairs a world action
    /// carrying the thrown count with the inventory slot it came from. The
    /// client is always shown its inventory afterwards, as Dragonfly does.
    pub(super) fn hud_drop(&mut self, actions: &[InventoryAction]) -> Reply {
        let thrown = actions
            .iter()
            .find(|action| action.source == action_source::WORLD && action.slot == 0)
            .map(|action| action.new_item.count);
        let from = actions.iter().find(|action| {
            action.source == action_source::CONTAINER
                && action.window_id == Some(INVENTORY_WINDOW as i8)
        });
        let dropped = match (thrown, from) {
            (Some(count), Some(from)) if actions.len() == 2 => self.inventory.throw_from_slot(
                from.slot as usize,
                from.old_item.network_id,
                u8::try_from(count).unwrap_or(u8::MAX),
            ),
            _ => None,
        };
        let mut reply = Reply::send(self.inventory_sync());
        if let Some(stack) = dropped {
            reply
                .events
                .push(SessionEvent::InventoryChanged(self.inventory.saved()));
            reply.events.push(SessionEvent::Dropped {
                stacks: vec![stack],
                feet: self.movement.feet(),
                pitch: self.movement.pitch,
                yaw: self.movement.yaw,
            });
            reply.events.extend(self.held_event());
        } else {
            tracing::debug!(player = %self.player, ?actions, "ignoring an inventory transaction that is not a valid drop");
        }
        reply
    }

    /// What the inventory threw out, for the world to take.
    pub(super) fn dropped_event(&mut self) -> Option<SessionEvent> {
        let stacks = self.inventory.take_dropped();
        (!stacks.is_empty()).then(|| SessionEvent::Dropped {
            stacks,
            feet: self.movement.feet(),
            pitch: self.movement.pitch,
            yaw: self.movement.yaw,
        })
    }

    /// Picks up the items within the player's reach, as many as fit. Called
    /// every tick while the player is in the world.
    pub fn pick_up(&mut self, items: &ItemEntities) -> Reply {
        if self.stage != Stage::InGame || !self.game_mode.is_present() {
            return Reply::default();
        }
        let feet = self.movement.feet();
        let inventory = &mut self.inventory;
        let picked = items.pick_up(feet, STANDING_HEIGHT, |stack| inventory.add(stack));
        if picked.is_empty() {
            return Reply::default();
        }
        let mut events = vec![
            SessionEvent::InventoryChanged(self.inventory.saved()),
            SessionEvent::PickedUp(picked),
        ];
        events.extend(self.held_event());
        Reply {
            packets: self.inventory_sync(),
            events,
            ..Reply::default()
        }
    }

    /// Everything the player carries, cursor included, as the server has it:
    /// what puts a client that went out of step back in line.
    pub(super) fn inventory_sync(&self) -> Vec<Vec<u8>> {
        let mut packets: Vec<Vec<u8>> = self
            .inventory
            .content()
            .iter()
            .map(Encode::encode)
            .collect();
        packets.extend(self.inventory.screen_slots().iter().map(Encode::encode));
        packets
    }

    /// Applies each request to the inventory and answers them all. Accepted
    /// changes are passed on, so saves include them.
    pub(super) fn item_stack_request(&mut self, packet: ItemStackRequest) -> Reply {
        let armor_before = self.inventory.armor();
        let responses: Vec<StackResponse> = packet
            .requests
            .iter()
            .map(|request| self.inventory.handle(request, self.game_mode.is_creative()))
            .collect();
        let changed = responses
            .iter()
            .any(|response| !response.containers.is_empty());
        let rejected = responses
            .iter()
            .any(|response| response.status != StackResponse::OK);
        let mut packets = vec![ItemStackResponse { responses }.encode()];
        let dropped = self.dropped_event();
        // The client undoes a rejected request by itself, but not always
        // cleanly (a rejected drop left an unusable slot), so it is also
        // shown what the server really has.
        if rejected {
            packets.extend(self.inventory_sync());
        }
        let mut events = Vec::new();
        if changed {
            events.push(SessionEvent::InventoryChanged(self.inventory.saved()));
        }
        events.extend(dropped);
        events.extend(self.held_event());
        events.extend(self.armor_events(armor_before));
        Reply {
            packets,
            events,
            ..Reply::default()
        }
    }
}
