//! The collision boxes of blocks: what a player's body cannot share with a
//! block. Placing a block refuses only if a box of it would be inside a
//! player, so a fence post or pane beside someone's body, or a torch at their
//! feet, goes in, while a stone block does not.
//!
//! As with the support rules in [`crate::support`], there is no shape data, so boxes are
//! worked out from names and states, following vanilla's shapes.

use bedrockrs_protocol::block::BlockState;
use bedrockrs_protocol::types::{BlockPos, Vec3};

use crate::placement::opposite_face;
use crate::support::{self, int, short, text};

/// A box inside a block, each axis 0 to 1 (fences and walls reach 1.5 up).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

const fn aabb(min: [f32; 3], max: [f32; 3]) -> Aabb {
    Aabb { min, max }
}

const FULL: Aabb = aabb([0.0; 3], [1.0; 3]);

/// Sixteenths of a block.
const fn px(n: f32) -> f32 {
    n / 16.0
}

/// Blocks a body passes through: torches, plants, rails, signs and the like.
/// Doors too: they can go where a player stands.
fn passable(state: &BlockState) -> bool {
    const PASSABLE: &[&str] = &[
        "torch",
        "lever",
        "_button",
        "pressure_plate",
        "_sign",
        "banner",
        "rail",
        "redstone_wire",
        "trip_wire",
        "tripwire_hook",
        "vine",
        "glow_lichen",
        "sculk_vein",
        "resin_clump",
        ":web",
        ":fire",
        ":soul_fire",
        "light_block",
        "structure_void",
        "frame",
        "coral_fan",
        ":kelp",
        "seagrass",
        "reeds",
        "sweet_berry_bush",
        "spore_blossom",
        "hanging_roots",
        "pale_hanging_moss",
        "leaf_litter",
        "pink_petals",
        "wildflowers",
        "_door",
        ":air",
        "water",
        "lava",
        "bubble_column",
        "nether_portal",
        ":portal",
        "end_portal",
        "end_gateway",
    ];
    let name = state.name.as_str();
    PASSABLE.iter().any(|part| name.contains(part)) || is_soft(short(name))
}

/// Plants and growths with no collision.
fn is_soft(name: &str) -> bool {
    name.ends_with("_sapling")
        || name.contains("flower")
        || name.contains("tulip")
        || name.contains("grass")
        || name.contains("fern")
        || name.contains("mushroom")
        || name.contains("fungus")
        || name.contains("roots")
        || name.ends_with("_stem")
        || matches!(
            name,
            "dandelion"
                | "poppy"
                | "blue_orchid"
                | "allium"
                | "azure_bluet"
                | "oxeye_daisy"
                | "cornflower"
                | "lily_of_the_valley"
                | "wither_rose"
                | "closed_eyeblossom"
                | "open_eyeblossom"
                | "lilac"
                | "rose_bush"
                | "peony"
                | "pitcher_plant"
                | "pitcher_crop"
                | "deadbush"
                | "bush"
                | "firefly_bush"
                | "nether_sprouts"
                | "wheat"
                | "carrots"
                | "potatoes"
                | "beetroot"
                | "nether_wart"
                | "torchflower_crop"
                | "cave_vines"
                | "cave_vines_body_with_berries"
                | "cave_vines_head_with_berries"
                | "weeping_vines"
                | "twisting_vines"
                | "small_dripleaf_block"
                | "mangrove_propagule"
                | "bamboo_sapling"
        )
}

/// The boxes `connected` sides of a post reach out to: arms `half` wide
/// either side of the middle and `height` tall, from the post to the edge.
fn arms(state: &BlockState, key: &dyn Fn(&str) -> String, half: f32, height: f32) -> Vec<Aabb> {
    let (low, high) = (0.5 - half, 0.5 + half);
    let connected = |side: &str| {
        let key = key(side);
        match text(state, &key) {
            Some(kind) => kind != "none",
            None => int(state, &key) == Some(1),
        }
    };
    let mut boxes = Vec::new();
    if connected("north") {
        boxes.push(aabb([low, 0.0, 0.0], [high, height, 0.5]));
    }
    if connected("south") {
        boxes.push(aabb([low, 0.0, 0.5], [high, height, 1.0]));
    }
    if connected("west") {
        boxes.push(aabb([0.0, 0.0, low], [0.5, height, high]));
    }
    if connected("east") {
        boxes.push(aabb([0.5, 0.0, low], [1.0, height, high]));
    }
    boxes
}

