//! The state a placed block takes: which way it faces, its axis, what it is
//! attached to, and which neighbours it connects to. Rules follow Dragonfly,
//! applied by the states a block has rather than block by block, so every
//! block with, say, `pillar_axis` gets the same treatment.
//!
//! Neighbour-dependent states (fence, pane, bar and wall connections, wall
//! heights, fence gates lowered in walls, stairs corners) are also
//! recomputed for the blocks around a change. Two-block blocks (doors, beds,
//! tall flowers) place both halves; what each block needs to stay in place is
//! [`support`]'s.

use bedrockrs_protocol::block::{BlockState, StateValue};
use bedrockrs_protocol::types::{BlockPos, Vec3};

use crate::blocks::palette;
use crate::support::{self, full_cube, solid};
use crate::world::World;

/// How the player placed the block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placing {
    /// The face of the clicked block the new one goes against: 0 down, 1 up,
    /// 2 north, 3 south, 4 west, 5 east.
    pub face: u8,
    /// Where on the clicked block the click landed, each axis 0 to 1.
    pub click: Vec3,
    pub pitch: f32,
    pub yaw: f32,
}

/// Why a block cannot be placed as asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("the client does not know this block")]
    UnknownBlock,
    #[error("it cannot hang from a ceiling")]
    OnCeiling,
    #[error("it cannot go on that face")]
    WrongFace,
    #[error("it only goes in water")]
    NeedsWater,
    #[error("no valid state fits")]
    NoValidState,
}

/// A horizontal direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    North,
    South,
    West,
    East,
}

impl Direction {
    /// The way a player with this yaw looks, as Dragonfly rounds it.
    fn of_yaw(yaw: f32) -> Self {
        let yaw = (yaw + 180.0).rem_euclid(360.0) - 180.0;
        match yaw {
            y if y > 45.0 && y <= 135.0 => Self::West,
            y if y > -45.0 && y <= 45.0 => Self::South,
            y if y > -135.0 && y <= -45.0 => Self::East,
            _ => Self::North,
        }
    }

    fn opposite(self) -> Self {
        match self {
            Self::North => Self::South,
            Self::South => Self::North,
            Self::West => Self::East,
            Self::East => Self::West,
        }
    }

    fn rotate_right(self) -> Self {
        match self {
            Self::North => Self::East,
            Self::East => Self::South,
            Self::South => Self::West,
            Self::West => Self::North,
        }
    }

    fn rotate_left(self) -> Self {
        self.rotate_right().opposite()
    }

    /// The direction a horizontal face (2 to 5) points.
    fn of_face(face: u8) -> Option<Self> {
        Some(match face {
            2 => Self::North,
            3 => Self::South,
            4 => Self::West,
            5 => Self::East,
            _ => return None,
        })
    }

    fn from_name(name: &str) -> Option<Self> {
        support::face_of(name).and_then(Self::of_face)
    }

    fn face(self) -> u8 {
        match self {
            Self::North => 2,
            Self::South => 3,
            Self::West => 4,
            Self::East => 5,
        }
    }

    fn name(self) -> &'static str {
        face_name(self.face())
    }

    /// Stairs' `weirdo_direction`.
    fn weirdo(self) -> i32 {
        match self {
            Self::East => 0,
            Self::West => 1,
            Self::South => 2,
            Self::North => 3,
        }
    }

    fn from_weirdo(value: i32) -> Option<Self> {
        [Self::East, Self::West, Self::South, Self::North]
            .get(usize::try_from(value).ok()?)
            .copied()
    }

    /// The legacy `direction` state: 0 south, 1 west, 2 north, 3 east.
    fn legacy(self) -> i32 {
        match self {
            Self::South => 0,
            Self::West => 1,
            Self::North => 2,
            Self::East => 3,
        }
    }

    const ALL: [Self; 4] = [Self::North, Self::East, Self::South, Self::West];
}

fn face_name(face: u8) -> &'static str {
    match face {
        0 => "down",
        1 => "up",
        2 => "north",
        3 => "south",
        4 => "west",
        _ => "east",
    }
}

pub(crate) fn opposite_face(face: u8) -> u8 {
    match face {
        0 => 1,
        1 => 0,
        2 => 3,
        3 => 2,
        4 => 5,
        _ => 4,
    }
}

pub(crate) fn side(pos: BlockPos, face: u8) -> BlockPos {
    let BlockPos { x, y, z } = pos;
    match face {
        0 => BlockPos { x, y: y - 1, z },
        1 => BlockPos { x, y: y + 1, z },
        2 => BlockPos { x, y, z: z - 1 },
        3 => BlockPos { x, y, z: z + 1 },
        4 => BlockPos { x: x - 1, y, z },
        _ => BlockPos { x: x + 1, y, z },
    }
}

/// Blocks whose `facing_direction` is the face they were placed against,
/// rather than towards the player.
const ATTACHED: [&str; 12] = [
    "ladder",
    "end_rod",
    "lightning_rod",
    "amethyst_cluster",
    "_amethyst_bud",
    "wall_sign",
    "wall_banner",
    "tripwire_hook",
    "_button",
    "frame",
    "_skull",
    "_head",
];

/// The `multi_face_direction_bits` bit of a face: 0 down, 1 up, 2 south,
/// 3 west, 4 north, 5 east.
fn multi_face_bit(face: u8) -> i32 {
    1 << match face {
        0 => 0,
        1 => 1,
        3 => 2,
        4 => 3,
        2 => 4,
        _ => 5,
    }
}

/// The `vine_direction_bits` bit of a side: 1 south, 2 west, 4 north, 8 east.
fn vine_bit(direction: Direction) -> i32 {
    match direction {
        Direction::South => 1,
        Direction::West => 2,
        Direction::North => 4,
        Direction::East => 8,
    }
}

