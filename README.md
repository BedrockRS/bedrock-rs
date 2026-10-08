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

BedrockRS is a high-performance Minecraft: Bedrock Edition dedicated server written in safe Rust. It speaks the modern **NetherNet** (WebRTC) transport and has a sandboxed, hot-reloading plugin environment: plugins are written in **Luau** or **JavaScript**, against one API. Both engines are built into the server, so a plugin is just its source files: nothing to install, nothing to build.

> [!IMPORTANT]
> BedrockRS is not affiliated with Mojang or Microsoft.

| | |
|---|---|
| Target | Minecraft Bedrock Edition **26.51**, network protocol **2193** |
| Transport | **NetherNet only** (WebRTC). RakNet is not implemented. |
| Language | Rust, edition 2024, MSRV 1.93, `unsafe_code = "forbid"`, tokio |
| Plugins | Hot-reloaded, with one API: Luau and JavaScript, both embedded |

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
- [x] Full 1.26.50 block palette with placement rules (facing, slabs, stairs, connections, tall walls)
- [x] Every block and block-placing item can be placed: doors, beds and tall flowers as two blocks, signs, seeds, redstone and the like
- [x] Support: ladders, torches, plants and the like need something to hold them, and pop off without it, a block a tick; vines and lichen face by face, scaffolding by stability, reaching out and climbing as in vanilla
- [x] Mojang's JSON-defined vanilla blocks (wool and concrete slabs and stairs, red shrub, shelf mushroom)
- [x] World persistence in vanilla's LevelDB format: vanilla worlds and `.mcworld` folders open as they are
- [x] Player persistence (position, rotation, flying state, inventory, game mode)
- [x] Per-player game modes: survival, creative, adventure and spectator

### Milestone 4: Inventories 🟡

- [x] Every vanilla 1.26.50 item and the full creative inventory, in vanilla's order
- [x] Items with NBT (enchanted books, fireworks, patterned banners), kept in inventories and saves
- [x] Armour: equipped from the inventory or by using it, with sounds, and shown to others
- [x] Pick block for every block (Ctrl+pick waits for block entities)
- [x] The inventory screen's crafting grid holds items (no crafting yet)
- [x] Server-owned player inventories (main, armour, offhand, cursor)
- [x] Item drops, item entities and held items shown to others
- [x] Falling blocks: sand, gravel, concrete powder, anvils and scaffolding fall and land
- [ ] Inventory protocol verified with a live client
- [ ] Containers (chests, furnaces, …)

### Milestone 5: Plugins 🟡

- [x] Luau sandbox with hot reload
- [x] `plugin.json` manifests
- [x] `on_load` / `on_enable` / `on_disable` lifecycle, the same in every engine
- [x] JavaScript engine (QuickJS, embedded) beside Luau, with ES modules and `async`/`await`
- [x] Events: `player_join`, `player_quit`, `player_chat` (cancellable), `block_break`, `block_place`, `player_damage` (cancellable), `player_death`, `player_respawn`
- [x] `Logger`, `Server` (`broadcast`, `getPlayer`, `getPlayers`, `on`, `registerCommand`) and `Player` (`sendMessage`, `kick`, `setGameMode`, `setHealth`, `damage`)
- [x] Scheduler: `run`, `runTimeout`, `runInterval`, `clearRun`, `waitTicks`
- [x] Slash commands with nested subcommands, typed arguments, aliases and permissions
- [ ] `player.give` and item events
- [ ] Cancellable block events
- [ ] Storage API

### Milestone 6: Survival 🟡

- [x] Health (`minecraft:health`), saved with the player
- [x] Fall damage and the void, with vanilla's numbers
- [x] Death screen, death messages, drops, and respawning at the world spawn
- [x] Regeneration (the world is peaceful for now)
- [x] Game rules: `dotiledrops`, `falldamage`, `keepinventory`, `naturalregeneration`, `showcoordinates`, `showdeathmessages`
- [ ] Difficulty, hunger and food
- [ ] Combat (PvP) and armour
- [x] Opening doors, trapdoors and fence gates
- [ ] Other block interactions
- [ ] Mobs and entity AI
- [ ] Vanilla biome data

### Milestone 7: Production ready ⬜

- [x] Slash commands for players and the server console
- [x] Permissions as vanilla's `permissions.json` (operator, member, visitor), by PlayFab ID; `/op`, `/deop`
- [ ] Movement validation (speed, teleport and breaking-time checks)
- [ ] Advertised public addresses for NAT'd deployments
- [x] Vanilla world import (LevelDB)
- [x] `level.dat`, so vanilla opens BedrockRS worlds (spawn and game rules kept there, as vanilla does)
- [ ] Target selectors beyond `@s` (`@a`, `@p`, …)
- [ ] More `server.properties` settings (server name, max players, …)
- [ ] Protocol 2216+ (Bedrock 26.60)

## 📁 Project structure

