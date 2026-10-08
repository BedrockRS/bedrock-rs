//! What a block needs to stay where it is: the block it stands on or hangs
//! from, the wall it is fixed to, the soil a plant grows in, or the other
//! half of a door, bed or tall flower. Placing a block checks it, and so does
//! every change next to a block: blocks left without support break.
//!
//! There is no block shape data, so as for connections, shapes are judged by
//! name: [`solid`] blocks have faces fences and walls connect to, and
//! [`full_cube`]s have every face sturdy enough to hang things on.

use std::collections::{HashMap, VecDeque};

use bedrockrs_protocol::block::{BlockState, StateValue};
use bedrockrs_protocol::types::BlockPos;

use crate::blocks::palette;
use crate::placement::{opposite_face, side};
use crate::world::World;

pub const DOWN: u8 = 0;
pub const UP: u8 = 1;

/// The blocks around a position, as support checks see them: the world, or
/// the world with a placement's blocks already in.
pub trait Blocks {
    fn at(&self, pos: BlockPos) -> Option<BlockState>;
}

impl Blocks for World {
    fn at(&self, pos: BlockPos) -> Option<BlockState> {
        self.block_state(pos).cloned()
    }
}

/// The world as it will be once `parts` are placed.
pub struct WithParts<'a> {
    pub world: &'a World,
    pub parts: &'a [(BlockPos, BlockState)],
}

impl Blocks for WithParts<'_> {
    fn at(&self, pos: BlockPos) -> Option<BlockState> {
        self.parts
            .iter()
            .find(|(at, _)| *at == pos)
            .map(|(_, state)| state.clone())
            .or_else(|| self.world.at(pos))
    }
}

/// A block's name without `minecraft:`.
pub fn short(name: &str) -> &str {
    name.strip_prefix("minecraft:").unwrap_or(name)
}

fn value<'a>(state: &'a BlockState, key: &str) -> Option<&'a StateValue> {
    state
        .states
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value)
}

pub(crate) fn int(state: &BlockState, key: &str) -> Option<i32> {
    match value(state, key)? {
        StateValue::Int(value) => Some(*value),
        StateValue::Byte(value) => Some(i32::from(*value)),
        StateValue::String(_) => None,
    }
}

pub(crate) fn text<'a>(state: &'a BlockState, key: &str) -> Option<&'a str> {
    match value(state, key)? {
        StateValue::String(value) => Some(value),
        _ => None,
    }
}

/// The face a direction name such as `north` stands for.
pub fn face_of(name: &str) -> Option<u8> {
    Some(match name {
        "down" => 0,
        "up" => 1,
        "north" => 2,
        "south" => 3,
        "west" => 4,
        "east" => 5,
        _ => return None,
    })
}

/// The face the legacy `direction` state (0 south, 1 west, 2 north, 3
/// east) stands for.
pub fn face_of_legacy(direction: i32) -> u8 {
    match direction.rem_euclid(4) {
        0 => 3,
        1 => 4,
        2 => 2,
        _ => 5,
    }
}

/// The face a wall coral fan's `coral_direction` points out of, as
/// PocketMine numbers it: 0 west, 1 east, 2 north, 3 south.
pub fn coral_facing(direction: i32) -> u8 {
    match direction {
        0 => 4,
        1 => 5,
        2 => 2,
        _ => 3,
    }
}

/// Plants that grow from soil, by name.
const PLANTS: &[&str] = &[
    "dandelion",
    "poppy",
    "blue_orchid",
    "allium",
    "azure_bluet",
    "red_tulip",
    "orange_tulip",
    "white_tulip",
    "pink_tulip",
    "oxeye_daisy",
    "cornflower",
    "lily_of_the_valley",
    "wither_rose",
    "torchflower",
    "closed_eyeblossom",
    "open_eyeblossom",
    "short_grass",
    "fern",
    "tall_grass",
    "large_fern",
    "sunflower",
    "lilac",
    "rose_bush",
    "peony",
    "pitcher_plant",
    "azalea",
    "flowering_azalea",
    "pink_petals",
    "wildflowers",
    "leaf_litter",
    "bush",
    "firefly_bush",
    "sweet_berry_bush",
    "mangrove_propagule",
    "deadbush",
    "short_dry_grass",
    "tall_dry_grass",
    "cactus_flower",
    "brown_mushroom",
    "red_mushroom",
    "crimson_fungus",
    "warped_fungus",
    "crimson_roots",
    "warped_roots",
    "nether_sprouts",
    "wheat",
    "carrots",
    "potatoes",
    "beetroot",
    "melon_stem",
    "pumpkin_stem",
    "torchflower_crop",
    "pitcher_crop",
    "nether_wart",
    "reeds",
    "cactus",
    "bamboo",
    "bamboo_sapling",
    "big_dripleaf",
    "small_dripleaf_block",
];