/// The state a block item places at `target`, as the player placed it; it
/// is checked against the states the client knows.
pub fn placed_state(
    item_state: &BlockState,
    target: BlockPos,
    placing: &Placing,
    world: &World,
) -> Result<BlockState, Refusal> {
    let mut state = palette().upgrade(item_state).ok_or(Refusal::UnknownBlock)?;
    let name = state.name.clone();
    let looking = Direction::of_yaw(placing.yaw);
    let upper_half = placing.face == 0 || (placing.click.y > 0.5 && placing.face != 1);
    // Looking steeply up or down faces blocks vertically.
    let toward_player = if placing.pitch > 45.0 {
        1
    } else if placing.pitch < -45.0 {
        0
    } else {
        looking.opposite().face()
    };

    let short = support::short(&name).to_owned();
    let door = short.ends_with("_door");
    // What the block is fixed to: the clicked face, for blocks on walls.
    let wall_face = Direction::of_face(placing.face);
    for (key, value) in state.states.iter_mut() {
        let new = match key.as_str() {
            // The lower half; the upper one is added with it. Carpets use the
            // bit for layering.
            "upper_block_bit" if !name.contains("carpet") => StateValue::Byte(0),
            // The foot; the head is added with it.
            "head_piece_bit" => StateValue::Byte(0),
            "door_hinge_bit" => {
                // Next to a door on its left, a door hinges on the right, so
                // the two open as a pair.
                let left = side(target, looking.rotate_left().face());
                let pair = world
                    .block_state(left)
                    .is_some_and(|other| support::short(&other.name).ends_with("_door"));
                StateValue::Byte(u8::from(pair))
            }
            "persistent_bit" => StateValue::Byte(1),
            "big_dripleaf_head" => StateValue::Byte(1),
            "dripstone_thickness" => StateValue::String("tip".into()),
            "hanging" if short.ends_with("lantern") || short == "pointed_dripstone" => {
                StateValue::Byte(u8::from(placing.face == 0))
            }
            "hanging" if short.ends_with("hanging_sign") => match placing.face {
                0 => StateValue::Byte(1),
                1 => return Err(Refusal::WrongFace),
                _ => StateValue::Byte(0),
            },
            "attachment" => StateValue::String(
                match placing.face {
                    0 => "hanging",
                    1 => "standing",
                    _ => "side",
                }
                .into(),
            ),
            "lever_direction" => {
                let axis = if matches!(looking, Direction::East | Direction::West) {
                    "east_west"
                } else {
                    "north_south"
                };
                StateValue::String(match placing.face {
                    0 => format!("down_{axis}"),
                    1 => format!("up_{axis}"),
                    face => face_name(face).to_owned(),
                })
            }
            "multi_face_direction_bits" => {
                StateValue::Int(multi_face_bit(opposite_face(placing.face)))
            }
            "vine_direction_bits" => match wall_face {
                Some(facing) => StateValue::Int(vine_bit(facing.opposite())),
                None if placing.face == 0 => StateValue::Int(0),
                None => return Err(Refusal::WrongFace),
            },
            "coral_direction" => StateValue::Int(match wall_face {
                Some(Direction::West) => 0,
                Some(Direction::East) => 1,
                Some(Direction::North) => 2,
                Some(Direction::South) => 3,
                None => return Err(Refusal::WrongFace),
            }),
            "rail_direction" => StateValue::Int(i32::from(matches!(
                looking,
                Direction::East | Direction::West
            ))),
            "orientation" => StateValue::String(match toward_player {
                0 => format!("down_{}", looking.name()),
                1 => format!("up_{}", looking.name()),
                face => format!("{}_up", face_name(face)),
            }),
            "pillar_axis" => StateValue::String(
                match placing.face {
                    0 | 1 => "y",
                    2 | 3 => "z",
                    _ => "x",
                }
                .into(),
            ),
            "weirdo_direction" => StateValue::Int(looking.weirdo()),
            "upside_down_bit" => StateValue::Byte(u8::from(upper_half)),
            "minecraft:vertical_half" => {
                StateValue::String(if upper_half { "top" } else { "bottom" }.into())
            }
            // Doors turn with their hinge; gates open away from the player.
            "minecraft:cardinal_direction" if door => {
                StateValue::String(looking.rotate_right().name().into())
            }
            "minecraft:cardinal_direction" if short.ends_with("fence_gate") => {
                StateValue::String(looking.name().into())
            }
            "minecraft:cardinal_direction" => StateValue::String(looking.opposite().name().into()),
            // Cocoa faces the log it grows on; hooks and bells on walls face
            // out of them.
            "direction" if short == "cocoa" => match wall_face {
                Some(facing) => StateValue::Int(facing.opposite().legacy()),
                None => return Err(Refusal::WrongFace),
            },
            // Trapdoors face away from the player, so they open towards
            // them: 3 minus Dragonfly's direction (north 0, south 1, west 2,
            // east 3), which is PocketMine's 5 minus the face.
            "direction" if short.ends_with("trapdoor") => {
                StateValue::Int(5 - i32::from(looking.opposite().face()))
            }
            "direction" if short == "tripwire_hook" => match wall_face {
                Some(facing) => StateValue::Int(facing.legacy()),
                None => return Err(Refusal::WrongFace),
            },
            "direction"
                if wall_face.is_some() && matches!(short.as_str(), "bell" | "grindstone") =>
            {
                StateValue::Int(wall_face.map_or(0, Direction::legacy))
            }
            "direction" => StateValue::Int(looking.legacy()),
            "ground_sign_direction" => {
                StateValue::Int(((placing.yaw + 180.0) * 16.0 / 360.0).round() as i32 & 15)
            }
            "torch_facing_direction" => match placing.face {
                0 => return Err(Refusal::OnCeiling),
                1 => StateValue::String("top".into()),
                face => StateValue::String(face_name(opposite_face(face)).into()),
            },
            "minecraft:block_face" => StateValue::String(face_name(placing.face).into()),
            // A stem points at its fruit, which it has none of yet.
            "facing_direction" if short.ends_with("_stem") => continue,
            "facing_direction" | "minecraft:facing_direction" => {
                let attached = ATTACHED.iter().any(|part| name.contains(part));
                let face = if attached && placing.face == 0 && !short.ends_with("_rod") {
                    // Skulls and wall-fixed blocks do not go on ceilings.
                    if short.ends_with("_skull") || short.ends_with("_head") {
                        return Err(Refusal::OnCeiling);
                    }
                    placing.face
                } else if attached {
                    placing.face
                } else if short.ends_with("glazed_terracotta") {
                    // Turned only around the vertical, facing the player.
                    looking.opposite().face()
                } else if short == "hopper" {
                    // Into the clicked block, or down when placed on top.
                    if placing.face == 1 {
                        0
                    } else {
                        opposite_face(placing.face)
                    }
                } else if name.ends_with(":observer") {
                    opposite_face(toward_player)
                } else {
                    toward_player
                };
                if key == "facing_direction" {
                    StateValue::Int(face.into())
                } else {
                    StateValue::String(face_name(face).into())
                }
            }
            _ => continue,
        };
        *value = new;
    }
    derive(&mut state, target, world);
    if palette().is_valid(&state) {
        Ok(state)
    } else {
        Err(Refusal::NoValidState)
    }
}

