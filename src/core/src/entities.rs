//! Item entities: stacks lying in the world after being dropped or broken
//! out of blocks. They fall, slide to a stop and wait to be picked up.
//!
//! Physics and timings follow Dragonfly: gravity 0.04 and drag 0.02 per tick,
//! a 2-second pickup delay for thrown items and half a second otherwise,
//! and a pickup range of the item's box grown by 1 sideways and 0.5 up and
//! down. Items vanish after 5 minutes, as in vanilla.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use bedrockrs_protocol::packet::Encode as _;
use bedrockrs_protocol::packets::{
    AddItemActor, EntityMetadata, ItemInstance, MetadataValue, MoveActorAbsolute, SetActorMotion,
    entity_flag, metadata_key,
};
use bedrockrs_protocol::types::{BlockPos, ChunkPos, Vec3};
use bytes::Bytes;

use crate::items::{ItemNbt, SHIELD, items};
use crate::world::{MIN_Y, World};

const GRAVITY: f32 = 0.04;
const DRAG: f32 = 0.02;
/// How much of its sideways speed an item on the ground keeps each tick:
/// vanilla's block friction of 0.6 (times the drag above).
const GROUND_FRICTION: f32 = 0.6;
/// Below this speed an item on the ground stops.
const REST_SPEED: f32 = 0.001;
/// Half the width and the height of an item's box.
const HALF_SIZE: f32 = 0.125;
const SIZE: f32 = 0.25;
/// Clients draw items this far above the position they are given.
const NETWORK_OFFSET: f32 = 0.125;
/// Ticks before a thrown item can be picked up, and before anything else can.
pub const THROWN_PICKUP_DELAY: u32 = 40;
pub const PICKUP_DELAY: u32 = 10;
/// Ticks an item lasts: 5 minutes.
const LIFETIME: u32 = 6000;
/// How far below the world an item may fall before it is gone.
const VOID_DEPTH: i32 = 64;

/// Some of one item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemStack {
    /// The item's network ID.
    pub item: i16,
    pub count: u8,
    pub metadata: u32,
    pub nbt: ItemNbt,
}

impl ItemStack {
    /// The stack as clients see it on the ground: no stack ID.
    fn instance(&self) -> ItemInstance {
        let item = items().get(self.item);
        ItemInstance {
            network_id: self.item,
            count: self.count.into(),
            metadata: self.metadata,
            stack_network_id: None,
            block_runtime_id: item.and_then(|item| item.block_network_id).unwrap_or(0),
            shield: item.is_some_and(|item| item.name == SHIELD),
            nbt: self.nbt,
        }
    }
}

#[derive(Debug, Clone)]
struct DroppedItem {
    stack: ItemStack,
    /// The bottom centre of its box.
    position: Vec3,
    velocity: Vec3,
    on_ground: bool,
    /// Ticks until it can be picked up.
    pickup_delay: u32,
    age: u32,
    /// Whether it moved this tick.
    moved: bool,
    /// Whether its stack changed, so viewers must be shown it afresh.
    changed: bool,
}

/// An item entity as viewers need it this tick.
#[derive(Debug, Clone)]
pub struct ItemView {
    pub id: u64,
    pub chunk: ChunkPos,
    /// AddItemActor, for viewers that do not have it yet.
    pub add: Bytes,
    /// Its movement this tick, for viewers that have it.
    pub movement: Option<[Bytes; 2]>,
    /// Viewers that have it must be sent it again: its stack changed.
    pub changed: bool,
}

/// Every item entity in the world, by entity ID.
#[derive(Debug, Default)]
pub struct ItemEntities {
    items: Mutex<HashMap<u64, DroppedItem>>,
}

/// An item entity picked up, in whole or in part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickedUp {
    pub entity_id: u64,
    pub taken: ItemStack,
}

