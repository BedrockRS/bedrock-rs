//! Falling blocks: sand over air, scaffolding placed too far out. A falling
//! block is an entity until it lands, then a block again where it lands if
//! it can go there, or else its item.
//!
//! As vanilla's and PocketMine's: gravity 0.04 and drag 0.02 per tick,
//! straight down, from the bottom centre of the block it was; 0.98 wide and
//! tall, shown 0.49 above its bottom. It lands on the top of the first
//! collision box under it (a fence's at 1.5), and goes in where its bottom
//! is if that block is replaceable and it stays up there; otherwise, as on
//! a torch or a bottom slab, it breaks into its item. One still falling
//! after 30 seconds, or below the world, breaks too.
//!
//! Clients are not told it has gravity: they only follow the positions sent.
//!
//! A block about to fall is first shown as a held falling block, still,
//! inside the block it will replace, for a tick; then the
//! block goes and it is released. Clients take a frame or two to draw a new
//! entity: added as the block went, it left a gap of a few frames with
//! neither, then appeared already lower (found live on 2026-10-08). It is
//! also put in place with a teleport right after it is added, so clients
//! do not glide it in from wherever they would otherwise start it.
//!
//! Landing, it is moved onto the ground and stays there a tick before the
//! block goes in: clients draw entities a little behind, and turning it
//! into the block the tick it reached the ground snapped it down from where
//! they still showed it (found live on 2026-10-08).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use bedrockrs_protocol::block::{BlockState, StateValue};
use bedrockrs_protocol::packet::Encode as _;
use bedrockrs_protocol::packets::{
    AddActor, EntityMetadata, MetadataValue, MoveActorAbsolute, SetActorMotion, metadata_key,
};
use bedrockrs_protocol::types::{BlockPos, ChunkPos, Vec3};
use bytes::Bytes;

use crate::entities::EntityView;
use crate::shape;
use crate::support::{self, Settled};
use crate::world::{MIN_Y, World};

const GRAVITY: f32 = 0.04;
const DRAG: f32 = 0.02;
const SIZE: f32 = 0.98;
/// Clients draw falling blocks this far above the position they are given.
const NETWORK_OFFSET: f32 = 0.49;
/// Ticks a block may fall before it breaks: 30 seconds, as in vanilla.
const LIFETIME: u32 = 600;
/// How far below the world a block may fall before it breaks.
const VOID_DEPTH: i32 = 64;
pub const ENTITY_TYPE: &str = "minecraft:falling_block";

#[derive(Debug, Clone)]
struct FallingBlock {
    state: BlockState,
    /// The bottom centre of its box.
    position: Vec3,
    velocity: f32,
    age: u32,
    /// Still inside its block, waiting to be released.
    held: bool,
    /// On the ground, to become this next tick.
    landed: Option<Landed>,
}

/// A falling block that stopped this tick.
#[derive(Debug, Clone, PartialEq)]
pub enum Landed {
    /// It goes back in as a block: at `pos`, as `state`, over `replacing`.
    Block {
        pos: BlockPos,
        state: BlockState,
        replacing: u32,
    },
    /// It could not go in, or fell too long, and breaks into its item at `pos`.
    Item { pos: BlockPos, state: BlockState },
}

/// Every falling block in the world, by entity ID.
#[derive(Debug, Default)]
pub struct FallingBlocks {
    blocks: Mutex<HashMap<u64, FallingBlock>>,
}