/// The blocks placing a block item at `target` puts in the world: the block,
/// and for two-block blocks the other half (the top of a door or tall
/// flower, the head of a bed). Whether they fit and stay up is the caller's
/// to check, with [`support::supported`].
pub fn placed_parts(
    item_state: &BlockState,
    target: BlockPos,
    placing: &Placing,
    world: &World,
) -> Result<Vec<(BlockPos, BlockState)>, Refusal> {
    // Water plants go into water, never onto dry land.
    let aquatic = matches!(support::short(&item_state.name), "kelp" | "seagrass");
    let in_water = world
        .block_state(target)
        .is_some_and(|block| support::short(&block.name).contains("water"));
    if aquatic && !in_water {
        return Err(Refusal::NeedsWater);
    }
    let state = placed_state(item_state, target, placing, world)?;
    let mut parts = vec![(target, state.clone())];
    if let Some((other_pos, _)) = support::partner(&state, target) {
        let mut other = state;
        for (key, value) in other.states.iter_mut() {
            if key == "upper_block_bit" || key == "head_piece_bit" {
                *value = StateValue::Byte(1);
            }
        }
        if !palette().is_valid(&other) {
            return Err(Refusal::NoValidState);
        }
        parts.push((other_pos, other));
    }
    Ok(parts)
}

/// Whether a player opens and closes `state` by using it: wooden doors,
/// trapdoors and fence gates. Iron ones need redstone.
pub fn openable(state: &BlockState) -> bool {
    let name = support::short(&state.name);
    let kind =
        name.ends_with("_door") || name.ends_with("trapdoor") || name.ends_with("fence_gate");
    kind && !name.starts_with("iron_") && support::int(state, "open_bit").is_some()
}

/// `state` opened or closed by a player looking at `yaw`. A gate swings
/// away from whoever opens it.
pub fn toggled(state: &BlockState, yaw: f32) -> BlockState {
    let opening = support::int(state, "open_bit") == Some(0);
    let looking = Direction::of_yaw(yaw);
    let gate = support::short(&state.name).ends_with("fence_gate");
    let mut toggled = state.clone();
    for (key, value) in toggled.states.iter_mut() {
        match key.as_str() {
            "open_bit" => *value = StateValue::Byte(u8::from(opening)),
            "minecraft:cardinal_direction" if gate && opening => {
                let facing = match value {
                    StateValue::String(name) => Direction::from_name(name),
                    _ => None,
                };
                if facing == Some(looking.opposite()) {
                    *value = StateValue::String(looking.name().into());
                }
            }
            _ => {}
        }
    }
    toggled
}

/// A slab placed onto a matching half slab, which becomes the double slab:
/// the slab clicked, when its open half was clicked (the top of a bottom
/// slab, the bottom of a top one), or one already filling the spot the new
/// slab would go. As Dragonfly merges them. Returns where the double slab
/// goes, its state, and the slab it replaces there.
pub fn slab_merge(
    item_state: &BlockState,
    clicked: BlockPos,
    face: u8,
    world: &World,
) -> Option<(BlockPos, BlockState, u32)> {
    let double = palette().double_slab(&item_state.name)?;
    let half = |pos: BlockPos| {
        let state = world.block_state(pos)?;
        (state.name == item_state.name).then_some(())?;
        state
            .states
            .iter()
            .find_map(|(key, value)| match (key.as_str(), value) {
                ("minecraft:vertical_half", StateValue::String(half)) => Some(half.clone()),
                _ => None,
            })
    };
    let open_half_clicked = match half(clicked).as_deref() {
        Some("bottom") => face == 1,
        Some("top") => face == 0,
        _ => false,
    };
    let at = if open_half_clicked {
        clicked
    } else if half(side(clicked, face)).is_some() {
        side(clicked, face)
    } else {
        return None;
    };
    let state = palette().upgrade(&BlockState::new(double))?;
    Some((at, state, world.block(at)))
}

/// The blocks around `pos` whose connections or shape depend on it, with
/// their new state where it changed.
pub fn neighbour_updates(pos: BlockPos, world: &World) -> Vec<(BlockPos, BlockState)> {
    (0..6)
        .filter_map(|face| {
            let neighbour = side(pos, face);
            let current = world.block_state(neighbour)?.clone();
            let mut updated = current.clone();
            derive(&mut updated, neighbour, world);
            (updated != current && palette().is_valid(&updated)).then_some((neighbour, updated))
        })
        .collect()
}

