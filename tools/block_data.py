"""Generates src/core/data/blocks.json from Dragonfly's block palette.

Dragonfly (https://github.com/df-mc/dragonfly; MIT licence, Copyright (c) 2019
Dragonfly Tech, reproduced in src/core/data/README.md) ships
`server/world/block_states.nbt`: every vanilla block state of the release it
targets, as concatenated network NBT `{name, states, version}` compounds.
This script reads one commit of it and writes, for each block, its states and
the values each can take. The states of a block are exactly every combination
of those values.

Usage: python tools/block_data.py [commit]   (default: the commit below)
"""

import json
import sys
import urllib.request
from pathlib import Path

from item_data import NbtReader

# Dragonfly's "Support 1.26.50" commit (protocol 2193).
COMMIT = "4c7b5074be94fa83a1cd98e9c752083ad04a6e21"
URL = "https://raw.githubusercontent.com/df-mc/dragonfly/{commit}/server/world/block_states.nbt"
OUTPUT = Path(__file__).resolve().parent.parent / "src/core/data/blocks.json"


def main():
    commit = sys.argv[1] if len(sys.argv) > 1 else COMMIT
    with urllib.request.urlopen(URL.format(commit=commit)) as response:
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

    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    with open(OUTPUT, "w", encoding="utf-8", newline="\n") as out:
        out.write("{\n")
        out.write(f'"source": {json.dumps(f"df-mc/dragonfly {commit[:12]} server/world/block_states.nbt (MIT)")},\n')
        out.write('"blocks": {\n')
        rows = [
            f"{json.dumps(name)}:{json.dumps([[s, k, v] for s, (k, v) in sorted(states.items())], separators=(',', ':'))}"
            for name, states in sorted(blocks.items())
        ]
        out.write(",\n".join(rows))
        out.write("\n}\n}\n")
    print(f"{len(blocks)} blocks, {count} states -> {OUTPUT}")


if __name__ == "__main__":
    main()