/// The collision boxes of `state`, inside its block.
pub fn collision(state: &BlockState) -> Vec<Aabb> {
    let name = short(&state.name);
    if passable(state) {
        return Vec::new();
    }
    let top_half = text(state, "minecraft:vertical_half") == Some("top")
        || int(state, "upside_down_bit") == Some(1);
    let facing = int(state, "facing_direction").and_then(|face| u8::try_from(face).ok());

    if name.ends_with("_slab") && support::solid(&state.name) {
        return vec![FULL];
    }
    if name.ends_with("_slab") {
        return vec![if top_half {
            aabb([0.0, 0.5, 0.0], [1.0, 1.0, 1.0])
        } else {
            aabb([0.0, 0.0, 0.0], [1.0, 0.5, 1.0])
        }];
    }
    if name.ends_with("trapdoor") {
        if int(state, "open_bit") == Some(1) {
            // Upright, on the side away from where it faces: `direction`
            // is 5 minus the face it faces.
            let facing = u8::try_from(5 - int(state, "direction").unwrap_or(0)).unwrap_or(2);
            return vec![side_slab(opposite_face(facing), px(3.0))];
        }
        return vec![if top_half {
            aabb([0.0, px(13.0), 0.0], [1.0, 1.0, 1.0])
        } else {
            aabb([0.0, 0.0, 0.0], [1.0, px(3.0), 1.0])
        }];
    }
    if name.ends_with("fence_gate") {
        if int(state, "open_bit") == Some(1) {
            return Vec::new();
        }
        let across_x = matches!(
            text(state, "minecraft:cardinal_direction"),
            Some("north" | "south")
        );
        return vec![if across_x {
            aabb([0.0, 0.0, px(6.0)], [1.0, 1.5, px(10.0)])
        } else {
            aabb([px(6.0), 0.0, 0.0], [px(10.0), 1.5, 1.0])
        }];
    }
    if name.ends_with("_fence") {
        let mut boxes = vec![aabb([px(6.0), 0.0, px(6.0)], [px(10.0), 1.5, px(10.0)])];
        boxes.extend(arms(
            state,
            &|side| format!("minecraft:connection_{side}"),
            px(2.0),
            1.5,
        ));
        return boxes;
    }
    if name.ends_with("_wall") {
        let mut boxes = vec![aabb([px(4.0), 0.0, px(4.0)], [px(12.0), 1.5, px(12.0)])];
        boxes.extend(arms(
            state,
            &|side| format!("wall_connection_type_{side}"),
            px(3.0),
            1.5,
        ));
        return boxes;
    }
    if name.contains("_pane") || name == "iron_bars" {
        let mut boxes = vec![aabb([px(7.0), 0.0, px(7.0)], [px(9.0), 1.0, px(9.0)])];
        boxes.extend(arms(
            state,
            &|side| format!("minecraft:connection_{side}"),
            px(1.0),
            1.0,
        ));
        return boxes;
    }
    if name.ends_with("chain") || name.ends_with("_rod") {
        let (low, high) = (px(6.5), px(9.5));
        let axis = text(state, "pillar_axis")
            .map(str::to_owned)
            .unwrap_or_else(|| {
                match facing {
                    Some(0 | 1) | None => "y",
                    Some(2 | 3) => "z",
                    _ => "x",
                }
                .to_owned()
            });
        return vec![match axis.as_str() {
            "x" => aabb([0.0, low, low], [1.0, high, high]),
            "z" => aabb([low, low, 0.0], [high, high, 1.0]),
            _ => aabb([low, 0.0, low], [high, 1.0, high]),
        }];
    }
    if name == "ladder" {
        // Against the block it is fixed to, behind it.
        let behind = opposite_face(facing.unwrap_or(2));
        return vec![side_slab(behind, px(3.0))];
    }
    if name.contains("carpet") {
        return vec![aabb([0.0; 3], [1.0, px(1.0), 1.0])];
    }
    if name == "snow_layer" {
        let height = int(state, "height").unwrap_or(0) as f32;
        if height == 0.0 {
            return Vec::new();
        }
        return vec![aabb([0.0; 3], [1.0, px(height * 2.0), 1.0])];
    }
    // Lower than a block, all the way across.
    let low = match name {
        "bed" | "straw_bed" => Some(px(9.0)),
        "enchanting_table" => Some(px(12.0)),
        "end_portal_frame" => Some(px(13.0)),
        "daylight_detector" | "daylight_detector_inverted" => Some(px(6.0)),
        "stonecutter_block" => Some(px(9.0)),
        "campfire" | "soul_campfire" => Some(px(7.0)),
        "farmland" | "grass_path" => Some(px(15.0)),
        "powered_repeater"
        | "unpowered_repeater"
        | "powered_comparator"
        | "unpowered_comparator" => Some(px(2.0)),
        _ => None,
    };
    if let Some(height) = low {
        return vec![aabb([0.0; 3], [1.0, height, 1.0])];
    }
    // Small things in the middle of their block.
    let small = match name {
        "lantern" | "soul_lantern" => Some((px(5.0), px(7.0))),
        "cake" => Some((px(1.0), px(8.0))),
        "flower_pot" => Some((px(5.0), px(6.0))),
        "sea_pickle" | "turtle_egg" | "sniffer_egg" => Some((px(4.0), px(7.0))),
        _ if name.ends_with("lantern") => Some((px(5.0), px(7.0))),
        _ if name.ends_with("candle") || name.ends_with("_candle") => Some((px(6.0), px(6.0))),
        _ if name.ends_with("_skull") || name.ends_with("_head") => Some((px(4.0), px(8.0))),
        _ if name.contains("amethyst") && !name.ends_with("_block") => Some((px(3.0), px(7.0))),
        "cactus" => Some((px(1.0), 1.0)),
        "bamboo" => Some((px(6.5), 1.0)),
        "chest" | "trapped_chest" | "ender_chest" => Some((px(1.0), px(14.0))),
        _ => None,
    };
    if let Some((inset, height)) = small {
        return vec![aabb(
            [inset, 0.0, inset],
            [1.0 - inset, height, 1.0 - inset],
        )];
    }
    if support::solid(&state.name) || name.ends_with("_stairs") {
        return vec![FULL];
    }
    Vec::new()
}

