"""Generates src/core/data/items.json from PocketMine's BedrockData.

BedrockData (https://github.com/pmmp/BedrockData, CC0 1.0) is dumped from
vanilla Bedrock servers. This script reads one tagged release and writes what
BedrockRS needs, in one file the server embeds:

- every vanilla item: name, network ID, version, whether it is component based
  (with its components as network NBT, base64), its largest stack, and for
  block items the block state it places, upgraded to the client's states
  with Dragonfly's newer palette (see tools/block_data.py): BedrockData's
  1.26.30 states lack ones 1.26.50 added, such as stairs corners;
- the creative inventory: groups (category, name, icon) and their items.

Creative entries carrying NBT (enchanted books, fireworks and the like) are
left out until items carry user data.

Usage: python tools/item_data.py [tag]   (default: the tag below)
"""

import base64
import json
import struct
import sys
import urllib.request
from pathlib import Path

TAG = "bedrock-1.26.30"
# Dragonfly's "Support 1.26.50" commit (protocol 2193), for block states.
DRAGONFLY = "4c7b5074be94fa83a1cd98e9c752083ad04a6e21"
DRAGONFLY_STATES = "https://raw.githubusercontent.com/df-mc/dragonfly/{commit}/server/world/block_states.nbt"
BASE = "https://raw.githubusercontent.com/pmmp/BedrockData/{tag}/{path}"
OUTPUT = Path(__file__).resolve().parent.parent / "src/core/data/items.json"

# Creative inventory categories, as the protocol numbers them.
CATEGORIES = {"construction": 1, "nature": 2, "equipment": 3, "items": 4}

# Largest stacks of items that are not component based, which BedrockData does
# not record. Everything else stacks to 64.
STACK_OF_1_SUFFIXES = (
    "_sword", "_shovel", "_pickaxe", "_axe", "_hoe", "_spear",
    "_helmet", "_chestplate", "_leggings", "_boots", "_horse_armor",
    "_boat", "_chest_boat", "_raft", "_chest_raft", "_minecart",
    "_bucket", "_stew", "_soup", "_potion", "music_disc", "_bundle",
)
STACK_OF_1 = {
    "minecraft:bow", "minecraft:crossbow", "minecraft:trident", "minecraft:shield",
    "minecraft:mace", "minecraft:fishing_rod", "minecraft:flint_and_steel",
    "minecraft:shears", "minecraft:carrot_on_a_stick",
    "minecraft:warped_fungus_on_a_stick", "minecraft:elytra",
    "minecraft:totem_of_undying", "minecraft:saddle", "minecraft:minecart",
    "minecraft:potion", "minecraft:cake", "minecraft:bed",
    "minecraft:writable_book", "minecraft:written_book",
    "minecraft:enchanted_book", "minecraft:spyglass", "minecraft:brush",
    "minecraft:goat_horn", "minecraft:bundle", "minecraft:wolf_armor",
}
STACK_OF_16_SUFFIXES = ("_sign", "_hanging_sign", "_banner", "_egg")
STACK_OF_16 = {
    "minecraft:ender_pearl", "minecraft:snowball", "minecraft:egg",
    "minecraft:banner", "minecraft:armor_stand", "minecraft:bucket",
    "minecraft:honey_bottle",
}
# Spawn eggs end in _egg but stack to 64.
STACK_OF_64_SUFFIXES = ("_spawn_egg",)


def fetch(tag, path):
    url = BASE.format(tag=tag, path=path)
    with urllib.request.urlopen(url) as response:
        return response.read()


class NbtReader:
    """Reads NBT in the little-endian (disk) or network (varint) flavour."""

    def __init__(self, data, network):
        self.data = data
        self.pos = 0
        self.network = network

    def take(self, n):
        chunk = self.data[self.pos:self.pos + n]
        if len(chunk) != n:
            raise ValueError("NBT ends early")
        self.pos += n
        return chunk

    def varuint(self):
        value = shift = 0
        while True:
            byte = self.take(1)[0]
            value |= (byte & 0x7F) << shift
            if not byte & 0x80:
                return value
            shift += 7

    def varint(self):
        value = self.varuint()
        return (value >> 1) ^ -(value & 1)

    def string(self):
        n = self.varuint() if self.network else struct.unpack("<H", self.take(2))[0]
        return self.take(n).decode("utf-8")

    def int(self):
        return self.varint() if self.network else struct.unpack("<i", self.take(4))[0]

    def long(self):
        return self.varint() if self.network else struct.unpack("<q", self.take(8))[0]

    def payload(self, kind):
        if kind == 1:
            return ("byte", struct.unpack("<b", self.take(1))[0])
        if kind == 2:
            return ("short", struct.unpack("<h", self.take(2))[0])
        if kind == 3:
            return ("int", self.int())
        if kind == 4:
            return ("long", self.long())
        if kind == 5:
            return ("float", struct.unpack("<f", self.take(4))[0])
        if kind == 6:
            return ("double", struct.unpack("<d", self.take(8))[0])
        if kind == 7:
            return ("byte_array", list(self.take(self.int())))
        if kind == 8:
            return ("string", self.string())
        if kind == 9:
            element = self.take(1)[0]
            return ("list", element, [self.payload(element) for _ in range(self.int())])
        if kind == 10:
            entries = []
            while True:
                tag = self.take(1)[0]
                if tag == 0:
                    return ("compound", entries)
                name = self.string()
                entries.append((name, self.payload(tag)))
        if kind == 11:
            return ("int_array", [self.int() for _ in range(self.int())])
        if kind == 12:
            return ("long_array", [self.long() for _ in range(self.int())])
        raise ValueError(f"unknown NBT tag {kind}")

    def root(self):
        kind = self.take(1)[0]
        self.string()
        return self.payload(kind)