impl FallingBlocks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn count(&self) -> usize {
        self.blocks().len()
    }

    /// Starts `state` falling from the block at `pos`, moving from the next
    /// tick. Returns the AddActor that shows it exactly where the block was.
    pub fn spawn(&self, entity_id: u64, state: BlockState, pos: BlockPos) -> Bytes {
        self.insert(entity_id, state, pos, false)
    }

    /// Puts a falling block for `state` inside the block at `pos`, which
    /// stays for now: still until
    /// [`FallingBlocks::release`]. Returns what shows it: the AddActor, and
    /// a teleport to where it is, so clients start it there.
    pub fn hold(&self, entity_id: u64, state: BlockState, pos: BlockPos) -> [Bytes; 2] {
        let add = self.insert(entity_id, state, pos, true);
        let blocks = self.blocks();
        let block = &blocks[&entity_id];
        let teleport = MoveActorAbsolute {
            entity_runtime_id: entity_id,
            flags: MoveActorAbsolute::TELEPORT,
            position: shown_at(block),
            rotation: Vec3::default(),
        };
        [add, Bytes::from(teleport.encode())]
    }

    /// Lets a held falling block go, once its block is gone: it moves from
    /// this tick.
    pub fn release(&self, entity_id: u64) {
        if let Some(block) = self.blocks().get_mut(&entity_id) {
            block.held = false;
        }
    }

    /// Takes away a held falling block whose block no longer falls.
    pub fn cancel(&self, entity_id: u64) {
        self.blocks().remove(&entity_id);
    }

    fn insert(&self, entity_id: u64, state: BlockState, pos: BlockPos, held: bool) -> Bytes {
        let block = FallingBlock {
            state,
            position: Vec3 {
                x: pos.x as f32 + 0.5,
                y: pos.y as f32,
                z: pos.z as f32 + 0.5,
            },
            velocity: 0.0,
            age: 0,
            held,
            landed: None,
        };
        let add = add_packet(entity_id, &block);
        self.blocks().insert(entity_id, block);
        add
    }

    /// Moves every falling block one tick. Returns what viewers need to see
    /// of those still falling, and those that stopped, by entity ID.
    pub fn tick(&self, world: &World, tick: u64) -> (Vec<EntityView>, Vec<(u64, Landed)>) {
        let mut blocks = self.blocks();
        let mut landed = Vec::new();
        blocks.retain(|&id, block| {
            if block.held {
                return true;
            }
            // On the ground since last tick: now it becomes its block.
            if let Some(outcome) = block.landed.take() {
                landed.push((id, outcome));
                return false;
            }
            block.age += 1;
            if let Some(outcome) = step(block, world) {
                block.velocity = 0.0;
                block.landed = Some(outcome);
            }
            true
        });
        let views = blocks
            .iter()
            .map(|(&id, block)| EntityView {
                id,
                chunk: ChunkPos::of_block(BlockPos::containing(block.position)),
                add: add_packet(id, block),
                movement: (!block.held).then(|| movement_packets(id, block, tick)),
                changed: false,
            })
            .collect();
        (views, landed)
    }

    fn blocks(&self) -> MutexGuard<'_, HashMap<u64, FallingBlock>> {
        self.blocks.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Moves a falling block one tick, returning how it stopped, if it did.
fn step(block: &mut FallingBlock, world: &World) -> Option<Landed> {
    let here = BlockPos::containing(block.position);
    if block.age > LIFETIME || block.position.y < (MIN_Y - VOID_DEPTH) as f32 {
        return Some(Landed::Item {
            pos: here,
            state: block.state.clone(),
        });
    }
    block.velocity -= GRAVITY;
    let next = block.position.y + block.velocity;
    block.velocity *= 1.0 - DRAG;
    match landing(block.position, next, world) {
        Some(top) => {
            block.position.y = top;
            Some(land(block, world))
        }
        None => {
            block.position.y = next;
            None
        }
    }
}

/// The highest top of a collision box between `from` (the bottom of the
/// block falling) and `to`, if any: where it lands.
fn landing(from: Vec3, to: f32, world: &World) -> Option<f32> {
    // From the block it is in (a fence's top reaches 1.5, so one more
    // above) down to the block it would reach.
    let highest = (from.y.floor() as i32) + 1;
    let lowest = (to.floor() as i32) - 1;
    (lowest..=highest)
        .rev()
        .filter_map(|y| {
            let pos = BlockPos {
                y,
                ..BlockPos::containing(from)
            };
            let state = world.block_state(pos)?;
            let top = shape::collision(state)
                .iter()
                .map(|aabb| aabb.max[1])
                .fold(None, |top: Option<f32>, max| {
                    Some(top.map_or(max, |top| top.max(max)))
                })?;
            let top = y as f32 + top;
            (top <= from.y + 1e-4 && top >= to).then_some(top)
        })
        .reduce(f32::max)
}

/// What becomes of a block that landed: a block where its bottom is, if
/// that is replaceable and it stays up there, or else its item.
fn land(block: &FallingBlock, world: &World) -> Landed {
    let pos = BlockPos::containing(Vec3 {
        y: block.position.y + 1e-3,
        ..block.position
    });
    let replaceable = world.block_state(pos).is_some_and(support::replaceable);
    let mut state = block.state.clone();
    // Scaffolding works out its stability where it lands.
    if let Some((_, value)) = state.states.iter_mut().find(|(key, _)| key == "stability") {
        *value = StateValue::Int(support::scaffolding_stability(pos, world));
    }
    let stays = match support::settled(&state, pos, world) {
        Settled::Stays(settled) => Some(settled),
        // Sand goes in even if it will fall again.
        Settled::Falls if support::falls(&state.name) => Some(state.clone()),
        Settled::Falls | Settled::Pops => None,
    };
    match stays {
        Some(state) if replaceable => Landed::Block {
            pos,
            state,
            replacing: world.block(pos),
        },
        _ => Landed::Item {
            pos,
            state: block.state.clone(),
        },
    }
}

