"""Generates src/core/data/items.json from Dragonfly's vanilla data.

Dragonfly (https://github.com/df-mc/dragonfly, MIT) ships data dumped from
vanilla Bedrock servers. This script reads it at one commit, the same one
tools/block_data.py uses, and writes what BedrockRS needs in one file the
server embeds:

- every vanilla item (server/world/vanilla_items.nbt): name, network ID,
  version, whether it is component based (with its components as network
  NBT, base64), its largest stack, and for block items the block state it
  places, valid for the block palette (server/world/block_states.nbt);
- the creative inventory (server/item/creative/creative_items.nbt): groups
  (category, name, icon) and their items, in vanilla's order.

The data must match the client's version exactly: the client places creative
items by its own network IDs, so items from an older release land in the
wrong groups.

Creative entries carrying NBT (enchanted books, fireworks and the like) keep
it as the item's user data: little-endian NBT, base64.

Usage: python tools/item_data.py [commit]   (default: the commit below)
"""

import base64
import json
import struct
import sys
import urllib.request
from pathlib import Path

# Dragonfly's "Support 1.26.50" commit (protocol 2193).
DRAGONFLY = "4c7b5074be94fa83a1cd98e9c752083ad04a6e21"
DRAGONFLY_FILE = "https://raw.githubusercontent.com/df-mc/dragonfly/{commit}/{path}"
DRAGONFLY_STATES = "server/world/block_states.nbt"
DRAGONFLY_ITEMS = "server/world/vanilla_items.nbt"
DRAGONFLY_CREATIVE = "server/item/creative/creative_items.nbt"
OUTPUT = Path(__file__).resolve().parent.parent / "src/core/data/items.json"

# Creative inventory categories, as the protocol numbers them.
CATEGORIES = {"construction": 1, "nature": 2, "equipment": 3, "items": 4}

# Largest stacks of items that are not component based, which the data does
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


def fetch(commit, path):
    url = DRAGONFLY_FILE.format(commit=commit, path=path)
    with urllib.request.urlopen(url) as response:
        return response.read()


def read_nbt(commit, path):
    """A Dragonfly file's root tag, in network NBT."""
    return NbtReader(fetch(commit, path), network=True).root()


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

    def int32(self, value):
        """An int, or a length."""
        self.varint(value)

    def int64(self, value):
        self.varint(value)

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
        elif kind == "int":
            self.int32(tag[1])
        elif kind == "long":
            self.int64(tag[1])
        elif kind == "float":
            self.out += struct.pack("<f", tag[1])
        elif kind == "double":
            self.out += struct.pack("<d", tag[1])
        elif kind == "byte_array":
            self.int32(len(tag[1]))
            self.out += bytes(tag[1])
        elif kind == "string":
            self.string(tag[1])
        elif kind == "list":
            self.out.append(tag[1])
            self.int32(len(tag[2]))
            for element in tag[2]:
                self.payload(element)
        elif kind == "compound":
            for name, value in tag[1]:
                self.out.append(KIND_IDS[value[0]])
                self.string(name)
                self.payload(value)
            self.out.append(0)
        elif kind == "int_array":
            self.int32(len(tag[1]))
            for value in tag[1]:
                self.int32(value)
        elif kind == "long_array":
            self.int32(len(tag[1]))
            for value in tag[1]:
                self.int64(value)
        else:
            raise ValueError(kind)

    def root(self, compound):
        self.out.append(10)
        self.string("")
        self.payload(compound)
        return bytes(self.out)


class LittleEndianWriter(NetworkWriter):
    """Writes NBT in the little-endian (disk) flavour, as item user data
    carries it."""

    def int32(self, value):
        self.out += struct.pack("<i", value)

    def int64(self, value):
        self.out += struct.pack("<q", value)

    def string(self, value):
        data = value.encode("utf-8")
        self.out += struct.pack("<H", len(data))
        self.out += data


def states_of(compound):
    """Block states as [name, type, value] triples, in their stored order."""
    states = []
    for name, (kind, value) in compound[1]:
        if kind not in ("byte", "int", "string"):
            raise ValueError(f"unexpected state type {kind} for {name}")
        states.append([name, kind, value])
    return states


