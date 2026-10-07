//! A player's inventory: 36 slots (the hotbar is slots 0 to 8), four armour
//! slots, the offhand and the cursor, which holds what the player is moving
//! in the inventory screen.
//!
//! The server owns the inventory. The client moves items on its side and
//! asks the server to confirm with item stack requests; each request is
//! applied whole or not at all, and the response tells the client what the
//! changed slots hold, or to undo the request.
//!
//! The client does not wait for answers: it names a slot that an earlier,
//! unanswered request changed by that request's (negative) ID. So the stack
//! IDs each recent request left behind are remembered, as Dragonfly does.

use std::collections::VecDeque;

use bedrockrs_protocol::packets::{
    ContainerResponse, FullContainerName, INVENTORY_WINDOW, InventoryContent, InventorySlot,
    ItemInstance, SlotResponse, StackAction, StackRequest, StackResponse, StackSlot, UI_WINDOW,
    container,
};

use crate::entities::ItemStack;
use crate::items::{SHIELD, items};
use crate::storage::{SavedInventory, SavedStack};

/// Slots of the main inventory, hotbar first.
pub const MAIN_SLOTS: usize = 36;
/// Slots of the hotbar, at the start of the main inventory.
pub const HOTBAR_SLOTS: usize = 9;
const ARMOR_SLOTS: usize = 4;
/// Window IDs of the offhand and armour, for InventoryContent.
const OFFHAND_WINDOW: u32 = 119;
const ARMOR_WINDOW: u32 = 120;
/// The slot of the created output container that creative items appear in.
const CREATED_OUTPUT_SLOT: u8 = 50;
/// Requests whose resulting stack IDs are remembered for the client to refer
/// back to. The client rarely has more than a few in flight.
const RECENT_REQUESTS: usize = 64;

/// A hotbar of building blocks for tests.
#[cfg(test)]
pub(crate) const TEST_KIT: [&str; HOTBAR_SLOTS] = [
    "minecraft:stone",
    "minecraft:grass_block",
    "minecraft:dirt",
    "minecraft:cobblestone",
    "minecraft:oak_planks",
    "minecraft:sand",
    "minecraft:glass",
    "minecraft:white_wool",
    "minecraft:bookshelf",
];

/// Some of one item in one slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stack {
    /// The item's network ID.
    pub item: i16,
    pub count: u8,
    pub metadata: u32,
    /// The server's ID for this stack, which requests refer to it by. Unique
    /// per player; a stack that splits gets a new one for the part that moves.
    pub id: i32,
}

impl Stack {
    /// The items in the stack, without its ID.
    pub fn stack(&self) -> ItemStack {
        ItemStack {
            item: self.item,
            count: self.count,
            metadata: self.metadata,
        }
    }

    /// Whether `other` is the same item and could join this stack.
    fn stacks_with(&self, other: &Stack) -> bool {
        self.item == other.item && self.metadata == other.metadata
    }

    fn max(&self) -> u8 {
        items().get(self.item).map_or(64, |item| item.max_stack)
    }

    /// The stack as InventoryContent sends it.
    pub fn instance(&self) -> ItemInstance {
        let item = items().get(self.item);
        ItemInstance {
            network_id: self.item,
            count: self.count.into(),
            metadata: self.metadata,
            stack_network_id: Some(self.id),
            block_runtime_id: item.and_then(|item| item.block_network_id).unwrap_or(0),
            shield: item.is_some_and(|item| item.name == SHIELD),
        }
    }
}

/// Where in the inventory a request points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    Main(usize),
    Armor(usize),
    Offhand,
    Cursor,
    CreatedOutput,
}

/// Why a request was rejected; logged, while the client just undoes it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("no slot {slot} in container {container}")]
    NoSuchSlot { container: u8, slot: u8 },
    #[error("slot {slot} of container {container} is not stack {expected} but {actual}")]
    StackMismatch {
        container: u8,
        slot: u8,
        expected: i32,
        actual: i32,
    },
    #[error("cannot move {count} items from a stack of {available}")]
    NotEnough { count: u8, available: u8 },
    #[error("the items do not stack")]
    DifferentItems,
    #[error("a stack of {0} is too big")]
    TooMany(u16),
    #[error("unknown creative item {0}")]
    UnknownCreativeItem(u32),
    #[error("only creative players can do that")]
    NotCreative,
    #[error("unsupported action: {0}")]
    Unsupported(&'static str),
}

/// A player's inventory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    main: [Option<Stack>; MAIN_SLOTS],
    armor: [Option<Stack>; ARMOR_SLOTS],
    offhand: Option<Stack>,
    cursor: Option<Stack>,
    /// What a creative pick put in the created output, for the rest of the request.
    created: Option<Stack>,
    last_stack_id: i32,
    /// The stack ID each slot the current request changed holds, in order.
    changes: Vec<(Place, i32)>,
    /// The same for recent accepted requests, by request ID, oldest first.
    recent: VecDeque<(i32, Vec<(Place, i32)>)>,
    /// Items thrown out of the inventory, for the world to take.
    dropped: Vec<ItemStack>,
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            main: [None; MAIN_SLOTS],
            armor: [None; ARMOR_SLOTS],
            offhand: None,
            cursor: None,
            created: None,
            last_stack_id: 0,
            changes: Vec::new(),
            recent: VecDeque::new(),
            dropped: Vec::new(),
        }
    }
}