/// Sets the states that depend on the neighbours: connections and stairs corners.
fn derive(state: &mut BlockState, pos: BlockPos, world: &World) {
    let kind = Connector::of(&state.name);
    let above = world.block_state(side(pos, support::UP)).cloned();
    if let Some(kind) = kind {
        let connected: Vec<(Direction, bool)> = Direction::ALL
            .into_iter()
            .map(|direction| (direction, kind.connects(pos, direction, world)))
            .collect();
        let is = |direction: Direction| connected.iter().any(|&(d, on)| d == direction && on);
        for (key, value) in state.states.iter_mut() {
            let direction = |suffix: &str| match suffix {
                "north" => Some(Direction::North),
                "south" => Some(Direction::South),
                "west" => Some(Direction::West),
                "east" => Some(Direction::East),
                _ => None,
            };
            if let Some(suffix) = key.strip_prefix("minecraft:connection_")
                && let Some(direction) = direction(suffix)
            {
                *value = StateValue::Byte(u8::from(is(direction)));
            } else if let Some(suffix) = key.strip_prefix("wall_connection_type_")
                && let Some(direction) = direction(suffix)
            {
                // Tall where the block above covers the connection.
                let height = if !is(direction) {
                    "none"
                } else if above.as_ref().is_some_and(|above| covers(above, direction)) {
                    "tall"
                } else {
                    "short"
                };
                *value = StateValue::String(height.into());
            } else if key == "wall_post_bit" {
                // A post, unless the wall runs straight through and nothing
                // above stands on its middle.
                let straight = (is(Direction::North)
                    && is(Direction::South)
                    && !is(Direction::East)
                    && !is(Direction::West))
                    || (is(Direction::East)
                        && is(Direction::West)
                        && !is(Direction::North)
                        && !is(Direction::South));
                let held_up = above.as_ref().is_some_and(stands_on_post);
                *value = StateValue::Byte(u8::from(!straight || held_up));
            }
        }
    }
    if support::short(&state.name).ends_with("fence_gate") {
        // Lowered between walls.
        let facing =
            support::text(state, "minecraft:cardinal_direction").and_then(Direction::from_name);
        let wall = |direction: Direction| {
            world
                .block_state(side(pos, direction.face()))
                .is_some_and(|block| matches!(Connector::of(&block.name), Some(Connector::Wall)))
        };
        let lowered =
            facing.is_some_and(|facing| wall(facing.rotate_left()) || wall(facing.rotate_right()));
        if let Some((_, value)) = state
            .states
            .iter_mut()
            .find(|(key, _)| key == "in_wall_bit")
        {
            *value = StateValue::Byte(u8::from(lowered));
        }
    }
    if let Some(stairs) = Stairs::of(state) {
        let corner = stairs.corner(pos, world);
        if let Some((_, value)) = state
            .states
            .iter_mut()
            .find(|(key, _)| key == "minecraft:corner")
        {
            *value = StateValue::String(corner.into());
        }
    }
}

/// Whether `above` covers a wall's connection in `direction`, making it
/// tall: a full block, the bottom half of a slab or stairs, or a wall or thin
/// block above connecting that way too.
fn covers(above: &BlockState, direction: Direction) -> bool {
    let name = support::short(&above.name);
    if full_cube(above) || support::sturdy(above, support::DOWN) {
        return true;
    }
    let connects = |key: &str| match support::text(above, key) {
        Some(height) => height != "none",
        None => support::int(above, key) == Some(1),
    };
    match Connector::of(&above.name) {
        Some(Connector::Wall) => connects(&format!("wall_connection_type_{}", direction.name())),
        Some(_) => connects(&format!("minecraft:connection_{}", direction.name())),
        None => name.ends_with("_wall"),
    }
}

/// Whether `above` stands on the middle of a wall, giving it a post: a
/// standing torch, lantern or sign, or another wall's post.
fn stands_on_post(above: &BlockState) -> bool {
    let name = support::short(&above.name);
    support::text(above, "torch_facing_direction") == Some("top")
        || (name.ends_with("lantern") && support::int(above, "hanging") == Some(0))
        || name.contains("standing_sign")
        || name == "standing_banner"
        || (name.ends_with("_wall") && support::int(above, "wall_post_bit") == Some(1))
}

/// Blocks that connect to their neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Connector {
    /// Fences; wooden ones connect only to wooden ones, nether brick to nether brick.
    Fence {
        wooden: bool,
    },
    /// Glass panes and iron bars.
    Thin,
    Wall,
}

impl Connector {
    fn of(name: &str) -> Option<Self> {
        if name.ends_with("_fence") {
            Some(Self::Fence {
                wooden: !name.contains("nether_brick"),
            })
        } else if name.ends_with("_pane") || name.ends_with(":iron_bars") {
            Some(Self::Thin)
        } else if name.ends_with("_wall") {
            Some(Self::Wall)
        } else {
            None
        }
    }

    fn connects(self, pos: BlockPos, direction: Direction, world: &World) -> bool {
        let Some(neighbour) = world.block_state(side(pos, direction.face())) else {
            return false;
        };
        let name = neighbour.name.as_str();
        let gate = name.ends_with("_fence_gate");
        match (self, Self::of(name)) {
            (Self::Fence { wooden }, Some(Self::Fence { wooden: other })) => wooden == other,
            (Self::Fence { .. }, _) if gate => true,
            (Self::Thin | Self::Wall, Some(Self::Thin | Self::Wall)) => true,
            (Self::Wall, _) if gate => true,
            (_, Some(_)) => false,
            _ => solid(name),
        }
    }
}

