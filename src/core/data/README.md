# Vanilla data

Generated files the server embeds. Do not edit them by hand: rerun the scripts.

| File | Script | Source |
|---|---|---|
| `items.json` | `tools/item_data.py` | [Dragonfly](https://github.com/df-mc/dragonfly) commit `4c7b5074be94` (1.26.50, protocol 2193), `server/world/vanilla_items.nbt` and `server/item/creative/creative_items.nbt`: items, network IDs, components, creative inventory. Block items' states from the same commit's palette. |
| `blocks.json` | `tools/block_data.py` | [Dragonfly](https://github.com/df-mc/dragonfly) commit `4c7b5074be94` (1.26.50, protocol 2193), `server/world/block_states.nbt`: every block state the client knows. |

Both scripts share `tools/item_data.py`'s NBT reader. Run them from `tools/`:

```
python item_data.py
python block_data.py
```

## Dragonfly's licence

`blocks.json` and `items.json` are derived from Dragonfly, whose licence requires this
notice:

```
MIT License

Copyright (c) 2019 Dragonfly Tech

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