impl Inventory {
    /// An inventory with a full stack of each named item on the hotbar, for tests.
    #[cfg(test)]
    pub(crate) fn with_hotbar(names: &[&str]) -> Self {
        let mut inventory = Self::default();
        for (slot, name) in names.iter().enumerate() {
            let item = items().by_name(name).expect("a known item");
            inventory.main[slot] = Some(inventory.new_stack(item.network_id, item.max_stack, 0));
        }
        inventory
    }

    /// Puts `stack` on the cursor, for tests.
    #[cfg(test)]
    pub(crate) fn cursor_for_test(&mut self, stack: Option<Stack>) {
        self.cursor = stack;
    }

    /// Puts what is on the cursor back into the first free slot, as the
    /// inventory screen closes, or throws it out if there is none. Returns
    /// whether anything moved.
    pub fn return_cursor(&mut self) -> bool {
        let Some(cursor) = self.cursor.take() else {
            return false;
        };
        match self.main.iter_mut().find(|slot| slot.is_none()) {
            Some(free) => *free = Some(cursor),
            None => self.dropped.push(cursor.stack()),
        }
        true
    }

    /// Throws `count` of the item in main slot `slot` out, as pressing Q on
    /// the HUD does, if the slot holds at least that many of `item`. Returns
    /// what was thrown.
    pub fn throw_from_slot(&mut self, slot: usize, item: i16, count: u8) -> Option<ItemStack> {
        let stack = self.main.get_mut(slot)?.as_mut()?;
        if stack.item != item || count == 0 || count > stack.count {
            return None;
        }
        let thrown = ItemStack {
            count,
            ..stack.stack()
        };
        stack.count -= count;
        if stack.count == 0 {
            self.main[slot] = None;
        }
        Some(thrown)
    }

    /// The items thrown out since the last call, for the world to take.
    pub fn take_dropped(&mut self) -> Vec<ItemStack> {
        std::mem::take(&mut self.dropped)
    }

    /// Adds as much of `stack` as fits: onto stacks of the same item first,
    /// then into empty slots, hotbar first. Returns how many were added.
    pub fn add(&mut self, stack: ItemStack) -> u8 {
        let max = items().get(stack.item).map_or(64, |item| item.max_stack);
        let mut left = stack.count;
        for slot in self.main.iter_mut().flatten() {
            if left == 0 {
                break;
            }
            if slot.item == stack.item && slot.metadata == stack.metadata && slot.count < max {
                let moved = left.min(max - slot.count);
                slot.count += moved;
                left -= moved;
            }
        }
        for index in 0..MAIN_SLOTS {
            if left == 0 {
                break;
            }
            if self.main[index].is_none() {
                let count = left.min(max);
                self.main[index] = Some(self.new_stack(stack.item, count, stack.metadata));
                left -= count;
            }
        }
        stack.count - left
    }

    /// The inventory as saved. Items the registry no longer knows are left out.
    pub fn from_saved(saved: &SavedInventory) -> Self {
        let mut inventory = Self::default();
        let restore = |stacks: &[SavedStack], slots: &dyn Fn(usize) -> Option<usize>| {
            let mut placed = Vec::new();
            for stack in stacks {
                let Some(index) = slots(usize::from(stack.slot)) else {
                    tracing::warn!(slot = stack.slot, item = %stack.item, "ignoring an item saved in a slot that does not exist");
                    continue;
                };
                let Some(item) = items().by_name(&stack.item) else {
                    tracing::warn!(item = %stack.item, "ignoring a saved item the server does not know");
                    continue;
                };
                let count = stack.count.clamp(1, item.max_stack);
                placed.push((index, (item.network_id, count, stack.meta)));
            }
            placed
        };
        let main = restore(&saved.main, &|slot| (slot < MAIN_SLOTS).then_some(slot));
        let armor = restore(&saved.armor, &|slot| (slot < ARMOR_SLOTS).then_some(slot));
        let offhand = restore(&saved.offhand, &|slot| (slot == 0).then_some(0));
        for (slot, (item, count, meta)) in main {
            inventory.main[slot] = Some(inventory.new_stack(item, count, meta));
        }
        for (slot, (item, count, meta)) in armor {
            inventory.armor[slot] = Some(inventory.new_stack(item, count, meta));
        }
        for (_, (item, count, meta)) in offhand {
            inventory.offhand = Some(inventory.new_stack(item, count, meta));
        }
        inventory
    }

    /// The inventory to save. What is on the cursor goes back into the
    /// inventory if there is room, as the client does when the screen closes.
    pub fn saved(&self) -> SavedInventory {
        let mut main = self.main;
        if let Some(cursor) = self.cursor
            && let Some(free) = main.iter_mut().find(|slot| slot.is_none())
        {
            *free = Some(cursor);
        }
        let save = |slots: &[Option<Stack>]| {
            slots
                .iter()
                .enumerate()
                .filter_map(|(slot, stack)| {
                    let stack = (*stack)?;
                    Some(SavedStack {
                        slot: slot as u8,
                        item: items().get(stack.item)?.name.clone(),
                        count: stack.count,
                        meta: stack.metadata,
                    })
                })
                .collect()
        };
        SavedInventory {
            main: save(&main),
            armor: save(&self.armor),
            offhand: save(&[self.offhand]),
        }
    }