impl ItemEntities {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.items().len()
    }

    /// Puts an item entity in the world, its box's bottom centre at `position`.
    pub fn spawn(
        &self,
        entity_id: u64,
        stack: ItemStack,
        position: Vec3,
        velocity: Vec3,
        pickup_delay: u32,
    ) {
        if stack.count == 0 || items().get(stack.item).is_none() {
            return;
        }
        self.items().insert(
            entity_id,
            DroppedItem {
                stack,
                position,
                velocity,
                on_ground: false,
                pickup_delay,
                age: 0,
                moved: true,
                changed: false,
            },
        );
    }

    /// Moves every item one tick and ages it, removing the expired and the
    /// fallen, then says what viewers need to see.
    pub fn tick(&self, world: &World, tick: u64) -> Vec<ItemView> {
        let mut items = self.items();
        items.retain(|_, item| {
            item.age += 1;
            item.pickup_delay = item.pickup_delay.saturating_sub(1);
            item.age < LIFETIME && item.position.y > (MIN_Y - VOID_DEPTH) as f32
        });
        for item in items.values_mut() {
            item.moved = step(item, world);
        }
        items
            .iter_mut()
            .map(|(&id, item)| {
                let view = ItemView {
                    id,
                    chunk: ChunkPos::of_block(BlockPos::containing(item.position)),
                    add: add_packet(id, item),
                    movement: item.moved.then(|| movement_packets(id, item, tick)),
                    changed: item.changed,
                };
                item.changed = false;
                view
            })
            .collect()
    }

    /// Picks up the items a player standing at `feet`, `height` tall, can
    /// reach. `take` is offered each stack and says how many of it the player
    /// took; the rest stays in the world.
    pub fn pick_up(
        &self,
        feet: Vec3,
        height: f32,
        mut take: impl FnMut(ItemStack) -> u8,
    ) -> Vec<PickedUp> {
        let mut items = self.items();
        let mut picked = Vec::new();
        for (&entity_id, item) in items.iter_mut() {
            if item.pickup_delay > 0 || !in_reach(feet, height, item.position) {
                continue;
            }
            let taken = take(item.stack).min(item.stack.count);
            if taken == 0 {
                continue;
            }
            item.stack.count -= taken;
            item.changed = true;
            picked.push(PickedUp {
                entity_id,
                taken: ItemStack {
                    count: taken,
                    ..item.stack
                },
            });
        }
        items.retain(|_, item| item.stack.count > 0);
        picked
    }

    fn items(&self) -> MutexGuard<'_, HashMap<u64, DroppedItem>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Moves an item one tick: gravity, then movement stopped by solid blocks
/// one axis at a time, then drag and ground friction. Returns whether it moved.
fn step(item: &mut DroppedItem, world: &World) -> bool {
    let resting = item.on_ground
        && item.velocity.x.abs() < REST_SPEED
        && item.velocity.z.abs() < REST_SPEED
        && solid(world, below(item.position));
    if resting {
        item.velocity = Vec3::default();
        return false;
    }
    let before = item.position;
    item.velocity.y -= GRAVITY;

    // Vertical first, so an item lands before sliding.
    let mut next = item.position;
    next.y += item.velocity.y;
    if item.velocity.y < 0.0 && solid(world, BlockPos::containing(next)) {
        next.y = next.y.floor() + 1.0;
        item.velocity.y = 0.0;
        item.on_ground = true;
    } else if item.velocity.y > 0.0
        && solid(
            world,
            BlockPos::containing(Vec3 {
                y: next.y + SIZE,
                ..next
            }),
        )
    {
        next.y = item.position.y;
        item.velocity.y = 0.0;
    } else {
        item.on_ground = item.velocity.y == 0.0 && solid(world, below(next));
    }
    for axis in [Axis::X, Axis::Z] {
        let mut moved = next;
        axis.add(&mut moved, axis.of(item.velocity));
        let edge = Vec3 {
            y: moved.y + 0.01,
            ..moved
        };
        let side = axis.of(item.velocity).signum() * HALF_SIZE;
        let mut probe = edge;
        axis.add(&mut probe, side);
        if solid(world, BlockPos::containing(probe)) {
            axis.set(&mut item.velocity, 0.0);
        } else {
            next = moved;
        }
    }
    item.position = next;

    item.velocity.x *= 1.0 - DRAG;
    item.velocity.y *= 1.0 - DRAG;
    item.velocity.z *= 1.0 - DRAG;
    if item.on_ground {
        item.velocity.x *= GROUND_FRICTION;
        item.velocity.z *= GROUND_FRICTION;
    }
    item.position != before
}

#[derive(Clone, Copy)]
enum Axis {
    X,
    Z,
}

impl Axis {
    fn of(self, v: Vec3) -> f32 {
        match self {
            Self::X => v.x,
            Self::Z => v.z,
        }
    }

    fn set(self, v: &mut Vec3, value: f32) {
        match self {
            Self::X => v.x = value,
            Self::Z => v.z = value,
        }
    }

    fn add(self, v: &mut Vec3, delta: f32) {
        match self {
            Self::X => v.x += delta,
            Self::Z => v.z += delta,
        }
    }
}

/// The block just under a point.
fn below(position: Vec3) -> BlockPos {
    BlockPos::containing(Vec3 {
        y: position.y - 0.01,
        ..position
    })
}

/// Whether a block stops items: anything but air, for now.
fn solid(world: &World, pos: BlockPos) -> bool {
    world.block(pos) != world.air()
}