fn is_plant(name: &str) -> bool {
    PLANTS.contains(&name) || name.ends_with("_sapling")
}

/// Whether a block has solid, full faces for fences, panes and walls to
/// connect to. This rules out what is known not to.
pub fn solid(name: &str) -> bool {
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
    // Two slabs make a whole block.
    if palette().single_slab(name).is_some() {
        return true;
    }
    !NOT_SOLID.iter().any(|part| name.contains(part)) && !is_plant(short(name))
}

/// Whether a block fills its whole cube, so every face is sturdy: what
/// ladders and wall torches hang on. Solid blocks that are lower, thinner
/// or hollow are not.
pub fn full_cube(state: &BlockState) -> bool {
    const NOT_FULL: &[&str] = &[
        "_fence",
        "_wall",
        "_pane",
        "iron_bars",
        "end_rod",
        "lightning_rod",
        "cactus",
        "cauldron",
        "anvil",
        "enchanting_table",
        "brewing_stand",
        "daylight_detector",
        "grindstone",
        "stonecutter",
        "campfire",
        "composter",
        "cocoa",
        "dripleaf",
        "scaffolding",
        "farmland",
        "grass_path",
        "end_portal_frame",
        "conduit",
        "chorus_",
        "sea_pickle",
        "turtle_egg",
        "sniffer_egg",
        "amethyst_cluster",
        "_amethyst_bud",
        "pointed_dripstone",
        "sulfur_spike",
        "frame",
        "web",
        "spore_blossom",
        "hanging_roots",
        "glow_lichen",
        "sculk_vein",
        "resin_clump",
        "decorated_pot",
        "frog_spawn",
        "heavy_core",
        "_shelf",
        "lectern",
        "dragon_egg",
        "redstone_wire",
        "repeater",
        "comparator",
        "lever",
        "tripwire_hook",
        "trip_wire",
        "piston_arm",
        "waterlily",
        "kelp",
        "seagrass",
        "light_block",
        "structure_void",
        "border_block",
        "moving_block",
        "dried_ghast",
        "golem_statue",
        "pale_hanging_moss",
        "bubble_column",
    ];
    const NOT_FULL_EXACT: &[&str] = &["fire", "soul_fire", "barrier"];
    let name = short(&state.name);
    solid(&state.name)
        && !NOT_FULL.iter().any(|part| name.contains(part))
        && !NOT_FULL_EXACT.contains(&name)
}

/// Whether the face of `state` toward `toward` (the face of the block, 0
/// to 5) is sturdy: all of a full cube's, and the full sides of half blocks
/// (the top of a top slab or upside-down stairs, the bottom of the others).
pub fn sturdy(state: &BlockState, toward: u8) -> bool {
    if full_cube(state) {
        return true;
    }
    let name = short(&state.name);
    let top_half = text(state, "minecraft:vertical_half") == Some("top")
        || int(state, "upside_down_bit") == Some(1);
    let half_block = name.ends_with("_slab") || name.ends_with("_stairs");
    match toward {
        UP => half_block && top_half,
        DOWN => half_block && !top_half,
        // A stairs block's back is whole, unless it is an outer corner.
        side if name.ends_with("_stairs") => {
            let back = match int(state, "weirdo_direction") {
                Some(0) => 5,
                Some(1) => 4,
                Some(2) => 3,
                _ => 2,
            };
            let outer =
                text(state, "minecraft:corner").is_some_and(|corner| corner.starts_with("outer"));
            side == back && !outer
        }
        _ => false,
    }
}

