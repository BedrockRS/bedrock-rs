"""Generates src/core/data/blocks.json from Dragonfly's block palette.

Dragonfly (https://github.com/df-mc/dragonfly; MIT licence, Copyright (c) 2019
Dragonfly Tech, reproduced in src/core/data/README.md) ships
`server/world/block_states.nbt`: every vanilla block state of the release it
targets, as concatenated network NBT `{name, states, version}` compounds.
This script reads one commit of it and writes, for each block, its states and
the values each can take. The states of a block are exactly every combination
of those values.

It also writes `server/world/data_driven_blocks.nbt`: the vanilla blocks
Mojang defines in JSON (wool and concrete slabs and stairs, and others), whose
definitions the client needs in StartGame to know them at all. Each is its
name and its components as network NBT, base64.

Usage: python tools/block_data.py [commit]   (default: the commit below)
"""

import base64
import json
import sys
import urllib.request
from pathlib import Path

from item_data import NbtReader, NetworkWriter

# Dragonfly's "Support 1.26.50" commit (protocol 2193).
COMMIT = "4c7b5074be94fa83a1cd98e9c752083ad04a6e21"
URL = "https://raw.githubusercontent.com/df-mc/dragonfly/{commit}/{path}"
OUTPUT = Path(__file__).resolve().parent.parent / "src/core/data/blocks.json"


def main():
    commit = sys.argv[1] if len(sys.argv) > 1 else COMMIT
    with urllib.request.urlopen(URL.format(commit=commit, path="server/world/block_states.nbt")) as response:
        data = response.read()
    reader = NbtReader(data, network=True)
    blocks = {}
    count = 0
    while reader.pos < len(reader.data):
        compound = dict(reader.root()[1])
        name = compound["name"][1]
        states = blocks.setdefault(name, {})
        for state, (kind, value) in compound["states"][1]:
            if kind not in ("byte", "int", "string"):
                raise ValueError(f"unexpected state type {kind} for {name}")
            entry = states.setdefault(state, [kind, []])
            if value not in entry[1]:
                entry[1].append(value)
        count += 1
    for states in blocks.values():
        for entry in states.values():
            entry[1].sort(key=lambda value: (isinstance(value, str), value))

    with urllib.request.urlopen(URL.format(commit=commit, path="server/world/data_driven_blocks.nbt")) as response:
        # Disk (little-endian) NBT: {blocks: [{name, components}]}.
        root = NbtReader(response.read(), network=False).root()
    data_driven = []
    for block in dict(root[1])["blocks"][2]:
        block = dict(block[1])
        data_driven.append({
            "name": block["name"][1],
            "properties": base64.b64encode(NetworkWriter().root(block["components"])).decode(),
        })

    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with open(OUTPUT, "w", encoding="utf-8", newline="\n") as out:
        out.write("{\n")
        out.write(f'"source": {json.dumps(f"df-mc/dragonfly {commit[:12]} server/world/block_states.nbt and data_driven_blocks.nbt (MIT)")},\n')
        out.write('"blocks": {\n')
        rows = [
            f"{json.dumps(name)}:{json.dumps([[s, k, v] for s, (k, v) in sorted(states.items())], separators=(',', ':'))}"
            for name, states in sorted(blocks.items())
        ]
        out.write(",\n".join(rows))
        out.write("\n},\n")
        out.write('"data_driven": [\n')
        out.write(",\n".join(json.dumps(block, separators=(",", ":")) for block in data_driven))
        out.write("\n]\n}\n")
    print(f"{len(blocks)} blocks, {count} states, {len(data_driven)} data-driven definitions -> {OUTPUT}")


if __name__ == "__main__":
    main()