/// A slab `thickness` thick against face `face` of the block.
fn side_slab(face: u8, thickness: f32) -> Aabb {
    match face {
        0 => aabb([0.0; 3], [1.0, thickness, 1.0]),
        1 => aabb([0.0, 1.0 - thickness, 0.0], [1.0; 3]),
        2 => aabb([0.0; 3], [1.0, 1.0, thickness]),
        3 => aabb([0.0, 0.0, 1.0 - thickness], [1.0; 3]),
        4 => aabb([0.0; 3], [thickness, 1.0, 1.0]),
        _ => aabb([1.0 - thickness, 0.0, 0.0], [1.0; 3]),
    }
}

/// Whether a body standing on `feet`, `half_width` either side and `height`
/// tall, is inside a collision box of `state` at `pos`. Touching a face does
/// not count, so standing on a block is fine.
pub fn body_inside(
    feet: Vec3,
    half_width: f32,
    height: f32,
    pos: BlockPos,
    state: &BlockState,
) -> bool {
    let body_min = [feet.x - half_width, feet.y, feet.z - half_width];
    let body_max = [feet.x + half_width, feet.y + height, feet.z + half_width];
    let origin = [pos.x as f32, pos.y as f32, pos.z as f32];
    collision(state).iter().any(|block| {
        (0..3).all(|axis| {
            body_min[axis] < origin[axis] + block.max[axis]
                && body_max[axis] > origin[axis] + block.min[axis]
        })
    })
}

#[cfg(test)]
mod tests {
    use bedrockrs_protocol::block::StateValue;

    use super::*;
    use crate::blocks::palette;

    fn state(name: &str) -> BlockState {
        palette()
            .upgrade(&BlockState::new(format!("minecraft:{name}")))
            .unwrap()
    }

    const AT: BlockPos = BlockPos { x: 0, y: 0, z: 0 };

    fn inside(feet: (f32, f32), state: &BlockState) -> bool {
        let feet = Vec3 {
            x: feet.0,
            y: 0.0,
            z: feet.1,
        };
        body_inside(feet, 0.3, 1.8, AT, state)
    }

    #[test]
    fn bodies_share_blocks_with_what_they_miss() {
        // A player standing in the middle of the block.
        assert!(inside((0.5, 0.5), &state("stone")));
        assert!(!inside((0.5, 0.5), &state("torch")));
        assert!(
            !inside((0.5, 0.5), &state("wooden_door")),
            "doors go anywhere"
        );
        assert!(!inside((0.5, 0.5), &state("poppy")));
        assert!(inside((0.5, 0.5), &state("oak_fence")), "the post");
        // Standing at the block's edge, clear of a fence post or pane.
        assert!(!inside((0.05, 0.05), &state("oak_fence")));
        assert!(!inside((0.05, 0.05), &state("glass_pane")));
        assert!(!inside((0.05, 0.05), &state("iron_chain")));
        // A closed gate across the block, and a bottom trapdoor under the feet,
        // raised a little: standing on top of it is fine.
        assert!(inside((0.5, 0.5), &state("fence_gate")));
        let trapdoor = state("trapdoor");
        assert!(inside((0.5, 0.5), &trapdoor));
        let on_top = Vec3 {
            x: 0.5,
            y: px(3.0),
            z: 0.5,
        };
        assert!(!body_inside(on_top, 0.3, 1.8, AT, &trapdoor));
        // A top slab clears a short body under it.
        let mut slab = state("oak_slab");
        for (key, value) in slab.states.iter_mut() {
            if key == "minecraft:vertical_half" {
                *value = StateValue::String("top".into());
            }
        }
        let crawling = Vec3 {
            x: 0.5,
            y: 0.0,
            z: 0.5,
        };
        assert!(!body_inside(crawling, 0.3, 0.5, AT, &slab));
        assert!(inside((0.5, 0.5), &slab));
    }
}