```text
bedrock-rs/
├── src/                  # the server software
│   ├── core/             # bedrockrs_core: game loop, world, entities, the `bedrockrs` binary
│   ├── net/              # bedrockrs_net: NetherNet signaling and WebRTC transport
│   ├── plugins/          # bedrockrs_plugins: sandboxed, hot-reloading plugin engine
│   └── protocol/         # bedrockrs_protocol: packet codec, batching and NBT
├── server/               # where the server runs; only the start scripts and README are tracked
│   ├── server.properties # configuration, vanilla's format (created on first run)
│   ├── permissions.json  # operators, members and visitors, by PlayFab ID (created on first run)
│   ├── plugins/          # plugins, one folder each (created on first run)
│   ├── worlds/           # saved worlds (created on first run)
│   └── keys/             # server identity key (created on first run)
├── docs/                 # architecture and design notes
├── examples/plugins/     # the same plugin in Luau and in JavaScript
├── packages/             # @bedrock-rs/core, the npm package of the JavaScript API
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

`server/server.properties` is created with every setting at its default on first run. It is vanilla's format, so a Bedrock Dedicated Server `server.properties` works as it is; properties BedrockRS does not use yet are ignored.

```properties
level-name=Bedrock level   # the world folder in worlds/
level-type=FLAT            # only FLAT so far
gamemode=creative          # for players joining for the first time
log-level=info             # BedrockRS only: error, warn, info, debug or trace
log-chat=true              # BedrockRS only: show chat in the console
```

Worlds are saved in vanilla's layout (`worlds/<level-name>/` with `level.dat`, `levelname.txt` and `db/`, a LevelDB database), so a vanilla world folder or an unzipped `.mcworld` copied into `server/worlds/` and named in `level-name` opens as it is, and a BedrockRS world zipped as a `.mcworld` opens in vanilla. Its name comes from its own `levelname.txt`; its spawn and game rules from its `level.dat`.

Network settings can be overridden with environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `BEDROCKRS_SIGNALING_ADDR` | `0.0.0.0:19132` | TCP address for HTTP signaling |
| `BEDROCKRS_MEDIA_PORT` | `19133` | UDP port for WebRTC traffic |
| `BEDROCKRS_MEDIA_IPS` | every IPv4 interface | Local addresses for WebRTC traffic |
| `BEDROCKRS_ADVERTISE_IPS` | none | Public addresses offered to clients |
| `BEDROCKRS_ICE_LITE` | `true` | `false` switches to full ICE |
| `BEDROCKRS_AUTHENTICATION` | `true` | `false` disables sign-in checks (offline testing only) |

## 🧩 Plugins

Each plugin is a folder in `server/plugins/` with a `plugin.json` and an entry file. Saving any file in the folder reloads the plugin while the server runs.

```json
{
  "name": "Welcome",
  "description": "Welcomes players",
  "version": "1.0.0",
  "author": "Mistvale Studios",
  "main": "main.luau"
}
```

`main` picks the engine:

| `main` | Engine |
|---|---|
| `.luau` | Luau, run in a sandbox. Nothing to install. |
| `.js`, `.mjs` | A JavaScript ES module, run by the server's built-in engine (QuickJS). Nothing to install. It can import its own files by relative path; editors get the API's types from the [`@bedrock-rs/core`](packages/bedrock-rs-core) package. |

Both engines have the same API, `@bedrock-rs/core`, and the same lifecycle. A plugin can export three functions, all optional:

- `on_load`: register commands (only here) and set up;
- `on_enable`: the plugin is live; events reach it and scheduled tasks run;
- `on_disable`: before it is unloaded, reloaded, or the server stops: tidy up and save.

There is no per-tick hook: the world's scheduler (`run`, `runTimeout`, `runInterval`, `clearRun`, `waitTicks`) runs code over time. The top level of the script is for plain setup; the API opens with `on_load`.

```js
// index.js
import { Server } from "@bedrock-rs/core";

export function on_enable() {
  // Replace vanilla's "joined the game" message with your own.
  Server.on("player_join", (event) => {
    event.cancel();
    Server.broadcast(`§a+ ${event.player.name}`);
  });
}
```

```lua
-- main.luau
local Server = require("@bedrock-rs/core").Server

return {
	on_enable = function()
		-- Replace vanilla's "joined the game" message with your own.
		Server.on("player_join", function(event)
			event.cancel()
			Server.broadcast(`§a+ {event.player.name}`)
		end)
	end,
}
```

Luau calls methods with `:` (`player:sendMessage(...)`, `world:runInterval(...)`); otherwise the two read line for line alike.

### Slash commands

Plugins add real slash commands, with subcommands nested as deep as you like. Players get them in `/help` and autocompleted as they type, and the server checks every argument before your code runs:

```js
export function on_load() {
  Server.registerCommand({
    name: "warp",
    description: "Travel between warps",
    aliases: ["w"],
    // /warp <name>
    args: [{ name: "name", type: "string" }],
    run: (ctx) => ctx.reply(`Warping to ${ctx.args.name}...`),
    subcommands: {
      // /warp set <name> [public]
      set: {
        description: "Make a warp where you stand",
        args: [
          { name: "name", type: "string" },
          { name: "public", type: "bool", optional: true },
        ],
        run: (ctx) => {
          if (!ctx.sender) return ctx.error("Only players can set warps.");
          ctx.reply(`Set warp ${ctx.args.name}.`);
        },
      },
      // /warp admin reload, for operators only
      admin: {
        permission: "operator",
        subcommands: {
          reload: { run: () => "Warps reloaded." },
        },
      },
    },
  });
}
```

Argument types are `string`, `text` (the rest of the line), `int`, `number`, `bool`, `player`, `gamemode` and `enum` (with `values`). Commands follow hot reload like everything else.

See [examples/plugins](examples/plugins) for a fuller plugin, written once in Luau and once in JavaScript.

## 🧱 Project goals

BedrockRS aims to be a fast, safe and extensible Bedrock server that is easy to run and easy to extend. The architecture is developed independently, using open specifications and reference projects for research only. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design, research and build log.

## 📚 Documentation

Guides for running a server and the full plugin API reference are in the [BedrockRS docs](https://github.com/BedrockRS/docs).

## 🤝 Contributing

Contributions are welcome! Please read [CONTRIBUTING.md](CONTRIBUTING.md) first.

## 📜 License

BedrockRS is licensed under the [MIT License](LICENSE). Generated vanilla data carries its own notices; see [src/core/data/README.md](src/core/data/README.md).