/// Whether a block can hold something small on top of its centre, such as a
/// torch, lantern or candle: a sturdy top, or the post of a fence or wall.
fn holds_centre(state: &BlockState) -> bool {
    let name = short(&state.name);
    sturdy(state, UP)
        || name.ends_with("_fence")
        || name.ends_with("_wall")
        || name.contains("_pane")
        || name == "iron_bars"
        || name.ends_with("chain")
}

/// Whether something can hang under a block: anything solid, or a chain,
/// fence or wall it can hang from.
fn holds_hanging(state: &BlockState) -> bool {
    solid(&state.name) || holds_centre(state) || sturdy(state, DOWN)
}

fn is_air(state: &Option<BlockState>) -> bool {
    state
        .as_ref()
        .is_none_or(|state| state.name == "minecraft:air")
}

fn is_liquid(state: &BlockState) -> bool {
    matches!(
        short(&state.name),
        "water" | "flowing_water" | "lava" | "flowing_lava"
    )
}

/// Blocks another can be placed into, replacing them: air, liquids, fire
/// and the small plants and growths a block simply goes over.
pub fn replaceable(state: &BlockState) -> bool {
    matches!(
        short(&state.name),
        "air"
            | "water"
            | "flowing_water"
            | "lava"
            | "flowing_lava"
            | "fire"
            | "soul_fire"
            | "short_grass"
            | "fern"
            | "deadbush"
            | "short_dry_grass"
            | "seagrass"
            | "vine"
            | "glow_lichen"
            | "sculk_vein"
            | "resin_clump"
            | "nether_sprouts"
            | "crimson_roots"
            | "warped_roots"
            | "bush"
            | "structure_void"
    ) || short(&state.name).starts_with("light_block")
}

/// The soils a plant grows in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Soil {
    /// Grass, dirt and the like.
    Dirt,
    /// Farmland: crops.
    Farmland,
    /// Soul sand: nether wart.
    SoulSand,
    /// Nylium and soul soil, or dirt: fungi and roots.
    Nether,
    /// Sand, terracotta or dirt: dead bushes and dry grass.
    Dry,
    /// Dirt or clay: azaleas, propagules and dripleaves.
    Lush,
    /// Any full block: mushrooms, leaf litter.
    Any,
}

impl Soil {
    fn of(name: &str) -> Option<Self> {
        if !is_plant(name) {
            return None;
        }
        Some(match name {
            "wheat" | "carrots" | "potatoes" | "beetroot" | "melon_stem" | "pumpkin_stem"
            | "torchflower_crop" | "pitcher_crop" => Self::Farmland,
            "nether_wart" => Self::SoulSand,
            "crimson_fungus" | "warped_fungus" | "crimson_roots" | "warped_roots"
            | "nether_sprouts" => Self::Nether,
            "deadbush" | "short_dry_grass" | "tall_dry_grass" => Self::Dry,
            "azalea"
            | "flowering_azalea"
            | "mangrove_propagule"
            | "big_dripleaf"
            | "small_dripleaf_block" => Self::Lush,
            "brown_mushroom" | "red_mushroom" | "leaf_litter" | "cactus_flower" => Self::Any,
            // Their own rules, below.
            "reeds" | "cactus" | "bamboo" | "bamboo_sapling" => return None,
            _ => Self::Dirt,
        })
    }

    fn grows_on(self, below: &BlockState) -> bool {
        let name = short(&below.name);
        let dirt = matches!(
            name,
            "grass_block"
                | "dirt"
                | "coarse_dirt"
                | "podzol"
                | "rooted_dirt"
                | "dirt_with_roots"
                | "moss_block"
                | "pale_moss_block"
                | "mud"
                | "muddy_mangrove_roots"
                | "mycelium"
                | "farmland"
        );
        match self {
            Self::Dirt => dirt,
            Self::Farmland => name == "farmland",
            Self::SoulSand => name == "soul_sand",
            Self::Nether => {
                dirt || matches!(name, "crimson_nylium" | "warped_nylium" | "soul_soil")
            }
            Self::Dry => {
                dirt || matches!(name, "sand" | "red_sand" | "suspicious_sand")
                    || name == "hardened_clay"
                    || (name.ends_with("terracotta") && !name.contains("glazed"))
            }
            Self::Lush => dirt || name == "clay",
            Self::Any => full_cube(below),
        }
    }
}