KIND_IDS = {
    "byte": 1, "short": 2, "int": 3, "long": 4, "float": 5, "double": 6,
    "byte_array": 7, "string": 8, "list": 9, "compound": 10, "int_array": 11,
    "long_array": 12,
}


class NetworkWriter:
    """Writes NBT in the network flavour, as ItemRegistry carries components."""

    def __init__(self):
        self.out = bytearray()

    def varuint(self, value):
        while True:
            byte = value & 0x7F
            value >>= 7
            if value:
                self.out.append(byte | 0x80)
            else:
                self.out.append(byte)
                return

    def varint(self, value):
        self.varuint((value << 1) ^ (value >> 63) if value < 0 else value << 1)

    def string(self, value):
        data = value.encode("utf-8")
        self.varuint(len(data))
        self.out += data

    def payload(self, tag):
        kind = tag[0]
        if kind == "byte":
            self.out += struct.pack("<b", tag[1])
        elif kind == "short":
            self.out += struct.pack("<h", tag[1])
        elif kind in ("int", "long"):
            self.varint(tag[1])
        elif kind == "float":
            self.out += struct.pack("<f", tag[1])
        elif kind == "double":
            self.out += struct.pack("<d", tag[1])
        elif kind == "byte_array":
            self.varint(len(tag[1]))
            self.out += bytes(tag[1])
        elif kind == "string":
            self.string(tag[1])
        elif kind == "list":
            self.out.append(tag[1])
            self.varint(len(tag[2]))
            for element in tag[2]:
                self.payload(element)
        elif kind == "compound":
            for name, value in tag[1]:
                self.out.append(KIND_IDS[value[0]])
                self.string(name)
                self.payload(value)
            self.out.append(0)
        elif kind in ("int_array", "long_array"):
            self.varint(len(tag[1]))
            for value in tag[1]:
                self.varint(value)
        else:
            raise ValueError(kind)

    def root(self, compound):
        self.out.append(10)
        self.string("")
        self.payload(compound)
        return bytes(self.out)


def states_of(compound):
    """Block states as [name, type, value] triples, in their stored order."""
    states = []
    for name, (kind, value) in compound[1]:
        if kind not in ("byte", "int", "string"):
            raise ValueError(f"unexpected state type {kind} for {name}")
        states.append([name, kind, value])
    return states


def le_states(b64):
    return states_of(NbtReader(base64.b64decode(b64), network=False).root())


def load_palette(commit):
    """Every block's states and their values in Dragonfly's palette."""
    with urllib.request.urlopen(DRAGONFLY_STATES.format(commit=commit)) as response:
        reader = NbtReader(response.read(), network=True)
    palette = {}
    while reader.pos < len(reader.data):
        compound = dict(reader.root()[1])
        states = palette.setdefault(compound["name"][1], {})
        for state, (kind, value) in compound["states"][1]:
            entry = states.setdefault(state, [kind, []])
            if value not in entry[1]:
                entry[1].append(value)
    for states in palette.values():
        for entry in states.values():
            entry[1].sort(key=lambda value: (isinstance(value, str), value))
    return palette


def upgrade(block, palette):
    """A block state made valid for the palette, as bedrockrs_core::blocks does:
    unknown states dropped, missing ones given their default ("none" or
    "unknown" where possible, else the smallest value). None if the block is
    not in the palette."""
    states = palette.get(block["name"])
    if states is None:
        return None
    have = {name: (kind, value) for name, kind, value in block["states"]}
    upgraded = []
    for name, (kind, values) in sorted(states.items()):
        if name in have and have[name][1] in values:
            upgraded.append([name, kind, have[name][1]])
        else:
            default = next((v for v in ("none", "unknown") if v in values), values[0])
            upgraded.append([name, kind, default])
    return {"name": block["name"], "states": upgraded}