/// Whether an item at `item` is close enough for a player at `feet` to
/// pick up: the item's box grown by 1 sideways and 0.5 vertically touches
/// the player's 0.6-wide box.
fn in_reach(feet: Vec3, height: f32, item: Vec3) -> bool {
    const PLAYER_HALF_WIDTH: f32 = 0.3;
    let reach = HALF_SIZE + 1.0 + PLAYER_HALF_WIDTH;
    (item.x - feet.x).abs() < reach
        && (item.z - feet.z).abs() < reach
        && item.y - 0.5 < feet.y + height
        && item.y + SIZE + 0.5 > feet.y
}

fn metadata() -> EntityMetadata {
    EntityMetadata(vec![
        (
            metadata_key::FLAGS,
            MetadataValue::Long(entity_flag::bits(&[
                entity_flag::HAS_GRAVITY,
                entity_flag::CAN_CLIMB,
            ])),
        ),
        (metadata_key::WIDTH, MetadataValue::Float(SIZE)),
        (metadata_key::HEIGHT, MetadataValue::Float(SIZE)),
    ])
}

fn shown_at(item: &DroppedItem) -> Vec3 {
    Vec3 {
        y: item.position.y + NETWORK_OFFSET,
        ..item.position
    }
}

fn add_packet(id: u64, item: &DroppedItem) -> Bytes {
    Bytes::from(
        AddItemActor {
            entity_unique_id: i64::try_from(id).expect("entity IDs stay small"),
            entity_runtime_id: id,
            item: item.stack.instance(),
            position: shown_at(item),
            velocity: item.velocity,
            metadata: metadata(),
            from_fishing: false,
        }
        .encode(),
    )
}

fn movement_packets(id: u64, item: &DroppedItem, tick: u64) -> [Bytes; 2] {
    let flags = if item.on_ground {
        MoveActorAbsolute::ON_GROUND
    } else {
        0
    };
    [
        Bytes::from(
            MoveActorAbsolute {
                entity_runtime_id: id,
                flags,
                position: shown_at(item),
                rotation: Vec3::default(),
            }
            .encode(),
        ),
        Bytes::from(
            SetActorMotion {
                entity_runtime_id: id,
                velocity: item.velocity,
                tick,
            }
            .encode(),
        ),
    ]
}

/// Where a thrown item starts and how fast it goes: from 1.4 blocks above
/// the thrower's feet, at 0.4 blocks a tick the way they look (as Dragonfly
/// throws), with the item's box centred there.
pub fn throw(feet: Vec3, pitch: f32, yaw: f32) -> (Vec3, Vec3) {
    let (pitch, yaw) = (pitch.to_radians(), yaw.to_radians());
    let direction = Vec3 {
        x: -yaw.sin() * pitch.cos(),
        y: -pitch.sin(),
        z: yaw.cos() * pitch.cos(),
    };
    let position = Vec3 {
        y: feet.y + 1.4 - SIZE / 2.0,
        ..feet
    };
    let velocity = Vec3 {
        x: direction.x * 0.4,
        y: direction.y * 0.4,
        z: direction.z * 0.4,
    };
    (position, velocity)
}

/// Where a block's drop starts and how fast it goes: the middle of the
/// block, popping up with a little sideways scatter picked from `seed`.
pub fn block_drop(pos: BlockPos, seed: u64) -> (Vec3, Vec3) {
    let position = Vec3 {
        x: pos.x as f32 + 0.5,
        y: pos.y as f32 + 0.5 - SIZE / 2.0,
        z: pos.z as f32 + 0.5,
    };
    // A small, cheap scramble; nothing depends on it being random.
    let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let unit = |shift: u32| ((mixed >> shift) & 0xFFFF) as f32 / 65535.0 - 0.5;
    let velocity = Vec3 {
        x: unit(0) * 0.1,
        y: 0.2,
        z: unit(16) * 0.1,
    };
    (position, velocity)
}