/// The other half of a two-block block (a door, a tall flower, a bed), if
/// `state` is one: where it is, and whether `state` is the half that drops
/// the item when broken.
pub fn partner(state: &BlockState, pos: BlockPos) -> Option<(BlockPos, bool)> {
    let name = short(&state.name);
    if let Some(head) = int(state, "head_piece_bit") {
        let facing = face_of_legacy(int(state, "direction")?);
        // The head lies in the direction the bed faces.
        return Some(if head == 1 {
            (side(pos, opposite_face(facing)), false)
        } else {
            (side(pos, facing), true)
        });
    }
    let upper = int(state, "upper_block_bit")?;
    // Carpets use the bit for layering, not halves.
    if name.contains("carpet") {
        return None;
    }
    if upper == 1 {
        Some((side(pos, DOWN), false))
    } else if name == "pitcher_crop" {
        // A young pitcher crop is one block tall.
        None
    } else {
        Some((side(pos, UP), true))
    }
}

/// Whether `other` is the other half of `state`: the same block, the other
/// half, the same way round.
fn is_partner(state: &BlockState, other: &BlockState) -> bool {
    if other.name != state.name {
        return false;
    }
    let flipped = |key| int(state, key).zip(int(other, key)).map(|(a, b)| a != b);
    let same = |key| int(state, key) == int(other, key);
    if int(state, "head_piece_bit").is_some() {
        return flipped("head_piece_bit") == Some(true) && same("direction");
    }
    flipped("upper_block_bit") == Some(true)
}

/// Whether a broken block drops its item: a two-block block only drops it
/// from one half.
pub fn drops_item(state: &BlockState) -> bool {
    partner(state, BlockPos::default()).is_none_or(|(_, drops)| drops)
}

/// The faces of `multi_face_direction_bits` (glow lichen, sculk vein, resin
/// clump), as `(bit, face)`: down 1, up 2, south 4, west 8, north 16, east
/// 32. Each face is the side of the block it lies on.
pub(crate) const MULTI_FACES: [(i32, u8); 6] = [(1, 0), (2, 1), (4, 3), (8, 4), (16, 2), (32, 5)];

/// The faces of `vine_direction_bits`, as `(bit, face)`: south 1, west 2,
/// north 4, east 8.
pub(crate) const VINE_FACES: [(i32, u8); 4] = [(1, 3), (2, 4), (4, 2), (8, 5)];

