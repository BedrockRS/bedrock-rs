# @bedrock-rs/core

Types for the plugin API of [BedrockRS](https://github.com/BedrockRS/bedrock-rs), the Minecraft: Bedrock Edition server, for plugins written in JavaScript.

```js
import { Logger, Server, Player } from "@bedrock-rs/core";

export function on_load() {
  Server.registerCommand({ name: "ping", run: () => "Pong!" });
}

export function on_enable() {
  Server.getWorld().runInterval(() => Logger.info("Still here"), 20 * 60);
}

export function on_disable() {}
```

Put the plugin in its own folder in the server's `plugins/` folder, with a `plugin.json` whose `main` is your `index.js`. The server runs it as it is, in its built-in JavaScript engine, and reloads it whenever a file in the folder is saved: there is nothing to install or build. See `examples/plugins/hello-js` in the BedrockRS repository for a complete plugin.

## Do I need to install this?

Only for your editor. `npm install --save-dev @bedrock-rs/core` gives you the types (`index.d.ts`): completion, documentation, and type checking with `// @ts-check` or TypeScript. The module itself is built into the server; importing this package anywhere else throws.

## How it works

The server embeds [QuickJS](https://github.com/quickjs-ng/quickjs) (through the `rquickjs` crate) and gives every JavaScript plugin its own runtime, with its own memory and time limits. When your module imports `@bedrock-rs/core`, the server's module loader answers with a **native module**: `Logger`, `Server`, `Player` and `World` are objects whose functions are Rust closures bound straight into the engine. There is no bridge, no JSON and no WebAssembly in between:

- `Server.getWorld().runInterval(callback, 20)` calls a Rust function with your callback and `20`. It checks them, asks the server's scheduler for a task that repeats every 20 ticks, keeps `callback` under the task's id, and returns the id.
- Each tick, the server runs the tasks that are due by calling the callbacks it kept, then runs the promise jobs they queued.
- `await world.waitTicks(20)` returns a real Promise; the task the server runs 20 ticks later resolves it, and the rest of your function continues.

The Luau engine binds the same functions over the same Rust code, so a plugin behaves the same in either language.

Things to know:

- **The top level is for setup** (constants, maps, functions). The API opens with `on_load`; calling it earlier is an error that says so. Luau plugins follow the same rule.
- **Imports:** your own files, by relative path (`./util.js`, `../lib/index.js`, `./data.json`), and `@bedrock-rs/core`. npm packages can't be imported directly: bundle them into your plugin first.
- **Modern JavaScript** (classes, `async`/`await`, modules, optional chaining, …) runs as it is. TypeScript needs compiling to JavaScript first.
- There is no `setTimeout`, `fetch` or file access: time goes through the scheduler, and plugins reach the world only through the API.

## The API

- `Logger`: `trace`, `debug`, `info`, `warn`, `error`. `console.log` and friends write here too.
- `Server`:
  - `on(event, handler)`;
  - `broadcast(message)`;
  - `getPlayer(uuid)`;
  - `getPlayers()`;
  - `registerCommand(definition)`, only in `on_load`;
  - `getWorld()`.
- `Player`: `name`, `uuid`, `sendMessage`, `kick`, `setGameMode`, `setHealth`, `damage`.
- `World` (from `Server.getWorld()`): `run`, `runTimeout`, `runInterval`, `clearRun`, `waitTicks`.

The event names are `player_join`, `player_quit`, `player_chat`, `block_break`, `block_place`, `player_damage`, `player_death` and `player_respawn`. `index.d.ts` documents each.