    /// The packets that show the client this inventory: the main inventory,
    /// the offhand and the armour.
    pub fn content(&self) -> Vec<InventoryContent> {
        let instances = |slots: &[Option<Stack>]| {
            slots
                .iter()
                .map(|slot| slot.map_or(ItemInstance::EMPTY, |stack| stack.instance()))
                .collect()
        };
        vec![
            InventoryContent {
                window_id: INVENTORY_WINDOW,
                content: instances(&self.main),
            },
            InventoryContent {
                window_id: OFFHAND_WINDOW,
                content: instances(&[self.offhand]),
            },
            InventoryContent {
                window_id: ARMOR_WINDOW,
                content: instances(&self.armor),
            },
        ]
    }

    /// Where the cursor's content goes: slot 0 of the UI window.
    pub fn cursor_slot(&self) -> InventorySlot {
        InventorySlot {
            window_id: UI_WINDOW,
            slot: 0,
            item: self
                .cursor
                .map_or(ItemInstance::EMPTY, |stack| stack.instance()),
        }
    }

    /// The stack in a hotbar slot.
    pub fn hotbar(&self, slot: i32) -> Option<&Stack> {
        let slot = usize::try_from(slot)
            .ok()
            .filter(|&slot| slot < HOTBAR_SLOTS)?;
        self.main[slot].as_ref()
    }

    /// Empties the whole inventory, cursor included, as dying does. Returns
    /// what it held, for the world to take.
    pub fn clear(&mut self) -> Vec<ItemStack> {
        self.main
            .iter_mut()
            .chain(self.armor.iter_mut())
            .chain([&mut self.offhand, &mut self.cursor])
            .filter_map(Option::take)
            .map(|stack| stack.stack())
            .collect()
    }

    /// Uses up one item from a hotbar slot, as placing a block outside
    /// creative mode does. Returns the slot as it is now, for the client.
    pub fn use_one(&mut self, slot: i32) -> Option<InventorySlot> {
        let index = usize::try_from(slot)
            .ok()
            .filter(|&slot| slot < HOTBAR_SLOTS)?;
        let stack = self.main[index].as_mut()?;
        stack.count -= 1;
        if stack.count == 0 {
            self.main[index] = None;
        }
        Some(InventorySlot {
            window_id: INVENTORY_WINDOW,
            slot: index as u32,
            item: self.main[index].map_or(ItemInstance::EMPTY, |stack| stack.instance()),
        })
    }

    /// Applies a request if it is valid, and says what changed. `creative`
    /// allows taking items from the creative inventory and destroying them.
    pub fn handle(&mut self, request: &StackRequest, creative: bool) -> StackResponse {
        let mut working = self.clone();
        working.changes.clear();
        let mut touched: Vec<(FullContainerName, u8)> = Vec::new();
        let result = request
            .actions
            .iter()
            .try_for_each(|action| working.apply(action, request.id, creative, &mut touched));
        // Whatever a creative pick left over is gone once the request ends.
        working.created = None;
        match result {
            Ok(()) => {
                *self = working;
                let changes = std::mem::take(&mut self.changes);
                self.recent.push_back((request.id, changes));
                if self.recent.len() > RECENT_REQUESTS {
                    self.recent.pop_front();
                }
                StackResponse {
                    status: StackResponse::OK,
                    request_id: request.id,
                    containers: self.describe(&touched),
                }
            }
            Err(err) => {
                tracing::debug!(request = request.id, %err, actions = ?request.actions, "rejected an item stack request");
                StackResponse::rejected(request.id)
            }
        }
    }

    fn apply(
        &mut self,
        action: &StackAction,
        request: i32,
        creative: bool,
        touched: &mut Vec<(FullContainerName, u8)>,
    ) -> Result<(), RequestError> {
        let mut touch = |slot: &StackSlot| {
            let key = (slot.container, slot.slot);
            if slot.container.container != container::CREATED_OUTPUT && !touched.contains(&key) {
                touched.push(key);
            }
        };
        match action {
            StackAction::Take {
                count,
                source,
                destination,
            }
            | StackAction::Place {
                count,
                source,
                destination,
            } => {
                self.transfer(*count, source, destination, request)?;
                touch(source);
                touch(destination);
            }
            StackAction::Swap {
                source,
                destination,
            } => {
                let from = self.checked(source, request)?;
                let to = self.checked(destination, request)?;
                if from == Place::CreatedOutput || to == Place::CreatedOutput {
                    return Err(RequestError::Unsupported(
                        "swapping with the created output",
                    ));
                }
                let (a, b) = (*self.slot(from), *self.slot(to));
                *self.slot(from) = b;
                *self.slot(to) = a;
                self.note(from);
                self.note(to);
                touch(source);
                touch(destination);
            }
            StackAction::Destroy { count, source } => {
                if !creative {
                    return Err(RequestError::NotCreative);
                }
                let from = self.checked(source, request)?;
                self.remove(from, *count)?;
                self.note(from);
                touch(source);
            }
            StackAction::CraftCreative { creative_item, .. } => {
                if !creative {
                    return Err(RequestError::NotCreative);
                }
                let entry = items()
                    .creative(*creative_item)
                    .ok_or(RequestError::UnknownCreativeItem(*creative_item))?;
                let max = items()
                    .get(entry.network_id)
                    .map_or(64, |item| item.max_stack);
                // Refers to the request until it lands in a real slot.
                self.created = Some(Stack {
                    item: entry.network_id,
                    count: max,
                    metadata: entry.metadata,
                    id: request,
                });
            }
            // Informational: what the client expects a craft to make, and a
            // tool's durability while mining, which creative tools keep.
            StackAction::CraftResults | StackAction::MineBlock { .. } => {}
            StackAction::Drop { count, source, .. } => {
                let from = self.checked(source, request)?;
                let dropping = self.slot(from).map(|stack| ItemStack {
                    count: *count,
                    ..stack.stack()
                });
                self.remove(from, *count)?;
                self.note(from);
                self.dropped.extend(dropping);
                touch(source);
            }
            StackAction::Consume { .. } | StackAction::Create { .. } => {
                return Err(RequestError::Unsupported("crafting"));
            }
            StackAction::Other(_) => return Err(RequestError::Unsupported("this action")),
        }
        Ok(())
    }