/// For a block made of faces (vines, glow lichen and the like): its state
/// key, the faces it has and those of them something holds. A face of glow
/// lichen needs a sturdy face behind it; a vine's side, a sturdy face or the
/// same side of a vine above it, as vines hang down walls.
fn held_faces(
    state: &BlockState,
    pos: BlockPos,
    blocks: &impl Blocks,
) -> Option<(&'static str, i32, i32)> {
    let sturdy_at = |face: u8| {
        blocks
            .at(side(pos, face))
            .is_some_and(|block| sturdy(&block, opposite_face(face)))
    };
    let (key, faces): (&'static str, &[(i32, u8)]) =
        if let Some(bits) = int(state, "vine_direction_bits") {
            let above = blocks
                .at(side(pos, UP))
                .filter(|above| above.name == state.name)
                .and_then(|above| int(&above, "vine_direction_bits"))
                .unwrap_or(0);
            let held = VINE_FACES
                .iter()
                .filter(|(bit, face)| bits & bit != 0 && (above & bit != 0 || sturdy_at(*face)))
                .fold(0, |held, (bit, _)| held | bit);
            return Some(("vine_direction_bits", bits, held));
        } else if int(state, "multi_face_direction_bits").is_some() {
            ("multi_face_direction_bits", &MULTI_FACES)
        } else {
            return None;
        };
    let bits = int(state, key)?;
    let held = faces
        .iter()
        .filter(|(bit, face)| bits & bit != 0 && sturdy_at(*face))
        .fold(0, |held, (bit, _)| held | bit);
    Some((key, bits, held))
}

/// The most scaffolding a search for the ground looks through, so a huge
/// structure costs a bounded amount; past it, the neighbours' stabilities
/// are trusted instead.
const SCAFFOLDING_SEARCH: usize = 4096;

/// How far scaffolding at `pos` is from a column standing on the ground,
/// as vanilla's `stability` (its distance): the fewest sideways steps
/// through scaffolding to scaffolding standing on a sturdy floor, going
/// down columns for free. 7 is too far.
///
/// Vanilla works this out from the neighbours' stored stabilities, so when
/// a column goes, what hung from it counts up a step a tick before falling;
/// a search through the structure finds at once that nothing holds it, so
/// it falls block by block outwards from where it was cut (found live on
/// 2026-10-08, when a bridge stood until a whole layer fell at once).
pub fn scaffolding_stability(pos: BlockPos, blocks: &impl Blocks) -> i32 {
    let is_scaffolding = |pos: BlockPos| {
        blocks
            .at(pos)
            .is_some_and(|block| short(&block.name) == "scaffolding")
    };
    // Steps down cost nothing and sideways one, so the queue keeps the
    // nearest first: down steps go to its front.
    let mut best: HashMap<BlockPos, i32> = HashMap::from([(pos, 0)]);
    let mut queue = VecDeque::from([(pos, 0)]);
    while let Some((at, steps)) = queue.pop_front() {
        if best.get(&at).is_some_and(|known| *known < steps) {
            continue;
        }
        if best.len() > SCAFFOLDING_SEARCH {
            return local_stability(pos, blocks);
        }
        let below = side(at, DOWN);
        if is_scaffolding(below) {
            if best.get(&below).is_none_or(|known| *known > steps) {
                best.insert(below, steps);
                queue.push_front((below, steps));
            }
        } else if blocks.at(below).is_some_and(|block| sturdy(&block, UP)) {
            return steps;
        }
        if steps + 1 >= 7 {
            continue;
        }
        for face in 2..6 {
            let beside = side(at, face);
            if is_scaffolding(beside) && best.get(&beside).is_none_or(|known| *known > steps + 1) {
                best.insert(beside, steps + 1);
                queue.push_back((beside, steps + 1));
            }
        }
    }
    7
}

/// Vanilla's stability from the neighbours' stored ones: that of the
/// scaffolding under it, 0 on a sturdy floor, or one more than the nearest
/// beside it.
fn local_stability(pos: BlockPos, blocks: &impl Blocks) -> i32 {
    let stored = |block: &BlockState| {
        (short(&block.name) == "scaffolding").then(|| int(block, "stability").unwrap_or(7))
    };
    let mut stability = 7;
    if let Some(below) = blocks.at(side(pos, DOWN)) {
        match stored(&below) {
            Some(below) => stability = below,
            None if sturdy(&below, UP) => return 0,
            None => {}
        }
    }
    for face in 2..6 {
        if let Some(beside) = blocks.at(side(pos, face)).as_ref().and_then(stored) {
            stability = stability.min(beside + 1);
        }
    }
    stability.min(7)
}

/// Blocks that fall when nothing is under them: sand, gravel, concrete
/// powder, anvils, the dragon egg.
pub fn falls(name: &str) -> bool {
    let name = short(name);
    matches!(
        name,
        "sand"
            | "red_sand"
            | "gravel"
            | "suspicious_sand"
            | "suspicious_gravel"
            | "anvil"
            | "chipped_anvil"
            | "damaged_anvil"
            | "dragon_egg"
    ) || name.ends_with("_concrete_powder")
}

/// Whether a falling block passes through `state`: air, liquids, fire and
/// what blocks simply replace.
pub fn falls_through(state: &BlockState) -> bool {
    replaceable(state)
}

/// What becomes of a block when its neighbours change.
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    /// It stays, as this state: itself, a vine or lichen without the faces
    /// nothing holds any more, or scaffolding with its new stability.
    Stays(BlockState),
    /// Nothing holds it: it breaks, dropping its item.
    Pops,
    /// It falls as a falling block: sand over air, or scaffolding placed
    /// too far out (vanilla drops it if it was steady before, and lets it
    /// fall if it was already too far).
    Falls,
}