def load_palette(commit):
    """Every block's states and their values in Dragonfly's palette, and each
    block's first state, which is its default."""
    reader = NbtReader(fetch(commit, DRAGONFLY_STATES), network=True)
    palette = {}
    first_state = {}
    while reader.pos < len(reader.data):
        compound = dict(reader.root()[1])
        name = compound["name"][1]
        first_state.setdefault(name, states_of(compound["states"]))
        states = palette.setdefault(name, {})
        for state, (kind, value) in compound["states"][1]:
            entry = states.setdefault(state, [kind, []])
            if value not in entry[1]:
                entry[1].append(value)
    for states in palette.values():
        for entry in states.values():
            entry[1].sort(key=lambda value: (isinstance(value, str), value))
    return palette, first_state


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


def is_block_item(name, items, palette):
    """Whether an item places the block of the same name. Some blocks have an
    item of their own (`minecraft:item.bed` for the bed); doors and hanging
    signs share their block's name but are placed as items, as vanilla's
    block-to-item map has them."""
    if name not in palette or name == "minecraft:air":
        return False
    if name.replace("minecraft:", "minecraft:item.", 1) in items:
        return False
    door = name.endswith("_door") and not name.endswith("trapdoor")
    return not (door or name.endswith("_hanging_sign"))


def main():
    commit = sys.argv[1] if len(sys.argv) > 1 else DRAGONFLY
    palette, first_state = load_palette(commit)
    vanilla = {name: dict(entry[1]) for name, entry in read_nbt(commit, DRAGONFLY_ITEMS)[1]}
    creative_root = dict(read_nbt(commit, DRAGONFLY_CREATIVE)[1])
    creative_groups = [dict(group[1]) for group in creative_root["groups"][2]]
    creative_entries = [dict(entry[1]) for entry in creative_root["items"][2]]

    def block_for(item, properties=None):
        if not is_block_item(item, vanilla, palette):
            return None
        states = states_of(properties) if properties and properties[1] else first_state[item]
        return upgrade({"name": item, "states": states}, palette)

    # The block state the creative inventory shows first for each block item.
    creative_state = {}
    for entry in creative_entries:
        if "block_properties" in entry and "nbt" not in entry:
            creative_state.setdefault(entry["name"][1], entry["block_properties"])

    items = []
    for name, entry in sorted(vanilla.items(), key=lambda kv: kv[1]["runtime_id"][1]):
        item = {
            "name": name,
            "id": entry["runtime_id"][1],
            "version": entry["version"][1],
            "component_based": bool(entry["component_based"][1]),
        }
        # Items without components have an empty compound.
        components = entry.get("data")
        if components is not None and not components[1]:
            components = None
        if components is not None:
            item["components"] = base64.b64encode(NetworkWriter().root(components)).decode()
        item["max_stack"] = max_stack(name, components)
        block = block_for(name, creative_state.get(name))
        if block is not None:
            item["block"] = block
        items.append(item)

    groups = []
    for group in creative_groups:
        icon = dict(group["icon"][1])
        icon_name = icon.get("name", ("string", ""))[1]
        groups.append({
            "category": group["category"][1],
            "name": group["name"][1],
            "icon": {"item": icon_name, "block": block_for(icon_name, icon.get("block_properties"))}
            if icon_name else None,
        })

    creative = []
    for entry in creative_entries:
        name = entry["name"][1]
        row = {
            "item": name,
            "meta": entry.get("meta", ("short", 0))[1],
            "block": block_for(name, entry.get("block_properties")),
            "group": entry.get("group_index", ("int", 0))[1],
        }
        # Enchanted books, fireworks and the like: the item's user data, as
        # little-endian NBT (base64), the form the client reads it in.
        if "nbt" in entry and entry["nbt"][1]:
            row["nbt"] = base64.b64encode(LittleEndianWriter().root(entry["nbt"])).decode()
        creative.append(row)

    data = {
        "source": f"df-mc/dragonfly {commit[:12]} (MIT), generated by tools/item_data.py",
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
        f"({sum('nbt' in c for c in creative)} with NBT) -> {OUTPUT}"
    )


if __name__ == "__main__":
    main()