    /// Moves `count` items from one slot to another, onto nothing or onto a
    /// stack of the same item.
    fn transfer(
        &mut self,
        count: u8,
        source: &StackSlot,
        destination: &StackSlot,
        request: i32,
    ) -> Result<(), RequestError> {
        let from = self.checked(source, request)?;
        let to = self.checked(destination, request)?;
        if to == Place::CreatedOutput {
            return Err(RequestError::Unsupported("placing into the created output"));
        }
        let moving = *self.slot(from);
        let Some(moving) = moving.filter(|stack| count >= 1 && stack.count >= count) else {
            return Err(RequestError::NotEnough {
                count,
                available: moving.map_or(0, |stack| stack.count),
            });
        };
        let whole = count == moving.count && moving.id > 0;
        let arriving = match *self.slot(to) {
            None => {
                if count > moving.max() {
                    return Err(RequestError::TooMany(count.into()));
                }
                // A whole stack keeps its ID; a part, or a fresh creative
                // item, is a new stack.
                let id = if whole {
                    moving.id
                } else {
                    self.next_stack_id()
                };
                Stack {
                    count,
                    id,
                    ..moving
                }
            }
            Some(there) => {
                if !there.stacks_with(&moving) {
                    return Err(RequestError::DifferentItems);
                }
                let total = u16::from(there.count) + u16::from(count);
                if total > u16::from(there.max()) {
                    return Err(RequestError::TooMany(total));
                }
                Stack {
                    count: total as u8,
                    ..there
                }
            }
        };
        *self.slot(to) = Some(arriving);
        self.remove(from, count)?;
        self.note(from);
        self.note(to);
        Ok(())
    }

    /// Remembers the stack ID a changed slot now holds, for later actions and
    /// requests that refer to it by request ID.
    fn note(&mut self, place: Place) {
        if place != Place::CreatedOutput {
            let id = self.slot(place).map_or(0, |stack| stack.id);
            self.changes.push((place, id));
        }
    }

    fn remove(&mut self, place: Place, count: u8) -> Result<(), RequestError> {
        let slot = self.slot(place);
        match slot {
            Some(stack) if count >= 1 && stack.count >= count => {
                stack.count -= count;
                if stack.count == 0 {
                    *slot = None;
                }
                Ok(())
            }
            _ => Err(RequestError::NotEnough {
                count,
                available: slot.map_or(0, |stack| stack.count),
            }),
        }
    }

    /// Where `slot` points, once the stack the client believes is there is
    /// the one the server has: the same stack ID, 0 for an empty slot, or a
    /// request ID standing for the stack that request left in the slot.
    fn checked(&mut self, slot: &StackSlot, request: i32) -> Result<Place, RequestError> {
        let place = resolve(slot)?;
        let actual = self.slot(place).map_or(0, |stack| stack.id);
        let expected = match slot.stack_id {
            id if id >= 0 => Some(id),
            // A fresh creative item carries the request's ID itself.
            id if id == actual => Some(id),
            id if id == request => last_change(&self.changes, place),
            id => self
                .recent
                .iter()
                .rev()
                .find(|(request, _)| *request == id)
                .and_then(|(_, changes)| last_change(changes, place)),
        };
        if expected == Some(actual) {
            Ok(place)
        } else {
            Err(RequestError::StackMismatch {
                container: slot.container.container,
                slot: slot.slot,
                expected: slot.stack_id,
                actual,
            })
        }
    }

    fn slot(&mut self, place: Place) -> &mut Option<Stack> {
        match place {
            Place::Main(slot) => &mut self.main[slot],
            Place::Armor(slot) => &mut self.armor[slot],
            Place::Offhand => &mut self.offhand,
            Place::Cursor => &mut self.cursor,
            Place::CreatedOutput => &mut self.created,
        }
    }