/// What the block `state` at `pos` becomes once its neighbours changed.
pub fn settled(state: &BlockState, pos: BlockPos, blocks: &impl Blocks) -> Settled {
    if let Some((key, bits, held)) = held_faces(state, pos, blocks) {
        if held == 0 {
            return Settled::Pops;
        }
        let mut settled = state.clone();
        if held != bits
            && let Some((_, value)) = settled.states.iter_mut().find(|(name, _)| name == key)
        {
            *value = StateValue::Int(held);
        }
        return Settled::Stays(settled);
    }
    if short(&state.name) == "scaffolding" {
        let stability = scaffolding_stability(pos, blocks);
        if stability >= 7 {
            return if int(state, "stability") == Some(7) {
                Settled::Falls
            } else {
                Settled::Pops
            };
        }
        let mut settled = state.clone();
        if let Some((_, value)) = settled
            .states
            .iter_mut()
            .find(|(name, _)| name == "stability")
        {
            *value = StateValue::Int(stability);
        }
        return Settled::Stays(settled);
    }
    if falls(&state.name)
        && blocks
            .at(side(pos, DOWN))
            .is_some_and(|below| falls_through(&below))
    {
        return Settled::Falls;
    }
    if supported(state, pos, blocks) {
        Settled::Stays(state.clone())
    } else {
        Settled::Pops
    }
}

