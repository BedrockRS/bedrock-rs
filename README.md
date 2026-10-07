<h1 align="center">BedrockRS</h1>

<p align="center"><b>Open source server software for Minecraft: Bedrock Edition written in Rust</b></p>
<p align="center">Built by <b>Mistvale Studios</b></p>

<p align="center">
  <img src="https://img.shields.io/github/license/BedrockRS/bedrock-rs?style=flat-square" alt="License">
  <img src="https://img.shields.io/github/stars/BedrockRS/bedrock-rs?style=flat-square" alt="GitHub Stars">
  <img src="https://img.shields.io/badge/Minecraft-Bedrock%2026.51-62B47A?style=flat-square" alt="Minecraft Bedrock 26.51">
  <img src="https://img.shields.io/badge/Protocol-2193-4A90D9?style=flat-square" alt="Protocol 2193">
  <img src="https://img.shields.io/badge/Rust-1.93%2B-CE422B?style=flat-square&logo=rust&logoColor=white" alt="Rust 1.93+">
  <img src="https://img.shields.io/badge/Transport-NetherNet-8E44AD?style=flat-square" alt="NetherNet">
</p>

# ℹ️ Information

BedrockRS is a high-performance Minecraft: Bedrock Edition dedicated server written in safe Rust. It speaks the modern **NetherNet** (WebRTC) transport and has a sandboxed, hot-reloading **Luau** plugin environment: plugins are plain script files, with no build step.

> [!IMPORTANT]
> BedrockRS is not affiliated with Mojang or Microsoft.

| | |
|---|---|
| Target | Minecraft Bedrock Edition **26.51**, network protocol **2193** |
| Transport | **NetherNet only** (WebRTC). RakNet is not implemented. |
| Language | Rust, edition 2024, MSRV 1.93, `unsafe_code = "forbid"`, tokio |
| Plugins | Zero-build, hot-reloaded scripts: Luau (JS/TS and Python planned) |

## 🚧 Current status

BedrockRS is in **early development**. A vanilla 26.51 client can join, build, chat, run commands and see other players, but many gameplay systems don't exist yet, and APIs may change significantly.

## 🗺️ Milestones

### Milestone 1: Connect ✅

- [x] Cargo workspace with four crates (`protocol`, `net`, `plugins`, `core`)
- [x] NetherNet signaling (`GET /v1/join`, `POST /v1/join/{networkId}`)
- [x] WebRTC data channels, P-384 identity assertion, message segmentation
- [x] Batch codec with zlib/snappy compression
- [x] Login handshake: NetworkSettings → Login → resource packs
- [x] Player authentication (login tokens verified against the Minecraft authorization service)

### Milestone 2: Spawn and play ✅

- [x] StartGame, hashed block network IDs, superflat chunks
- [x] 20 TPS game loop
- [x] Chunk streaming as players move
- [x] Players see each other move, with real skins
- [x] Vanilla movement speed and abilities

### Milestone 3: Build ✅

- [x] Block breaking and placing, with reach and overlap checks
- [x] Full 1.26.50 block palette with placement rules (facing, slabs, stairs, connections)
- [x] World persistence (compressed chunk files, versioned format)
- [x] Player persistence (position, rotation, flying state, inventory, game mode)
- [x] Per-player game modes: survival, creative, adventure and spectator

### Milestone 4: Inventories 🟡

- [x] Every vanilla item and the vanilla creative inventory
- [x] Server-owned player inventories (main, armour, offhand, cursor)
- [x] Item drops, item entities and held items shown to others
- [ ] Inventory protocol verified with a live client
- [ ] Containers (chests, furnaces, …)

### Milestone 5: Plugins 🟡

- [x] Luau sandbox with hot reload
- [x] `plugin.json` manifests
- [x] Events: `player_join`, `player_quit`, `player_chat` (cancellable), `block_break`, `block_place`, `player_damage` (cancellable), `player_death`, `player_respawn`
- [x] Player methods: `send_message`, `set_game_mode`, `set_health`, `damage`, `kick`; `server.broadcast`, `server.player(uuid)`
- [x] Slash commands with nested subcommands, typed arguments, aliases and permissions
- [ ] `player.give` and item events
- [ ] Cancellable block events
- [ ] JavaScript/TypeScript engine
- [ ] Python engine

### Milestone 6: Survival 🟡

- [x] Health (`minecraft:health`), saved with the player
- [x] Fall damage and the void, with vanilla's numbers
- [x] Death screen, death messages, drops, and respawning at the world spawn
- [x] Regeneration (the world is peaceful for now)
- [x] Game rules: `falldamage`, `keepinventory`, `naturalregeneration`, `showcoordinates`, `showdeathmessages`
- [ ] Difficulty, hunger and food
- [ ] Combat (PvP) and armour
- [ ] Block interactions
- [ ] Mobs and entity AI
- [ ] Vanilla biome data

### Milestone 7: Production ready ⬜

- [x] Slash commands for players and the server console
- [x] Operators (`/op`, `/deop`, `ops.json`)
- [ ] Movement validation (speed, teleport and breaking-time checks)
- [ ] Advertised public addresses for NAT'd deployments
- [ ] Vanilla world import (LevelDB)
- [ ] Target selectors beyond `@s` (`@a`, `@p`, …)
- [ ] More `bedrockrs.toml` settings (server name, max players, …)
- [ ] Protocol 2216+ (Bedrock 26.60)

## 📁 Project structure