    /// What the touched slots hold now, grouped by container as the client named them.
    fn describe(&mut self, touched: &[(FullContainerName, u8)]) -> Vec<ContainerResponse> {
        let mut containers: Vec<ContainerResponse> = Vec::new();
        for &(name, slot) in touched {
            let Ok(place) = resolve(&StackSlot {
                container: name,
                slot,
                stack_id: 0,
            }) else {
                continue;
            };
            let stack = *self.slot(place);
            let response = SlotResponse {
                slot,
                count: stack.map_or(0, |stack| stack.count),
                stack_id: stack.map_or(0, |stack| stack.id),
            };
            match containers.iter_mut().find(|entry| entry.container == name) {
                Some(entry) => entry.slots.push(response),
                None => containers.push(ContainerResponse {
                    container: name,
                    slots: vec![response],
                }),
            }
        }
        containers
    }

    fn new_stack(&mut self, item: i16, count: u8, metadata: u32) -> Stack {
        Stack {
            item,
            count,
            metadata,
            id: self.next_stack_id(),
        }
    }

    fn next_stack_id(&mut self) -> i32 {
        self.last_stack_id += 1;
        self.last_stack_id
    }
}

/// The stack ID a request's changes last left in `place`.
fn last_change(changes: &[(Place, i32)], place: Place) -> Option<i32> {
    changes
        .iter()
        .rev()
        .find(|(changed, _)| *changed == place)
        .map(|(_, id)| *id)
}