def max_stack(name, components):
    if components is not None:
        found = find(components, ["components", "item_properties", "max_stack_size"])
        if found is not None:
            return found
    if name.endswith(STACK_OF_64_SUFFIXES):
        return 64
    if name in STACK_OF_1 or name.endswith(STACK_OF_1_SUFFIXES):
        return 1
    if name in STACK_OF_16 or name.endswith(STACK_OF_16_SUFFIXES):
        return 16
    return 64


def find(compound, path):
    tag = compound
    for key in path:
        if tag[0] != "compound":
            return None
        tag = dict(tag[1]).get(key)
        if tag is None:
            return None
    return tag[1]


def main():
    tag = sys.argv[1] if len(sys.argv) > 1 else TAG
    palette = load_palette(DRAGONFLY)
    required = json.loads(fetch(tag, "required_item_list.json"))
    block_to_item = json.loads(fetch(tag, "block_id_to_item_id_map.json"))
    creative_files = {
        category: json.loads(fetch(tag, f"creative/{category}.json"))
        for category in CATEGORIES
    }

    # The first canonical state of every block, for block items the creative
    # inventory does not give a state for.
    canonical = NbtReader(fetch(tag, "canonical_block_states.nbt"), network=True)
    first_state = {}
    while canonical.pos < len(canonical.data):
        state = canonical.root()
        name = find(state, ["name"])
        if name not in first_state:
            first_state[name] = states_of(dict(state[1])["states"])

    # Block items: an item that places the block of the same name.
    item_blocks = {item: block for block, item in block_to_item.items() if item == block}

    def block_for(item, states_b64=None):
        block = item_blocks.get(item)
        if block is None:
            return None
        states = le_states(states_b64) if states_b64 else first_state.get(block)
        if states is None:
            return None
        return upgrade({"name": block, "states": states}, palette)

    # The block state the creative inventory shows first for each block item.
    creative_state = {}
    for groups in creative_files.values():
        for group in groups:
            for entry in group["items"]:
                if isinstance(entry, dict) and "block_states" in entry and "nbt" not in entry:
                    creative_state.setdefault(entry["name"], entry["block_states"])

    items = []
    for name, entry in sorted(required.items(), key=lambda kv: kv[1]["runtime_id"]):
        components = None
        item = {
            "name": name,
            "id": entry["runtime_id"],
            "version": entry["version"],
            "component_based": entry["component_based"],
        }
        if "component_nbt" in entry:
            components = NbtReader(base64.b64decode(entry["component_nbt"]), network=False).root()
            item["components"] = base64.b64encode(NetworkWriter().root(components)).decode()
        item["max_stack"] = max_stack(name, components)
        block = block_for(name, creative_state.get(name))
        if block is not None:
            item["block"] = block
        items.append(item)

    groups = []
    creative = []
    skipped = 0
    for category, category_groups in creative_files.items():
        for group in category_groups:
            icon = group.get("group_icon")
            if isinstance(icon, str):
                icon = {"item": icon}
            elif isinstance(icon, dict):
                icon = {"item": icon["name"], "block": block_for(icon["name"], icon.get("block_states"))}
            groups.append({"category": CATEGORIES[category], "name": group["group_name"], "icon": icon})
            for entry in group["items"]:
                if isinstance(entry, str):
                    entry = {"name": entry}
                if "nbt" in entry:
                    skipped += 1
                    continue
                creative.append({
                    "item": entry["name"],
                    "meta": entry.get("meta", 0),
                    "block": block_for(entry["name"], entry.get("block_states")),
                    "group": len(groups) - 1,
                })

    data = {
        "source": f"pmmp/BedrockData {tag} (CC0 1.0), block states from df-mc/dragonfly {DRAGONFLY[:12]} (MIT), generated by tools/item_data.py",
        "items": items,
        "creative_groups": groups,
        "creative_items": creative,
    }
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    # One entry per line keeps diffs readable.
    with open(OUTPUT, "w", encoding="utf-8", newline="\n") as out:
        out.write("{\n")
        out.write(f'"source": {json.dumps(data["source"])},\n')
        for key in ("items", "creative_groups", "creative_items"):
            out.write(f'"{key}": [\n')
            rows = [json.dumps(row, separators=(",", ":")) for row in data[key]]
            out.write(",\n".join(rows))
            out.write("\n]" + (",\n" if key != "creative_items" else "\n"))
        out.write("}\n")
    print(
        f"{len(items)} items ({sum('block' in i for i in items)} blocks), "
        f"{len(groups)} creative groups, {len(creative)} creative items "
        f"({skipped} with NBT skipped) -> {OUTPUT}"
    )


if __name__ == "__main__":
    main()