```text
bedrock-rs/
├── src/                  # the server software
│   ├── core/             # bedrockrs_core: game loop, world, entities, the `bedrockrs` binary
│   ├── net/              # bedrockrs_net: NetherNet signaling and WebRTC transport
│   ├── plugins/          # bedrockrs_plugins: sandboxed, hot-reloading plugin engine
│   └── protocol/         # bedrockrs_protocol: packet codec, batching and NBT
├── server/               # where the server runs
│   ├── bedrockrs.toml    # configuration (created on first run)
│   ├── ops.json          # operators (created by the first /op)
│   ├── plugins/          # plugins, one folder each
│   ├── worlds/           # saved worlds
│   └── keys/             # server identity key (created on first run)
├── docs/                 # architecture and design notes
└── tools/                # scripts that generate the vanilla data in src/core/data
```

## 🚀 Getting started

You need [Rust](https://rustup.rs) 1.93 or newer.

```bash
git clone https://github.com/BedrockRS/bedrock-rs.git
cd bedrock-rs
cargo build --release
```

Start the server from the `server/` folder with `server/start.bat` (Windows) or `server/start.sh` (Linux/macOS). They build the server if needed and run it inside `server/`, so the configuration, worlds, plugins and keys all live there. See [server/README.md](server/README.md) for the details.

Then connect from Minecraft with your machine's IP address and port `19132`.

## 💬 Commands

Type commands into the server console (with or without the `/`) or in game. The console can run everything; in game, operator commands need an operator.

| Command | Who | What it does |
|---|---|---|
| `/help [command]` | everyone | Lists the commands you can use, or explains one |
| `/list` | everyone | Lists the players online |
| `/version` | everyone | Shows the server's version |
| `/gamemode <gameMode> [player]` | operators | Sets a game mode, as in vanilla: `survival`, `creative`, `adventure`, `spectator`, `default` (or `s`, `c`, `a`, `d`, or `0`, `1`, `2`) |
| `/op <player>` | operators | Makes an online player an operator |
| `/deop <player>` | operators | Takes away a player's operator status |
| `/gamerule [rule] [value]` | operators | Lists the game rules, or shows or sets one, e.g. `/gamerule keepinventory true` |
| `/kill [target]` | operators | Kills a player (yourself by default) |
| `/stop` | operators | Saves everything and stops the server |

To make yourself an operator, join the server and type `op <your name>` in the console.

## ⚙️ Configuration

`server/bedrockrs.toml` is created with every setting at its default on first run:

```toml
[logs]
chat = true                     # show chat in the console
system_noise = false            # show routine internal activity

[players]
default_game_mode = "creative"  # for players joining for the first time
```

Network settings can be overridden with environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `BEDROCKRS_SIGNALING_ADDR` | `0.0.0.0:19132` | TCP address for HTTP signaling |
| `BEDROCKRS_MEDIA_PORT` | `19133` | UDP port for WebRTC traffic |
| `BEDROCKRS_MEDIA_IPS` | every IPv4 interface | Local addresses for WebRTC traffic |
| `BEDROCKRS_ADVERTISE_IPS` | none | Public addresses offered to clients |
| `BEDROCKRS_ICE_LITE` | `true` | `false` switches to full ICE |
| `BEDROCKRS_WORLD_DIR` | `worlds/world` | Where the world is saved |
| `BEDROCKRS_AUTHENTICATION` | `true` | `false` disables sign-in checks (offline testing only) |

## 🧩 Plugins

Each plugin is a folder in `server/plugins/` with a `plugin.json` and an entry script. Saving a file reloads the plugin while the server runs.

```json
{
  "name": "hello",
  "description": "Welcomes players",
  "version": "1.0.0",
  "author": "Mistvale Studios",
  "main": "main.luau"
}
```

```lua
-- Replace vanilla's "joined the game" message with your own.
server.on("player_join", function(event)
	event.cancel()
	server.broadcast(`§a+ {event.player.name}`)
end)
```

### Slash commands

Plugins add real slash commands, with subcommands nested as deep as you like. Players get them in `/help` and autocompleted as they type, and the server checks every argument before your code runs:

```lua
server.command({
	name = "warp",
	description = "Travel between warps",
	aliases = { "w" },
	-- /warp <name>
	args = { { name = "name", type = "string" } },
	run = function(ctx)
		ctx.reply(`Warping to {ctx.args.name}...`)
	end,
	subcommands = {
		-- /warp set <name> [public]
		set = {
			description = "Make a warp where you stand",
			args = {
				{ name = "name", type = "string" },
				{ name = "public", type = "bool", optional = true },
			},
			run = function(ctx)
				if not ctx.sender then
					return ctx.error("Only players can set warps.")
				end
				ctx.reply(`Set warp {ctx.args.name}.`)
			end,
		},
		-- /warp admin reload, for operators only
		admin = {
			permission = "operator",
			subcommands = {
				reload = { run = function(ctx) return "Warps reloaded." end },
			},
		},
	},
})
```

Argument types are `string`, `text` (the rest of the line), `int`, `number`, `bool`, `player`, `gamemode` and `enum` (with `values = { ... }`). Commands follow hot reload like everything else.

See [server/plugins/hello](server/plugins/hello) for a fuller example.

## 🧱 Project goals

BedrockRS aims to be a fast, safe and extensible Bedrock server that is easy to run and easy to extend. The architecture is developed independently, using open specifications and reference projects for research only. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design, research and build log.

## 📚 Documentation

Guides for running a server and the full plugin API reference are in the [BedrockRS docs](https://github.com/BedrockRS/docs).

## 🤝 Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) first.

## 📜 License

BedrockRS is licensed under the [MIT License](LICENSE). Generated vanilla data carries its own notices; see [src/core/data/README.md](src/core/data/README.md).