/// A stairs block, for its corner shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stairs {
    facing: Direction,
    upside_down: bool,
}

impl Stairs {
    fn of(state: &BlockState) -> Option<Self> {
        if !state.name.ends_with("_stairs") {
            return None;
        }
        let mut facing = None;
        let mut upside_down = false;
        for (key, value) in &state.states {
            match (key.as_str(), value) {
                ("weirdo_direction", StateValue::Int(value)) => {
                    facing = Direction::from_weirdo(*value)
                }
                ("upside_down_bit", StateValue::Byte(value)) => upside_down = *value == 1,
                _ => {}
            }
        }
        Some(Self {
            facing: facing?,
            upside_down,
        })
    }

    fn at(pos: BlockPos, world: &World) -> Option<Self> {
        Self::of(world.block_state(pos)?)
    }

    /// The corner shape, from the stairs in front and behind, as Dragonfly
    /// works it out.
    fn corner(self, pos: BlockPos, world: &World) -> &'static str {
        let rotated = self.facing.rotate_right();
        let continued = || {
            Self::at(side(pos, rotated.face()), world).is_some_and(|other| {
                other.facing == self.facing && other.upside_down == self.upside_down
            })
        };
        if let Some(closed) = Self::at(side(pos, self.facing.face()), world)
            && closed.upside_down == self.upside_down
        {
            if closed.facing == rotated {
                return "outer_right";
            } else if closed.facing == rotated.opposite() {
                return if continued() { "none" } else { "outer_left" };
            }
        }
        if let Some(open) = Self::at(side(pos, self.facing.opposite().face()), world)
            && open.upside_down == self.upside_down
        {
            if open.facing == rotated && !continued() {
                return "inner_right";
            } else if open.facing == rotated.opposite() {
                return "inner_left";
            }
        }
        "none"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(value: &str) -> StateValue {
        StateValue::String(value.into())
    }

