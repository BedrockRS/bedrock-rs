// HelloJS: a BedrockRS plugin in modern JavaScript.
//
// Drop this folder into the server's plugins/ folder: the server runs it as
// it is, in its built-in JavaScript engine, and reloads it whenever a file in
// it is saved. There is nothing to install or build. hello-luau is the same
// plugin in Luau: both use the same API, line for line.
import { Logger, Server, Player } from "@bedrock-rs/core";

// ---- Top level ---------------------------------------------------------
// Runs once, when the plugin loads: plain setup only. The API (Logger,
// Server, Player) opens with on_load, so it can't be used here.

const BLOCKED_WORDS = ["badword"];
const TICKS_PER_SECOND = 20;
const TIP_EVERY = 5 * 60 * TICKS_PER_SECOND;
const TIPS = [
  "Type /hello to see what this plugin can do.",
  "Operators can make announcements with /hello admin announce.",
];

// What the plugin keeps track of while it runs.
const stats = { joins: 0, blocksPlaced: 0, blocksBroken: 0 };
let newest; // the UUID of whoever joined last
let tipTask; // the id of the recurring tip

// ---- on_load -----------------------------------------------------------
// Commands are registered here, and only here: the server learns every
// command before the plugin is enabled.
export function on_load() {
  Logger.info("Hello from JavaScript!");

  Server.registerCommand({
    name: "hello",
    description: "Friendly things to do",
    aliases: ["hi"],
    // /hello on its own. A string returned is the reply.
    run: () => "Hello! Try /hello wave, /hello to <player> or /hello kickme",
    subcommands: {
      // /hello wave
      wave: {
        description: "Wave at whoever joined last",
        run: (ctx) => {
          // ctx.sender is the player, or undefined when the console runs it.
          const from = ctx.sender?.name ?? "The console";
          const target = newest && Server.getPlayer(newest);
          if (!target) {
            ctx.error("Whoever joined last has left");
            return;
          }
          target.sendMessage(`§b${from} waves at you!`);
          ctx.reply(`You waved at ${target.name}`);
        },
      },
      // /hello to <player> [message]
      to: {
        description: "Say hello to another player",
        args: [
          { name: "player", type: "player" },
          { name: "message", type: "text", optional: true },
        ],
        run: (ctx) => {
          const from = ctx.sender?.name ?? "The console";
          const message = ctx.args.message ?? "Hello!";
          ctx.args.player.sendMessage(`§d${from} says: ${message}`);
          ctx.reply(`Said hello to ${ctx.args.player.name}`);
        },
      },
      // /hello kickme
      kickme: {
        description: "Leave with a friendly goodbye",
        run: (ctx) => {
          if (!(ctx.sender instanceof Player)) {
            ctx.error("Only players can be kicked");
            return;
          }
          ctx.sender.kick("You asked to be kicked. Come back any time!");
        },
      },
      // /hello admin announce <message>: only operators see and run these.
      admin: {
        permission: "operator",
        subcommands: {
          announce: {
            description: "Announce something to everyone",
            args: [{ name: "message", type: "text" }],
            run: (ctx) => {
              Server.broadcast(`§6[Announcement] ${ctx.args.message}`);
              return "Announced";
            },
          },
        },
      },
    },
  });
}

// ---- on_enable ---------------------------------------------------------
// From here on, events reach the plugin and scheduled tasks run.
export function on_enable() {
  // Everyone sees vanilla's "joined the game" message; the new player also
  // gets a hint only they can see. (event.cancel() here would stop vanilla's
  // message, to broadcast one of your own instead.)
  Server.on("player_join", (event) => {
    event.player.sendMessage("§7Only you can see this. Try §f/hello§7.");
    newest = event.player.uuid;
    stats.joins += 1;
  });

  Server.on("player_chat", (event) => {
    const message = event.message.toLowerCase();
    if (BLOCKED_WORDS.some((word) => message.includes(word))) {
      event.cancel();
      event.player.sendMessage("§cYour message was blocked.");
    }
  });

  Server.on("block_place", () => {
    stats.blocksPlaced += 1;
  });
  Server.on("block_break", (event) => {
    stats.blocksBroken += 1;
    const { x, y, z } = event.position;
    Logger.debug(`${event.player.name} broke ${event.block} at ${x} ${y} ${z}`);
  });

  // There is no per-tick hook: the world's scheduler runs code over time.
  const world = Server.getWorld();
  let next = 0;
  tipTask = world.runInterval(() => {
    Server.broadcast(`§e[Tip] ${TIPS[next]}`);
    next = (next + 1) % TIPS.length;
  }, TIP_EVERY);

  // waitTicks pauses an async function without holding up the server.
  world.run(async () => {
    await world.waitTicks(5 * TICKS_PER_SECOND);
    Logger.info(`Up for 5 seconds; ${Server.getPlayers().length} players online.`);
  });
}

// ---- on_disable --------------------------------------------------------
// Before the plugin is unloaded or reloaded, or the server stops: tidy up
// and save. (Tasks are stopped after this anyway; clearing is shown here.)
export function on_disable() {
  Server.getWorld().clearRun(tipTask);
  // Plugins have no storage API yet; this is where you would write to one.
  saveStats(stats);
}

function saveStats(current) {
  Logger.info(`Saving stats: ${JSON.stringify(current)}`);
}