fn metadata(block: &FallingBlock) -> EntityMetadata {
    EntityMetadata(vec![
        (
            metadata_key::FLAGS,
            // No HasGravity: clients only follow the positions the server
            // sends. With it they fell it on their own, out of its block
            // while held and into the ground while it rested there, where it
            // turned black (found live on 2026-10-08).
            MetadataValue::Long(0),
        ),
        (
            metadata_key::VARIANT,
            MetadataValue::Int(block.state.network_id() as i32),
        ),
        (metadata_key::WIDTH, MetadataValue::Float(SIZE)),
        (metadata_key::HEIGHT, MetadataValue::Float(SIZE)),
    ])
}

fn shown_at(block: &FallingBlock) -> Vec3 {
    Vec3 {
        y: block.position.y + NETWORK_OFFSET,
        ..block.position
    }
}

fn add_packet(id: u64, block: &FallingBlock) -> Bytes {
    Bytes::from(
        AddActor {
            entity_unique_id: i64::try_from(id).expect("entity IDs stay small"),
            entity_runtime_id: id,
            entity_type: ENTITY_TYPE.into(),
            position: shown_at(block),
            velocity: Vec3 {
                y: block.velocity,
                ..Vec3::default()
            },
            pitch: 0.0,
            yaw: 0.0,
            head_yaw: 0.0,
            body_yaw: 0.0,
            metadata: metadata(block),
        }
        .encode(),
    )
}

fn movement_packets(id: u64, block: &FallingBlock, tick: u64) -> [Bytes; 2] {
    let flags = if block.landed.is_some() {
        MoveActorAbsolute::ON_GROUND
    } else {
        0
    };
    [
        Bytes::from(
            MoveActorAbsolute {
                entity_runtime_id: id,
                flags,
                position: shown_at(block),
                rotation: Vec3::default(),
            }
            .encode(),
        ),
        Bytes::from(
            SetActorMotion {
                entity_runtime_id: id,
                velocity: Vec3 {
                    y: block.velocity,
                    ..Vec3::default()
                },
                tick,
            }
            .encode(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::palette;

    fn state(name: &str) -> BlockState {
        palette()
            .upgrade(&BlockState::new(format!("minecraft:{name}")))
            .unwrap()
    }

    /// Ticks until everything landed, at most `ticks`.
    fn fall(falling: &FallingBlocks, world: &World, ticks: u64) -> Vec<Landed> {
        for tick in 0..ticks {
            let (_, landed) = falling.tick(world, tick);
            if !landed.is_empty() {
                return landed.into_iter().map(|(_, landed)| landed).collect();
            }
        }
        Vec::new()
    }

    #[test]
    fn sand_falls_onto_the_ground() {
        let world = World::new();
        let falling = FallingBlocks::new();
        // The flat world's grass top is at -60.
        falling.spawn(1, state("sand"), BlockPos { x: 0, y: -50, z: 0 });
        let landed = fall(&falling, &world, 100);
        assert_eq!(
            landed,
            [Landed::Block {
                pos: BlockPos { x: 0, y: -60, z: 0 },
                state: state("sand"),
                replacing: world.air(),
            }]
        );
        assert_eq!(falling.count(), 0);
    }

    #[test]
    fn a_block_rests_on_the_ground_a_tick_before_it_goes_in() {
        let world = World::new();
        let falling = FallingBlocks::new();
        falling.spawn(1, state("sand"), BlockPos { x: 0, y: -57, z: 0 });
        for tick in 0..100 {
            let (views, landed) = falling.tick(&world, tick);
            assert!(landed.is_empty(), "never the tick it reaches the ground");
            let on_ground = falling
                .blocks()
                .get(&1)
                .filter(|block| block.landed.is_some())
                .map(|block| block.position.y);
            if let Some(y) = on_ground {
                // Shown on the ground this tick, a block next tick.
                assert_eq!(y, -60.0);
                assert!(views[0].movement.is_some());
                let (_, landed) = falling.tick(&world, tick + 1);
                assert_eq!(landed.len(), 1);
                return;
            }
        }
        panic!("it never landed");
    }

    #[test]
    fn sand_breaks_on_a_torch_and_stands_on_a_fence() {
        let world = World::new();
        let falling = FallingBlocks::new();
        let torch_at = BlockPos { x: 0, y: -60, z: 0 };
        world.set_block(torch_at, state("torch").network_id());
        falling.spawn(1, state("sand"), BlockPos { x: 0, y: -55, z: 0 });
        assert!(matches!(
            fall(&falling, &world, 100)[..],
            [Landed::Item { pos, .. }] if pos == torch_at
        ));

        let fence_at = BlockPos { x: 2, y: -60, z: 0 };
        world.set_block(fence_at, state("oak_fence").network_id());
        falling.spawn(2, state("sand"), BlockPos { x: 2, y: -55, z: 0 });
        assert!(matches!(
            fall(&falling, &world, 100)[..],
            [Landed::Block { pos, .. }] if pos == BlockPos { x: 2, y: -59, z: 0 }
        ));
    }
}