/// Where an item a dying player drops starts and how fast it goes: at their
/// feet, popping up and scattering a little, as Dragonfly drops them.
pub fn death_drop(feet: Vec3, seed: u64) -> (Vec3, Vec3) {
    let mixed = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let unit = |shift: u32| ((mixed >> shift) & 0xFFFF) as f32 / 65535.0 - 0.5;
    let position = Vec3 {
        y: feet.y + 0.25,
        ..feet
    };
    let velocity = Vec3 {
        x: unit(0) * 0.2,
        y: 0.2,
        z: unit(16) * 0.2,
    };
    (position, velocity)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stone(count: u8) -> ItemStack {
        ItemStack {
            item: items().by_name("minecraft:stone").unwrap().network_id,
            count,
            metadata: 0,
            nbt: None,
        }
    }

    /// The superflat's grass is at y = -61, so the ground is at y = -60.
    const GROUND: f32 = -60.0;

    fn settle(entities: &ItemEntities, world: &World, ticks: u64) -> Vec<ItemView> {
        let mut last = Vec::new();
        for tick in 0..ticks {
            last = entities.tick(world, tick);
        }
        last
    }

    fn position(entities: &ItemEntities, id: u64) -> Vec3 {
        entities.items()[&id].position
    }

    #[test]
    fn dropped_items_fall_to_the_ground_and_stop() {
        let world = World::new();
        let entities = ItemEntities::new();
        entities.spawn(
            7,
            stone(3),
            Vec3 {
                x: 0.5,
                y: -55.0,
                z: 0.5,
            },
            Vec3::default(),
            PICKUP_DELAY,
        );
        let views = settle(&entities, &world, 100);
        let at = position(&entities, 7);
        assert!((at.y - GROUND).abs() < 1e-4, "{at:?}");
        assert!(views[0].movement.is_none(), "at rest");
        assert_eq!(views[0].chunk, ChunkPos::new(0, 0));
    }

    #[test]
    fn thrown_items_travel_the_way_the_player_looks_and_walls_stop_them() {
        let world = World::new();
        let entities = ItemEntities::new();
        // Looking along +z (yaw 0), level.
        let feet = Vec3 {
            x: 0.5,
            y: GROUND,
            z: 0.5,
        };
        let (start, velocity) = throw(feet, 0.0, 0.0);
        assert!(velocity.z > 0.39 && velocity.x.abs() < 1e-6);
        entities.spawn(1, stone(1), start, velocity, THROWN_PICKUP_DELAY);
        settle(&entities, &world, 100);
        let landed = position(&entities, 1);
        assert!(landed.z > 2.0 && landed.z < 6.0, "{landed:?}");
        assert!((landed.y - GROUND).abs() < 1e-4);

        // A wall two blocks ahead stops the next one.
        for y in -60..-57 {
            world.set_block(
                BlockPos { x: 0, y, z: 2 },
                bedrockrs_protocol::block::BlockState::new("minecraft:stone").network_id(),
            );
        }
        entities.spawn(2, stone(1), start, velocity, THROWN_PICKUP_DELAY);
        settle(&entities, &world, 100);
        let stopped = position(&entities, 2);
        assert!(stopped.z < 2.0, "{stopped:?}");
    }

    #[test]
    fn pickup_waits_for_the_delay_and_takes_what_fits() {
        let world = World::new();
        let entities = ItemEntities::new();
        let spot = Vec3 {
            x: 0.5,
            y: GROUND,
            z: 0.5,
        };
        entities.spawn(1, stone(10), spot, Vec3::default(), PICKUP_DELAY);
        let feet = Vec3 { x: 1.5, ..spot };
        assert!(entities.pick_up(feet, 1.8, |_| 64).is_empty(), "too soon");
        settle(&entities, &world, PICKUP_DELAY.into());

        // Out of reach.
        let far = Vec3 { x: 4.0, ..spot };
        assert!(entities.pick_up(far, 1.8, |_| 64).is_empty());

        // Room for 4: 6 stay behind, and viewers are shown the change.
        let picked = entities.pick_up(feet, 1.8, |_| 4);
        assert_eq!(
            picked,
            [PickedUp {
                entity_id: 1,
                taken: stone(4)
            }]
        );
        assert!(entities.tick(&world, 0)[0].changed);
        let picked = entities.pick_up(feet, 1.8, |stack| stack.count);
        assert_eq!(picked[0].taken, stone(6));
        assert_eq!(entities.count(), 0);
    }

    #[test]
    fn items_expire_and_fall_out_of_the_world() {
        let world = World::new();
        let entities = ItemEntities::new();
        entities.spawn(
            1,
            stone(1),
            Vec3 {
                x: 0.5,
                y: -200.0,
                z: 0.5,
            },
            Vec3::default(),
            0,
        );
        entities.spawn(
            2,
            stone(1),
            Vec3 {
                x: 0.5,
                y: GROUND,
                z: 0.5,
            },
            Vec3::default(),
            0,
        );
        // Unknown items are never spawned.
        entities.spawn(
            3,
            ItemStack {
                item: 32000,
                count: 1,
                metadata: 0,
                nbt: None,
            },
            Vec3::default(),
            Vec3::default(),
            0,
        );
        entities.tick(&world, 0);
        assert_eq!(entities.count(), 1, "the one below the world is gone");
        settle(&entities, &world, LIFETIME.into());
        assert_eq!(entities.count(), 0);
    }

    #[test]
    fn block_drops_pop_up_from_the_middle_of_the_block() {
        let (position, velocity) = block_drop(
            BlockPos {
                x: 3,
                y: -61,
                z: -2,
            },
            42,
        );
        assert_eq!((position.x, position.z), (3.5, -1.5));
        assert!(velocity.y > 0.0);
        assert!(velocity.x.abs() <= 0.05 && velocity.z.abs() <= 0.05);
    }
}
