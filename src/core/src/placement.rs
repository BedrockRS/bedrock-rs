//! The state a placed block takes: which way it faces, its axis, what it is
//! attached to, and which neighbours it connects to. Rules follow Dragonfly,
//! applied by the states a block has rather than block by block, so every
//! block with, say, `pillar_axis` gets the same treatment.
//!
//! Neighbour-dependent states (fence, pane, bar and wall connections, stairs
//! corners) are also recomputed for the blocks around a change.

use bedrockrs_protocol::block::{BlockState, StateValue};
use bedrockrs_protocol::types::{BlockPos, Vec3};

use crate::blocks::palette;
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
    #[error("{0} is not supported yet")]
    Unsupported(&'static str),
    #[error("it cannot hang from a ceiling")]
    OnCeiling,
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

fn opposite_face(face: u8) -> u8 {
    match face {
        0 => 1,
        1 => 0,
        2 => 3,
        3 => 2,
        4 => 5,
        _ => 4,
    }
}

fn side(pos: BlockPos, face: u8) -> BlockPos {
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
const ATTACHED: [&str; 8] = [
    "ladder",
    "end_rod",
    "lightning_rod",
    "amethyst_cluster",
    "_amethyst_bud",
    "wall_sign",
    "wall_banner",
    "tripwire_hook",
];

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

    for (key, value) in state.states.iter_mut() {
        let new = match key.as_str() {
            // Carpets use it for layering; tall plants need both halves.
            "upper_block_bit" if !name.contains("carpet") => {
                return Err(Refusal::Unsupported("placing two-block-tall blocks"));
            }
            "head_piece_bit" => return Err(Refusal::Unsupported("placing beds")),
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
            "minecraft:cardinal_direction" => StateValue::String(looking.opposite().name().into()),
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
            "facing_direction" | "minecraft:facing_direction" => {
                let face = if ATTACHED.iter().any(|part| name.contains(part)) {
                    placing.face
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
    Direction::ALL
        .into_iter()
        .filter_map(|direction| {
            let neighbour = side(pos, direction.face());
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
                *value = StateValue::String(if is(direction) { "short" } else { "none" }.into());
            } else if key == "wall_post_bit" {
                // A post, unless the wall runs straight through.
                let straight = (is(Direction::North)
                    && is(Direction::South)
                    && !is(Direction::East)
                    && !is(Direction::West))
                    || (is(Direction::East)
                        && is(Direction::West)
                        && !is(Direction::North)
                        && !is(Direction::South));
                *value = StateValue::Byte(u8::from(!straight));
            }
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

/// Whether a block has solid, full faces for others to connect to. There is
/// no shape data yet, so this rules out what is known not to be.
fn solid(name: &str) -> bool {
    const NOT_SOLID: [&str; 40] = [
        ":air",
        "_stairs",
        "_slab",
        "fence_gate",
        "_door",
        "trapdoor",
        "_sign",
        "_button",
        "pressure_plate",
        "carpet",
        "rail",
        "torch",
        "lantern",
        "_chain",
        ":chain",
        "ladder",
        "vine",
        "flower",
        "sapling",
        "tulip",
        "_grass",
        ":short_grass",
        ":tall_grass",
        "fern",
        ":snow_layer",
        "_bed",
        "banner",
        "_skull",
        "_head",
        "candle",
        "flower_pot",
        "cake",
        "water",
        "lava",
        ":chest",
        "trapped_chest",
        "ender_chest",
        "bell",
        "hopper",
        "_coral",
    ];
    !NOT_SOLID.iter().any(|part| name.contains(part))
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

    #[test]
    fn two_block_tall_blocks_are_refused_for_now() {
        let world = World::new();
        assert!(matches!(
            placed_state(&item("minecraft:sunflower"), GROUND, &on_top(0.0), &world),
            Err(Refusal::Unsupported(_))
        ));
        assert!(
            placed_state(
                &item("minecraft:pale_moss_carpet"),
                GROUND,
                &on_top(0.0),
                &world
            )
            .is_ok()
        );
    }
}