    fn get<'a>(state: &'a BlockState, key: &str) -> &'a StateValue {
        &state.states.iter().find(|(k, _)| k == key).unwrap().1
    }

    fn item(name: &str) -> BlockState {
        crate::items::items()
            .by_name(name)
            .unwrap()
            .block
            .clone()
            .unwrap()
    }

    const GROUND: BlockPos = BlockPos { x: 0, y: -60, z: 0 };

    fn on_top(yaw: f32) -> Placing {
        Placing {
            face: 1,
            click: Vec3 {
                x: 0.5,
                y: 1.0,
                z: 0.5,
            },
            pitch: 10.0,
            yaw,
        }
    }

    fn put(world: &World, pos: BlockPos, state: &BlockState) {
        world.set_block(pos, state.network_id());
    }

    #[test]
    fn yaw_rounds_to_the_nearest_direction() {
        assert_eq!(Direction::of_yaw(0.0), Direction::South);
        assert_eq!(Direction::of_yaw(90.0), Direction::West);
        assert_eq!(Direction::of_yaw(-90.0), Direction::East);
        assert_eq!(Direction::of_yaw(180.0), Direction::North);
        assert_eq!(Direction::of_yaw(-170.0), Direction::North);
        assert_eq!(Direction::of_yaw(350.0), Direction::South);
    }

    #[test]
    fn stairs_face_the_way_the_player_looks_and_flip_on_upper_halves() {
        let world = World::new();
        let stairs = placed_state(
            &item("minecraft:oak_stairs"),
            GROUND,
            &on_top(-90.0),
            &world,
        )
        .unwrap();
        assert_eq!(
            get(&stairs, "weirdo_direction"),
            &StateValue::Int(0),
            "east"
        );
        assert_eq!(get(&stairs, "upside_down_bit"), &StateValue::Byte(0));
        assert_eq!(get(&stairs, "minecraft:corner"), &string("none"));

        let side_high = Placing {
            face: 2,
            click: Vec3 {
                x: 0.5,
                y: 0.8,
                z: 0.0,
            },
            ..on_top(0.0)
        };
        let stairs =
            placed_state(&item("minecraft:oak_stairs"), GROUND, &side_high, &world).unwrap();
        assert_eq!(get(&stairs, "upside_down_bit"), &StateValue::Byte(1));
        assert_eq!(
            get(&stairs, "weirdo_direction"),
            &StateValue::Int(2),
            "south"
        );
    }

    #[test]
    fn stairs_turn_corners_with_their_neighbours() {
        let world = World::new();
        // Stairs facing east at x = 1; new stairs facing north just west of
        // them close the corner: outer right, as Dragonfly computes it.
        let east = placed_state(
            &item("minecraft:oak_stairs"),
            GROUND,
            &on_top(-90.0),
            &world,
        )
        .unwrap();
        put(
            &world,
            BlockPos {
                x: 0,
                y: -60,
                z: -1,
            },
            &east,
        );
        let north = placed_state(
            &item("minecraft:oak_stairs"),
            GROUND,
            &on_top(180.0),
            &world,
        )
        .unwrap();
        assert_eq!(get(&north, "minecraft:corner"), &string("outer_right"));
    }

    #[test]
    fn logs_follow_the_clicked_face_and_chests_face_the_player() {
        let world = World::new();
        let side = Placing {
            face: 5,
            ..on_top(0.0)
        };
        let log = placed_state(&item("minecraft:oak_log"), GROUND, &side, &world).unwrap();
        assert_eq!(get(&log, "pillar_axis"), &string("x"));
        let chest = placed_state(&item("minecraft:chest"), GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(
            get(&chest, "minecraft:cardinal_direction"),
            &string("north"),
            "looking south"
        );
    }

    #[test]
    fn torches_attach_to_what_was_clicked() {
        let world = World::new();
        let torch = placed_state(&item("minecraft:torch"), GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(get(&torch, "torch_facing_direction"), &string("top"));
        let north_side = Placing {
            face: 2,
            ..on_top(0.0)
        };
        let torch = placed_state(&item("minecraft:torch"), GROUND, &north_side, &world).unwrap();
        assert_eq!(get(&torch, "torch_facing_direction"), &string("south"));
        let ceiling = Placing {
            face: 0,
            ..on_top(0.0)
        };
        assert_eq!(
            placed_state(&item("minecraft:torch"), GROUND, &ceiling, &world),
            Err(Refusal::OnCeiling)
        );
    }

    #[test]
    fn slabs_take_the_half_that_was_clicked() {
        let world = World::new();
        let slab = placed_state(&item("minecraft:oak_slab"), GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(get(&slab, "minecraft:vertical_half"), &string("bottom"));
        let high = Placing {
            face: 3,
            click: Vec3 {
                x: 0.5,
                y: 0.9,
                z: 1.0,
            },
            ..on_top(0.0)
        };
        let slab = placed_state(&item("minecraft:oak_slab"), GROUND, &high, &world).unwrap();
        assert_eq!(get(&slab, "minecraft:vertical_half"), &string("top"));
    }

    #[test]
    fn matching_half_slabs_become_double_slabs() {
        let world = World::new();
        let oak = item("minecraft:oak_slab");
        let bottom = placed_state(&oak, GROUND, &on_top(0.0), &world).unwrap();
        put(&world, GROUND, &bottom);
        let double = BlockState::new("minecraft:oak_double_slab")
            .with("minecraft:vertical_half", string("bottom"));

        // Clicking the top of the bottom slab doubles it where it is.
        let merged = slab_merge(&oak, GROUND, 1, &world).unwrap();
        assert_eq!(merged, (GROUND, double.clone(), bottom.network_id()));
        // Its side does not: a new slab goes next to it.
        assert_eq!(slab_merge(&oak, GROUND, 5, &world), None);
        // Clicking the grass under a spot a half slab fills doubles that one.
        let below = BlockPos { x: 0, y: -61, z: 0 };
        assert_eq!(
            slab_merge(&oak, below, 1, &world).map(|(at, ..)| at),
            Some(GROUND)
        );
        // Another kind of slab, or a non-slab, does not merge.
        assert_eq!(
            slab_merge(&item("minecraft:spruce_slab"), GROUND, 1, &world),
            None
        );
        assert_eq!(
            slab_merge(&item("minecraft:stone"), GROUND, 1, &world),
            None
        );

        // A top slab doubles from below.
        let top =
            BlockState::new("minecraft:oak_slab").with("minecraft:vertical_half", string("top"));
        let above = BlockPos { x: 0, y: -58, z: 0 };
        put(&world, above, &top);
        assert_eq!(
            slab_merge(&oak, above, 0, &world).map(|(at, ..)| at),
            Some(above)
        );
        assert_eq!(
            slab_merge(&oak, above, 1, &world),
            None,
            "its top is not open"
        );
    }

    #[test]
    fn fences_panes_and_walls_connect_to_what_they_should() {
        let world = World::new();
        let stone = BlockState::new("minecraft:stone");
        put(&world, BlockPos { x: 1, y: -60, z: 0 }, &stone);
        let fence =
            placed_state(&item("minecraft:oak_fence"), GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(
            get(&fence, "minecraft:connection_east"),
            &StateValue::Byte(1),
            "stone"
        );
        assert_eq!(
            get(&fence, "minecraft:connection_west"),
            &StateValue::Byte(0),
            "air"
        );
        put(&world, GROUND, &fence);

        // Bars next to the fence do not connect to it; a second fence does,
        // and the first one connects back.
        let west = BlockPos {
            x: -1,
            y: -60,
            z: 0,
        };
        let bars = placed_state(&item("minecraft:iron_bars"), west, &on_top(0.0), &world).unwrap();
        assert_eq!(
            get(&bars, "minecraft:connection_east"),
            &StateValue::Byte(0)
        );
        let north = BlockPos {
            x: 0,
            y: -60,
            z: -1,
        };
        let other =
            placed_state(&item("minecraft:spruce_fence"), north, &on_top(0.0), &world).unwrap();
        assert_eq!(
            get(&other, "minecraft:connection_south"),
            &StateValue::Byte(1)
        );
        put(&world, north, &other);
        let updates = neighbour_updates(north, &world);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].0, GROUND);
        assert_eq!(
            get(&updates[0].1, "minecraft:connection_north"),
            &StateValue::Byte(1)
        );

        // A wall between two others runs straight, without a post.
        let wall = item("minecraft:cobblestone_wall");
        let world = World::new();
        put(&world, BlockPos { x: 1, y: -60, z: 0 }, &wall);
        put(
            &world,
            BlockPos {
                x: -1,
                y: -60,
                z: 0,
            },
            &wall,
        );
        let straight = placed_state(&wall, GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(
            get(&straight, "wall_connection_type_east"),
            &string("short")
        );
        assert_eq!(
            get(&straight, "wall_connection_type_north"),
            &string("none")
        );
        assert_eq!(get(&straight, "wall_post_bit"), &StateValue::Byte(0));
    }

    fn block(name: &str) -> BlockState {
        palette()
            .upgrade(&BlockState::new(format!("minecraft:{name}")))
            .unwrap()
    }

    #[test]
    fn two_block_blocks_place_both_halves() {
        let world = World::new();
        let parts =
            placed_parts(&item("minecraft:sunflower"), GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].0, side(GROUND, support::UP));
        assert_eq!(get(&parts[0].1, "upper_block_bit"), &StateValue::Byte(0));
        assert_eq!(get(&parts[1].1, "upper_block_bit"), &StateValue::Byte(1));
        // Carpets use the bit for layers: one block.
        let carpet = placed_parts(
            &item("minecraft:pale_moss_carpet"),
            GROUND,
            &on_top(0.0),
            &world,
        );
        assert_eq!(carpet.unwrap().len(), 1);

        // A bed's head lies the way the player looks (south, at yaw 0).
        let bed = placed_parts(&block("bed"), GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(bed[1].0, side(GROUND, 3));
        assert_eq!(get(&bed[1].1, "head_piece_bit"), &StateValue::Byte(1));
    }

    #[test]
    fn doors_turn_with_their_hinge_and_pair_up() {
        let world = World::new();
        // Looking south: turned right, west.
        let door = placed_parts(&block("wooden_door"), GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(
            get(&door[0].1, "minecraft:cardinal_direction"),
            &string("west")
        );
        assert_eq!(get(&door[0].1, "door_hinge_bit"), &StateValue::Byte(0));
        assert_eq!(door[1].1.name, door[0].1.name);
        // A door with another on its left (east, looking south) hinges right.
        put(&world, side(GROUND, 5), &door[0].1);
        let pair = placed_state(&block("wooden_door"), GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(get(&pair, "door_hinge_bit"), &StateValue::Byte(1));
    }

    #[test]
    fn walls_rise_to_meet_a_block_above_and_lose_their_post() {
        let world = World::new();
        let wall_at = BlockPos { x: 0, y: -60, z: 0 };
        put(&world, side(wall_at, 4), &block("cobblestone"));
        put(&world, side(wall_at, 5), &block("cobblestone"));
        let wall = placed_state(
            &item("minecraft:cobblestone_wall"),
            wall_at,
            &on_top(0.0),
            &world,
        )
        .unwrap();
        assert_eq!(get(&wall, "wall_connection_type_east"), &string("short"));
        assert_eq!(
            get(&wall, "wall_post_bit"),
            &StateValue::Byte(0),
            "straight"
        );
        put(&world, wall_at, &wall);

        // A block above makes the connections tall.
        put(&world, side(wall_at, support::UP), &block("cobblestone"));
        let updated = neighbour_updates(side(wall_at, support::UP), &world);
        let (_, tall) = updated.iter().find(|(pos, _)| *pos == wall_at).unwrap();
        assert_eq!(get(tall, "wall_connection_type_east"), &string("tall"));
        assert_eq!(get(tall, "wall_connection_type_west"), &string("tall"));
        assert_eq!(get(tall, "wall_connection_type_north"), &string("none"));
        assert_eq!(get(tall, "wall_post_bit"), &StateValue::Byte(0));

        // A torch standing on it gives it a post.
        let torch = placed_state(
            &item("minecraft:torch"),
            side(wall_at, support::UP),
            &on_top(0.0),
            &world,
        )
        .unwrap();
        put(&world, side(wall_at, support::UP), &torch);
        let updated = neighbour_updates(side(wall_at, support::UP), &world);
        let (_, post) = updated.iter().find(|(pos, _)| *pos == wall_at).unwrap();
        assert_eq!(get(post, "wall_post_bit"), &StateValue::Byte(1));
        assert_eq!(get(post, "wall_connection_type_east"), &string("short"));
    }

    #[test]
    fn gates_lower_between_walls_and_open_away() {
        let world = World::new();
        put(&world, side(GROUND, 4), &block("cobblestone_wall"));
        // Looking south, the gate's sides are east and west.
        let gate = placed_state(&block("fence_gate"), GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(get(&gate, "in_wall_bit"), &StateValue::Byte(1));
        assert!(openable(&gate));
        // Opened from the other side (looking north), it swings north.
        let opened = toggled(&gate, 180.0);
        assert_eq!(get(&opened, "open_bit"), &StateValue::Byte(1));
        assert_eq!(
            get(&opened, "minecraft:cardinal_direction"),
            &string("north")
        );
        assert_eq!(
            get(&toggled(&opened, 0.0), "open_bit"),
            &StateValue::Byte(0)
        );
        assert!(!openable(&block("iron_door")), "iron needs redstone");
    }

    #[test]
    fn trapdoors_face_away_and_glazed_terracotta_turns_flat() {
        let world = World::new();
        // Facing away from the player: 5 minus that face.
        for (yaw, direction) in [(0.0, 3), (180.0, 2), (-90.0, 1), (90.0, 0)] {
            let trapdoor = placed_state(&block("trapdoor"), GROUND, &on_top(yaw), &world).unwrap();
            assert_eq!(
                get(&trapdoor, "direction"),
                &StateValue::Int(direction),
                "yaw {yaw}"
            );
        }
        // Looking steeply down, glazed terracotta still faces the player.
        let looking_down = Placing {
            pitch: 80.0,
            ..on_top(0.0)
        };
        let tile = placed_state(
            &item("minecraft:white_glazed_terracotta"),
            GROUND,
            &looking_down,
            &world,
        )
        .unwrap();
        assert_eq!(get(&tile, "facing_direction"), &StateValue::Int(2));
    }

    #[test]
    fn torches_hang_on_the_back_of_stairs() {
        let world = World::new();
        // Stairs placed looking east have their whole back to the east.
        let stairs_at = BlockPos { x: 1, y: -60, z: 0 };
        let stairs = placed_state(
            &item("minecraft:oak_stairs"),
            stairs_at,
            &on_top(-90.0),
            &world,
        )
        .unwrap();
        put(&world, stairs_at, &stairs);
        let behind = BlockPos { x: 2, y: -60, z: 0 };
        let on_back = Placing {
            face: 5,
            click: Vec3 {
                x: 1.0,
                y: 0.5,
                z: 0.5,
            },
            pitch: 0.0,
            yaw: 90.0,
        };
        let torch = placed_state(&item("minecraft:torch"), behind, &on_back, &world).unwrap();
        assert!(support::supported(&torch, behind, &world));
        // The open front is no wall.
        let front = BlockPos { x: 0, y: -60, z: 0 };
        let on_front = Placing { face: 4, ..on_back };
        let torch = placed_state(&item("minecraft:torch"), front, &on_front, &world).unwrap();
        assert!(!support::supported(&torch, front, &world));
    }

    #[test]
    fn wall_fixed_blocks_take_the_clicked_face() {
        let world = World::new();
        let against_north = Placing {
            face: 2,
            click: Vec3 {
                x: 0.5,
                y: 0.5,
                z: 0.0,
            },
            pitch: 0.0,
            yaw: 0.0,
        };
        let button = placed_state(
            &item("minecraft:stone_button"),
            GROUND,
            &against_north,
            &world,
        )
        .unwrap();
        assert_eq!(get(&button, "facing_direction"), &StateValue::Int(2));
        let lever = placed_state(&block("lever"), GROUND, &against_north, &world).unwrap();
        assert_eq!(get(&lever, "lever_direction"), &string("north"));
        let floor_lever = placed_state(&block("lever"), GROUND, &on_top(90.0), &world).unwrap();
        assert_eq!(
            get(&floor_lever, "lever_direction"),
            &string("up_east_west")
        );
        let vine = placed_state(&item("minecraft:vine"), GROUND, &against_north, &world).unwrap();
        // Facing north, so fixed to the block south of it.
        assert_eq!(get(&vine, "vine_direction_bits"), &StateValue::Int(1));
        let lantern = placed_state(
            &item("minecraft:lantern"),
            GROUND,
            &Placing {
                face: 0,
                ..against_north
            },
            &world,
        )
        .unwrap();
        assert_eq!(get(&lantern, "hanging"), &StateValue::Byte(1));
        let leaves =
            placed_state(&item("minecraft:oak_leaves"), GROUND, &on_top(0.0), &world).unwrap();
        assert_eq!(
            get(&leaves, "persistent_bit"),
            &StateValue::Byte(1),
            "placed leaves keep"
        );
    }
}

#[cfg(test)]
mod every_item {
    use super::*;
    use crate::items::{ItemType, items};
    use crate::support::{WithParts, supported};

    /// Where items are tried: a block to stand on (or hang from, or be fixed
    /// to), the face of it clicked, and whether there is water beside it.
    const SETUPS: [(&str, u8, bool); 9] = [
        ("grass_block", 1, false),
        ("farmland", 1, false),
        ("sand", 1, false),
        ("soul_sand", 1, false),
        ("stone", 1, false),
        ("stone", 2, false),
        ("stone", 0, false),
        ("jungle_log", 2, false),
        // Sugar cane needs water by its soil.
        ("sand", 1, true),
    ];

    /// Items that cannot be placed in any setup above, and why.
    const UNPLACEABLE: &[(&str, &str)] = &[
        ("kelp", "only grows in water"),
        ("seagrass", "only grows in water"),
        ("waterlily", "floats on water"),
        ("frog_spawn", "floats on water"),
    ];

    /// One world with every setup in it, each around its own target.
    fn scenes() -> (World, Vec<BlockPos>) {
        let world = World::new();
        let block = |name: &str| {
            palette()
                .upgrade(&BlockState::new(format!("minecraft:{name}")))
                .unwrap()
                .network_id()
        };
        let targets = SETUPS
            .iter()
            .enumerate()
            .map(|(i, (support, face, water))| {
                let target = BlockPos {
                    x: i as i32 * 8,
                    y: -50,
                    z: 0,
                };
                let support_at = side(target, opposite_face(*face));
                world.set_block(support_at, block(support));
                if *water {
                    world.set_block(side(support_at, 5), block("water"));
                }
                target
            })
            .collect();
        (world, targets)
    }

    /// Whether `item` places a block, with every part held up, in a setup.
    fn places(item: &ItemType, world: &World, target: BlockPos, face: u8) -> Result<(), String> {
        let state = items()
            .placed_block(item, (2..6).contains(&face))
            .ok_or("places nothing")?;
        let placing = Placing {
            face,
            click: Vec3 {
                x: 0.5,
                y: 0.5,
                z: 0.5,
            },
            pitch: 0.0,
            yaw: 0.0,
        };
        let parts = placed_parts(&state, target, &placing, world).map_err(|err| err.to_string())?;
        let placed = WithParts {
            world,
            parts: &parts,
        };
        match parts
            .iter()
            .find(|(pos, state)| !supported(state, *pos, &placed))
        {
            Some((_, state)) => Err(format!("{} is not held up", state.name)),
            None => Ok(()),
        }
    }

    #[test]
    fn every_item_that_places_a_block_can_be_placed_somewhere() {
        let (world, targets) = scenes();
        let mut failures = Vec::new();
        let mut tried = 0;
        for item in items().iter() {
            if items().placed_block(item, false).is_none() {
                continue;
            }
            tried += 1;
            let name = item.name.trim_start_matches("minecraft:");
            let errors: Vec<String> = SETUPS
                .iter()
                .zip(&targets)
                .filter_map(|((support, face, _), target)| {
                    places(item, &world, *target, *face)
                        .err()
                        .map(|err| format!("{support}/{face}: {err}"))
                })
                .collect();
            let placed = errors.len() < SETUPS.len();
            let excused = UNPLACEABLE
                .iter()
                .any(|(unplaceable, _)| *unplaceable == name);
            if placed == excused {
                failures.push(format!("{name}: {}", errors.join("; ")));
            }
        }
        assert!(tried > 1400, "{tried}");
        assert!(
            failures.is_empty(),
            "{} items:
{}",
            failures.len(),
            failures.join(
                "
"
            )
        );
    }
}