/// Whether the block `state` at `pos` has what it needs around it.
pub fn supported(state: &BlockState, pos: BlockPos, blocks: &impl Blocks) -> bool {
    let name = short(&state.name);
    // Every face of a vine or lichen is held, and it has one.
    if let Some((_, bits, held)) = held_faces(state, pos, blocks) {
        return bits != 0 && held == bits;
    }
    if name == "scaffolding" {
        return scaffolding_stability(pos, blocks) < 7;
    }
    let at = |face: u8| blocks.at(side(pos, face));
    let below = || at(DOWN);
    let above = || at(UP);
    let sturdy_at = |face: u8| at(face).is_some_and(|block| sturdy(&block, opposite_face(face)));
    let solid_at = |face: u8| at(face).is_some_and(|block| solid(&block.name));

    if let Some((other_pos, main)) = partner(state, pos) {
        let paired = blocks
            .at(other_pos)
            .is_some_and(|other| is_partner(state, &other));
        if !paired {
            return false;
        }
        // The upper half and the bed head stand on the other half.
        if !main || int(state, "head_piece_bit").is_some() {
            return true;
        }
    }

    // Torches: on top of a block, or on a side.
    if let Some(facing) = text(state, "torch_facing_direction") {
        return match facing {
            "top" | "unknown" => below().is_some_and(|block| holds_centre(&block)),
            facing => face_of(facing).is_some_and(sturdy_at),
        };
    }
    if let Some(direction) = text(state, "lever_direction") {
        let attached = match direction.split('_').next() {
            Some("up") => DOWN,
            Some("down") => UP,
            Some(facing) => face_of(facing).map_or(DOWN, opposite_face),
            None => DOWN,
        };
        return sturdy_at(attached);
    }
    // Fixed to the face of the block behind them.
    let facing = int(state, "facing_direction").and_then(|face| u8::try_from(face).ok());
    if let Some(facing) = facing {
        let behind = opposite_face(facing);
        let on_wall = (2..6).contains(&facing);
        if name == "ladder" {
            return on_wall && sturdy_at(behind);
        }
        if name.ends_with("_button") {
            return sturdy_at(behind);
        }
        if name.contains("wall_sign") {
            return on_wall && solid_at(behind);
        }
        if name.ends_with("frame") {
            return solid_at(behind);
        }
    }
    if name == "wall_banner" {
        return int(state, "facing_direction")
            .and_then(|face| u8::try_from(face).ok())
            .is_some_and(|facing| (2..6).contains(&facing) && solid_at(opposite_face(facing)));
    }
    if let Some(face) = text(state, "minecraft:block_face").and_then(face_of) {
        return sturdy_at(opposite_face(face));
    }
    // Facing away from the block they are fixed to.
    if name == "tripwire_hook" {
        let facing = face_of_legacy(int(state, "direction").unwrap_or(0));
        return sturdy_at(opposite_face(facing));
    }
    if let Some(direction) = int(state, "coral_direction") {
        return sturdy_at(opposite_face(coral_facing(direction)));
    }
    // Cocoa faces the jungle log it grows on.
    if name == "cocoa" {
        let log = face_of_legacy(int(state, "direction").unwrap_or(0));
        return at(log).is_some_and(|block| {
            let log = short(&block.name);
            log.contains("jungle_log") || log.contains("jungle_wood")
        });
    }

    // Hanging from the block above, or standing on the one below.
    if let Some(hanging) = int(state, "hanging") {
        let lantern = name.ends_with("lantern");
        let sign = name.ends_with("hanging_sign");
        let dripstone = name == "pointed_dripstone" || name == "sulfur_spike";
        if hanging == 1 && (lantern || sign) {
            return above().is_some_and(|block| holds_hanging(&block));
        }
        if hanging == 0 && lantern {
            return below().is_some_and(|block| holds_centre(&block));
        }
        if dripstone {
            let (support, toward) = if hanging == 1 { (UP, DOWN) } else { (DOWN, UP) };
            return at(support)
                .is_some_and(|block| block.name == state.name || sturdy(&block, toward));
        }
    }
    if let Some(attachment) = text(state, "attachment")
        && name == "bell"
    {
        return match attachment {
            "standing" => !is_air(&below()),
            "hanging" => !is_air(&above()),
            _ => true,
        };
    }
    if name.starts_with("cave_vines") || name == "weeping_vines" {
        return above().is_some_and(|block| block.name.contains(name) || sturdy(&block, DOWN))
            || (name.starts_with("cave_vines")
                && above().is_some_and(|block| short(&block.name).starts_with("cave_vines")));
    }
    if matches!(
        name,
        "spore_blossom" | "hanging_roots" | "pale_hanging_moss"
    ) {
        return above().is_some_and(|block| block.name == state.name || sturdy(&block, DOWN));
    }
    if name == "kelp" {
        return below().is_some_and(|block| block.name == state.name || sturdy(&block, UP));
    }
    if name == "twisting_vines" {
        return below().is_some_and(|block| block.name == state.name || sturdy(&block, UP));
    }
    if matches!(name, "waterlily" | "frog_spawn") {
        return below().is_some_and(|block| is_liquid(&block) && block.name.contains("water"));
    }

    // Standing on the block below.
    let on_floor = name.ends_with("_door")
        || name.contains("rail")
        || name == "redstone_wire"
        || name.ends_with("repeater")
        || name.ends_with("comparator")
        || name == "snow_layer"
        || name == "sea_pickle"
        || name.ends_with("coral_fan");
    if on_floor {
        return sturdy_at(DOWN);
    }
    let on_centre = name.contains("pressure_plate")
        || (name.ends_with("candle") && !name.contains("cake"))
        || name.ends_with("_candle");
    if on_centre {
        return below().is_some_and(|block| holds_centre(&block));
    }
    if name.contains("standing_sign") || name == "standing_banner" {
        return solid_at(DOWN);
    }
    if name.contains("carpet") || name.ends_with("cake") || name == "turtle_egg" {
        let below = below();
        return !is_air(&below) && !below.as_ref().is_some_and(is_liquid);
    }

    // Plants and their soils.
    match name {
        "reeds" => {
            let Some(soil) = below() else { return false };
            if soil.name == state.name {
                return true;
            }
            let ground = matches!(
                short(&soil.name),
                "grass_block" | "dirt" | "coarse_dirt" | "podzol" | "mud" | "sand" | "red_sand"
            );
            let soil_pos = side(pos, DOWN);
            ground
                && (2..6).any(|face| {
                    blocks
                        .at(side(soil_pos, face))
                        .is_some_and(|block| short(&block.name).contains("water"))
                })
        }
        "cactus" => {
            let on_sand = below()
                .is_some_and(|block| matches!(short(&block.name), "sand" | "red_sand" | "cactus"));
            on_sand && (2..6).all(|face| !solid_at(face))
        }
        "bamboo" | "bamboo_sapling" => below().is_some_and(|block| {
            short(&block.name).starts_with("bamboo")
                || Soil::Dirt.grows_on(&block)
                || matches!(short(&block.name), "sand" | "red_sand" | "gravel")
        }),
        _ => match Soil::of(name) {
            Some(Soil::Lush)
                if name == "mangrove_propagule" && int(state, "hanging") == Some(1) =>
            {
                above().is_some_and(|block| short(&block.name) == "mangrove_leaves")
            }
            Some(Soil::Lush) if name == "big_dripleaf" => {
                below().is_some_and(|block| block.name == state.name || Soil::Lush.grows_on(&block))
            }
            Some(soil) => below().is_some_and(|block| soil.grows_on(&block)),
            None => true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(name: &str) -> BlockState {
        palette()
            .upgrade(&BlockState::new(format!("minecraft:{name}")))
            .unwrap()
    }

    fn with(state: BlockState, key: &str, value: StateValue) -> BlockState {
        let mut state = state;
        if let Some((_, slot)) = state.states.iter_mut().find(|(name, _)| name == key) {
            *slot = value;
        }
        state
    }

    const AT: BlockPos = BlockPos { x: 0, y: -50, z: 0 };

    fn put(world: &World, pos: BlockPos, state: &BlockState) {
        world.set_block(pos, state.network_id());
    }

    #[test]
    fn shapes_are_judged_by_name() {
        assert!(full_cube(&state("stone")));
        assert!(full_cube(&state("oak_double_slab")));
        assert!(solid("minecraft:oak_double_slab"));
        assert!(!full_cube(&state("cobblestone_wall")));
        assert!(!full_cube(&state("ladder")));
        assert!(!solid("minecraft:poppy"), "flowers are not solid");
        let top = with(
            state("oak_slab"),
            "minecraft:vertical_half",
            StateValue::String("top".into()),
        );
        assert!(sturdy(&top, UP) && !sturdy(&top, DOWN));
        assert!(replaceable(&state("short_grass")) && !replaceable(&state("stone")));
    }

    #[test]
    fn ladders_need_a_full_block_behind() {
        let world = World::new();
        // Facing south: fixed to the block on its north.
        let ladder = with(state("ladder"), "facing_direction", StateValue::Int(3));
        assert!(!supported(&ladder, AT, &world), "nothing behind");
        put(&world, side(AT, 2), &state("stone"));
        assert!(supported(&ladder, AT, &world));
        put(&world, side(AT, 2), &ladder);
        assert!(!supported(&ladder, AT, &world), "not on another ladder");
        put(&world, side(AT, 2), &state("oak_fence"));
        assert!(!supported(&ladder, AT, &world), "not on a fence");
    }

    #[test]
    fn doors_need_the_floor_and_both_halves() {
        let world = World::new();
        let lower = state("wooden_door");
        let upper = with(lower.clone(), "upper_block_bit", StateValue::Byte(1));
        let parts = [(AT, lower.clone()), (side(AT, UP), upper.clone())];
        let placing = WithParts {
            world: &world,
            parts: &parts,
        };
        assert!(!supported(&lower, AT, &placing), "in the air");
        put(&world, side(AT, DOWN), &state("stone"));
        assert!(supported(&lower, AT, &placing));
        assert!(supported(&upper, side(AT, UP), &placing));
        // Without its other half, neither stays.
        assert!(!supported(&lower, AT, &world));
        assert!(!supported(&upper, side(AT, UP), &world));
        assert!(drops_item(&lower) && !drops_item(&upper));
    }

    #[test]
    fn plants_need_their_soil() {
        let world = World::new();
        let poppy = state("poppy");
        put(&world, side(AT, DOWN), &state("stone"));
        assert!(!supported(&poppy, AT, &world));
        put(&world, side(AT, DOWN), &state("grass_block"));
        assert!(supported(&poppy, AT, &world));
        assert!(
            !supported(&state("wheat"), AT, &world),
            "crops need farmland"
        );
        put(&world, side(AT, DOWN), &state("farmland"));
        assert!(supported(&state("wheat"), AT, &world));
    }

    #[test]
    fn torches_stand_on_posts_and_hang_on_walls() {
        let world = World::new();
        let torch = state("torch");
        let on_top = with(
            torch.clone(),
            "torch_facing_direction",
            StateValue::String("top".into()),
        );
        put(&world, side(AT, DOWN), &state("cobblestone_wall"));
        assert!(supported(&on_top, AT, &world), "on a wall post");
        let on_wall = with(
            torch,
            "torch_facing_direction",
            StateValue::String("west".into()),
        );
        assert!(!supported(&on_wall, AT, &world));
        put(&world, side(AT, 4), &state("stone"));
        assert!(supported(&on_wall, AT, &world));
    }
}