/// The inventory place a request's slot names.
fn resolve(slot: &StackSlot) -> Result<Place, RequestError> {
    let index = usize::from(slot.slot);
    let place = match slot.container.container {
        container::HOTBAR | container::INVENTORY | container::COMBINED_HOTBAR_AND_INVENTORY
            if index < MAIN_SLOTS =>
        {
            Some(Place::Main(index))
        }
        container::ARMOR if index < ARMOR_SLOTS => Some(Place::Armor(index)),
        // The offhand is slot 1 on the wire; accept 0 too.
        container::OFFHAND if index <= 1 => Some(Place::Offhand),
        container::CURSOR if index == 0 => Some(Place::Cursor),
        container::CREATED_OUTPUT if slot.slot == CREATED_OUTPUT_SLOT => Some(Place::CreatedOutput),
        _ => None,
    };
    place.ok_or(RequestError::NoSuchSlot {
        container: slot.container.container,
        slot: slot.slot,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(container: u8, slot: u8, stack_id: i32) -> StackSlot {
        StackSlot {
            container: FullContainerName::new(container),
            slot,
            stack_id,
        }
    }

    fn request(id: i32, actions: Vec<StackAction>) -> StackRequest {
        StackRequest {
            id,
            actions,
            filter_strings: Vec::new(),
            filter_cause: 0,
        }
    }

    fn slots(response: &StackResponse) -> Vec<(u8, u8, u8, i32)> {
        response
            .containers
            .iter()
            .flat_map(|container| {
                container.slots.iter().map(|slot| {
                    (
                        container.container.container,
                        slot.slot,
                        slot.count,
                        slot.stack_id,
                    )
                })
            })
            .collect()
    }

    #[test]
    fn new_players_start_empty() {
        let inventory = Inventory::default();
        assert!(
            inventory
                .content()
                .iter()
                .all(|window| { window.content.iter().all(ItemInstance::is_empty) })
        );
        assert_eq!(inventory.saved(), SavedInventory::default());
    }

    #[test]
    fn the_client_sees_every_slot() {
        let inventory = Inventory::with_hotbar(&TEST_KIT);
        let stone = items().by_name("minecraft:stone").unwrap().network_id;
        let first = inventory.hotbar(0).unwrap();
        assert_eq!((first.item, first.count), (stone, 64));
        assert!(inventory.hotbar(8).is_some());
        assert!(inventory.hotbar(9).is_none());
        let content = inventory.content();
        assert_eq!(content[0].content.len(), MAIN_SLOTS);
        assert_eq!(content[0].content[0].stack_network_id, Some(first.id));
        assert_eq!(content[1].content.len(), 1);
        assert_eq!(content[2].content.len(), ARMOR_SLOTS);
    }

    #[test]
    fn splitting_a_stack_onto_the_cursor_and_placing_it() {
        let mut inventory = Inventory::with_hotbar(&TEST_KIT);
        let stone = *inventory.hotbar(0).unwrap();

        // Pick up half the stone.
        let response = inventory.handle(
            &request(
                -1,
                vec![StackAction::Take {
                    count: 32,
                    source: at(container::HOTBAR, 0, stone.id),
                    destination: at(container::CURSOR, 0, 0),
                }],
            ),
            true,
        );
        assert_eq!(response.status, StackResponse::OK);
        let cursor = inventory.cursor.unwrap();
        assert_ne!(cursor.id, stone.id, "the part that moved is a new stack");
        assert_eq!(
            slots(&response),
            [
                (container::HOTBAR, 0, 32, stone.id),
                (container::CURSOR, 0, 32, cursor.id)
            ]
        );

        // Put 10 in an empty inventory slot, then the rest back on the stone.
        let response = inventory.handle(
            &request(
                -3,
                vec![
                    StackAction::Place {
                        count: 10,
                        source: at(container::CURSOR, 0, cursor.id),
                        destination: at(container::INVENTORY, 20, 0),
                    },
                    StackAction::Place {
                        count: 22,
                        source: at(container::CURSOR, 0, cursor.id),
                        destination: at(container::HOTBAR, 0, stone.id),
                    },
                ],
            ),
            true,
        );
        assert_eq!(response.status, StackResponse::OK);
        assert_eq!(inventory.cursor, None);
        assert_eq!(inventory.main[20].unwrap().count, 10);
        assert_eq!(inventory.main[0].unwrap().count, 54);
    }

    #[test]
    fn swapping_and_moving_whole_stacks_keeps_their_ids() {
        let mut inventory = Inventory::with_hotbar(&TEST_KIT);
        let (stone, dirt) = (*inventory.hotbar(0).unwrap(), *inventory.hotbar(2).unwrap());
        let response = inventory.handle(
            &request(
                -1,
                vec![StackAction::Swap {
                    source: at(container::HOTBAR, 0, stone.id),
                    destination: at(container::HOTBAR, 2, dirt.id),
                }],
            ),
            true,
        );
        assert_eq!(response.status, StackResponse::OK);
        assert_eq!(inventory.main[0], Some(dirt));
        assert_eq!(inventory.main[2], Some(stone));

        let response = inventory.handle(
            &request(
                -3,
                vec![StackAction::Take {
                    count: 64,
                    source: at(container::HOTBAR, 2, stone.id),
                    destination: at(container::CURSOR, 0, 0),
                }],
            ),
            true,
        );
        assert_eq!(response.status, StackResponse::OK);
        assert_eq!(inventory.cursor, Some(stone));
        assert_eq!(inventory.main[2], None);
    }

    #[test]
    fn bad_requests_change_nothing() {
        let mut inventory = Inventory::with_hotbar(&TEST_KIT);
        let before = inventory.clone();
        let (stone, dirt) = (*inventory.hotbar(0).unwrap(), *inventory.hotbar(2).unwrap());
        let rejected = [
            // The client's idea of the stack is out of date.
            vec![StackAction::Take {
                count: 1,
                source: at(container::HOTBAR, 0, stone.id + 100),
                destination: at(container::CURSOR, 0, 0),
            }],
            // More than there is.
            vec![StackAction::Take {
                count: 65,
                source: at(container::HOTBAR, 0, stone.id),
                destination: at(container::CURSOR, 0, 0),
            }],
            // Onto a different item.
            vec![StackAction::Place {
                count: 1,
                source: at(container::HOTBAR, 0, stone.id),
                destination: at(container::HOTBAR, 2, dirt.id),
            }],
            // A later action fails, so the earlier ones are undone too.
            vec![
                StackAction::Take {
                    count: 1,
                    source: at(container::HOTBAR, 0, stone.id),
                    destination: at(container::CURSOR, 0, 0),
                },
                StackAction::Place {
                    count: 1,
                    source: at(container::CURSOR, 0, -5),
                    destination: at(container::HOTBAR, 0, stone.id),
                },
                StackAction::Place {
                    count: 1,
                    source: at(container::HOTBAR, 1, 0),
                    destination: at(container::HOTBAR, 0, stone.id),
                },
            ],
            // A slot that does not exist, and dropping more than there is.
            vec![StackAction::Destroy {
                count: 1,
                source: at(container::HOTBAR, 40, 0),
            }],
            vec![StackAction::Drop {
                count: 65,
                source: at(container::HOTBAR, 0, stone.id),
                randomly: false,
            }],
        ];
        for actions in rejected {
            let response = inventory.handle(&request(-5, actions.clone()), true);
            assert_eq!(response, StackResponse::rejected(-5), "{actions:?}");
            assert_eq!(inventory, before);
        }
    }

    #[test]
    fn creative_players_take_any_item_and_destroy_items() {
        let mut inventory = Inventory::default();
        let sword_entry = (1..)
            .find(|&id| {
                items().creative(id).is_some_and(|entry| {
                    items().get(entry.network_id).unwrap().name == "minecraft:diamond_sword"
                })
            })
            .unwrap();
        let pick = |count| {
            request(
                -7,
                vec![
                    StackAction::CraftCreative {
                        creative_item: sword_entry,
                        crafts: 1,
                    },
                    StackAction::CraftResults,
                    StackAction::Take {
                        count,
                        source: at(container::CREATED_OUTPUT, CREATED_OUTPUT_SLOT, -7),
                        destination: at(container::CURSOR, 0, 0),
                    },
                ],
            )
        };
        assert_eq!(
            inventory.handle(&pick(1), false),
            StackResponse::rejected(-7)
        );
        assert_eq!(
            inventory.handle(&pick(2), true),
            StackResponse::rejected(-7),
            "swords do not stack"
        );

        let response = inventory.handle(&pick(1), true);
        assert_eq!(response.status, StackResponse::OK);
        let sword = inventory.cursor.unwrap();
        assert!(sword.id > 0);
        assert_eq!(slots(&response), [(container::CURSOR, 0, 1, sword.id)]);
        assert_eq!(inventory.created, None);

        let response = inventory.handle(
            &request(
                -9,
                vec![StackAction::Destroy {
                    count: 1,
                    source: at(container::CURSOR, 0, sword.id),
                }],
            ),
            true,
        );
        assert_eq!(slots(&response), [(container::CURSOR, 0, 0, 0)]);
        assert_eq!(inventory.cursor, None);
    }

    #[test]
    fn later_requests_refer_to_earlier_ones_by_request_id() {
        let mut inventory = Inventory::with_hotbar(&TEST_KIT);
        let stone = *inventory.hotbar(0).unwrap();
        let ok = |response: StackResponse| assert_eq!(response.status, StackResponse::OK);

        // Pick up the stone; the answer has not reached the client yet.
        ok(inventory.handle(
            &request(
                -1,
                vec![StackAction::Take {
                    count: 64,
                    source: at(container::HOTBAR, 0, stone.id),
                    destination: at(container::CURSOR, 0, 0),
                }],
            ),
            true,
        ));
        // Paint 16 into each of four slots, naming the cursor by request -1.
        let paint: Vec<_> = [20, 21, 22, 23]
            .into_iter()
            .map(|slot| StackAction::Place {
                count: 16,
                source: at(container::CURSOR, 0, -1),
                destination: at(container::INVENTORY, slot, 0),
            })
            .collect();
        ok(inventory.handle(&request(-3, paint), true));
        assert_eq!(inventory.cursor, None);
        assert!((20..24).all(|slot| inventory.main[slot].unwrap().count == 16));

        // Gather them back onto the cursor by double-clicking one, naming
        // the painted stacks by request -3.
        let gather: Vec<_> = [20, 21, 22, 23]
            .into_iter()
            .map(|slot| StackAction::Take {
                count: 16,
                source: at(container::INVENTORY, slot, -3),
                destination: at(container::CURSOR, 0, if slot == 20 { 0 } else { -5 }),
            })
            .collect();
        ok(inventory.handle(&request(-5, gather), true));
        assert_eq!(inventory.cursor.unwrap().count, 64);
        assert!((20..24).all(|slot| inventory.main[slot].is_none()));

        // Closing the screen stashes the cursor, named by request -5.
        ok(inventory.handle(
            &request(
                -7,
                vec![StackAction::Place {
                    count: 64,
                    source: at(container::CURSOR, 0, -5),
                    destination: at(container::HOTBAR, 0, 0),
                }],
            ),
            true,
        ));
        assert_eq!(inventory.cursor, None);
        assert_eq!(inventory.main[0].unwrap().count, 64);
    }

    #[test]
    fn stale_or_unknown_references_are_rejected() {
        let mut inventory = Inventory::with_hotbar(&TEST_KIT);
        let (stone, grass) = (*inventory.hotbar(0).unwrap(), *inventory.hotbar(1).unwrap());
        for (source, destination) in [
            // A request the server never saw.
            (at(container::HOTBAR, 0, -99), at(container::CURSOR, 0, 0)),
            // "Empty" when the server has grass there.
            (
                at(container::HOTBAR, 0, stone.id),
                at(container::HOTBAR, 1, 0),
            ),
        ] {
            let response = inventory.handle(
                &request(
                    -1,
                    vec![StackAction::Place {
                        count: 1,
                        source,
                        destination,
                    }],
                ),
                true,
            );
            assert_eq!(response, StackResponse::rejected(-1));
        }
        assert_eq!(inventory.main[1], Some(grass));
    }

    #[test]
    fn dropped_items_leave_the_inventory_for_the_world() {
        let mut inventory = Inventory::with_hotbar(&TEST_KIT);
        let stone = *inventory.hotbar(0).unwrap();
        let response = inventory.handle(
            &request(
                -1,
                vec![StackAction::Drop {
                    count: 1,
                    source: at(container::HOTBAR, 0, stone.id),
                    randomly: false,
                }],
            ),
            false,
        );
        assert_eq!(slots(&response), [(container::HOTBAR, 0, 63, stone.id)]);
        assert_eq!(
            inventory.take_dropped(),
            [ItemStack {
                count: 1,
                ..stone.stack()
            }]
        );
        assert!(inventory.take_dropped().is_empty());

        // A whole stack from the cursor.
        inventory.cursor = inventory.main[1].take();
        let grass = inventory.cursor.unwrap();
        let response = inventory.handle(
            &request(
                -3,
                vec![StackAction::Drop {
                    count: 64,
                    source: at(container::CURSOR, 0, grass.id),
                    randomly: false,
                }],
            ),
            false,
        );
        assert_eq!(slots(&response), [(container::CURSOR, 0, 0, 0)]);
        assert_eq!(inventory.take_dropped(), [grass.stack()]);
    }

    #[test]
    fn the_hud_throws_from_a_slot_only_what_it_holds() {
        let mut inventory = Inventory::with_hotbar(&TEST_KIT);
        let stone = *inventory.hotbar(0).unwrap();
        let grass = inventory.hotbar(1).unwrap().item;
        assert_eq!(
            inventory.throw_from_slot(0, grass, 1),
            None,
            "not what is there"
        );
        assert_eq!(
            inventory.throw_from_slot(0, stone.item, 65),
            None,
            "too many"
        );
        assert_eq!(inventory.throw_from_slot(40, stone.item, 1), None);
        assert_eq!(
            inventory.throw_from_slot(0, stone.item, 1),
            Some(ItemStack {
                count: 1,
                ..stone.stack()
            })
        );
        assert_eq!(
            inventory.throw_from_slot(0, stone.item, 63).unwrap().count,
            63
        );
        assert_eq!(inventory.hotbar(0), None);
    }

    #[test]
    fn picked_up_items_fill_stacks_then_empty_slots() {
        let mut inventory = Inventory::default();
        let stone = items().by_name("minecraft:stone").unwrap().network_id;
        let pick = |count| ItemStack {
            item: stone,
            count,
            metadata: 0,
        };
        assert_eq!(inventory.add(pick(40)), 40);
        assert_eq!(inventory.add(pick(40)), 40);
        assert_eq!(inventory.main[0].unwrap().count, 64);
        assert_eq!(inventory.main[1].unwrap().count, 16);
        // Swords do not stack, so each takes a slot.
        let sword = items()
            .by_name("minecraft:diamond_sword")
            .unwrap()
            .network_id;
        let swords = ItemStack {
            item: sword,
            count: 3,
            metadata: 0,
        };
        assert_eq!(inventory.add(swords), 3);
        assert_eq!(inventory.main[4].unwrap().item, sword);

        // A full inventory takes what fits and no more.
        for slot in inventory.main.iter_mut() {
            *slot = Some(Stack {
                item: sword,
                count: 1,
                metadata: 0,
                id: 99,
            });
        }
        inventory.main[35] = Some(Stack {
            item: stone,
            count: 60,
            metadata: 0,
            id: 100,
        });
        assert_eq!(inventory.add(pick(10)), 4);
    }

    #[test]
    fn a_cursor_with_nowhere_to_go_is_thrown_out() {
        let mut inventory = Inventory::with_hotbar(&TEST_KIT);
        let sword = items()
            .by_name("minecraft:diamond_sword")
            .unwrap()
            .network_id;
        for slot in inventory.main.iter_mut().filter(|slot| slot.is_none()) {
            *slot = Some(Stack {
                item: sword,
                count: 1,
                metadata: 0,
                id: 99,
            });
        }
        let held = ItemStack {
            item: sword,
            count: 1,
            metadata: 0,
        };
        inventory.cursor = Some(Stack {
            item: sword,
            count: 1,
            metadata: 0,
            id: 100,
        });
        assert!(inventory.return_cursor());
        assert_eq!(inventory.cursor, None);
        assert_eq!(inventory.take_dropped(), [held]);
    }

    #[test]
    fn the_cursor_goes_back_into_the_inventory() {
        let mut inventory = Inventory::with_hotbar(&TEST_KIT);
        inventory.cursor = inventory.main[0].take();
        assert_eq!(inventory.cursor_slot().item.count, 64);
        assert!(inventory.return_cursor());
        assert_eq!(inventory.cursor, None);
        assert_eq!(inventory.main[0].unwrap().count, 64, "the first free slot");
        assert!(inventory.cursor_slot().item.is_empty());
        assert!(!inventory.return_cursor());
        assert!(inventory.take_dropped().is_empty());
    }

    #[test]
    fn inventories_save_and_load() {
        let mut inventory = Inventory::with_hotbar(&TEST_KIT);
        inventory.main.swap(0, 30);
        inventory.cursor = inventory.main[1].take();
        let saved = inventory.saved();
        assert_eq!(saved.main.len(), 9, "the cursor item went back in");
        // Slot 0 was emptied by the swap, so the cursor's grass went there.
        assert_eq!(
            (saved.main[0].slot, saved.main[0].item.as_str()),
            (0, "minecraft:grass_block")
        );
        let loaded = Inventory::from_saved(&saved);
        assert_eq!(loaded.saved(), saved);
        assert_eq!(
            loaded.main[30].unwrap().item,
            items().by_name("minecraft:stone").unwrap().network_id
        );

        let odd = SavedInventory {
            main: vec![
                SavedStack {
                    slot: 99,
                    item: "minecraft:stone".into(),
                    count: 1,
                    meta: 0,
                },
                SavedStack {
                    slot: 3,
                    item: "minecraft:no_such_item".into(),
                    count: 1,
                    meta: 0,
                },
                SavedStack {
                    slot: 4,
                    item: "minecraft:diamond_sword".into(),
                    count: 5,
                    meta: 0,
                },
            ],
            armor: Vec::new(),
            offhand: Vec::new(),
        };
        let loaded = Inventory::from_saved(&odd);
        assert_eq!(loaded.saved().main.len(), 1);
        assert_eq!(loaded.main[4].unwrap().count, 1, "swords stack to 1");
    }

    #[test]
    fn placing_uses_up_one_item() {
        let mut inventory = Inventory::with_hotbar(&["minecraft:stone"]);
        let stone = inventory.hotbar(0).unwrap().stack();
        let slot = inventory.use_one(0).unwrap();
        assert_eq!(slot.slot, 0);
        assert_eq!(inventory.hotbar(0).unwrap().count, stone.count - 1);
        assert_eq!(slot.item, inventory.hotbar(0).unwrap().instance());

        for _ in 1..stone.count {
            inventory.use_one(0).unwrap();
        }
        assert!(
            inventory.hotbar(0).is_none(),
            "the last one empties the slot"
        );
        assert!(inventory.use_one(0).is_none());
        assert!(inventory.use_one(9).is_none(), "not a hotbar slot");
    }
}
