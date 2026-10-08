--!strict
-- HelloLuau: a BedrockRS plugin in Luau.
--
-- Drop this folder into the server's plugins/ folder; it reloads whenever a
-- file in it is saved. hello-js is the same plugin in JavaScript: both use
-- the same API, line for line (Luau calls methods with `:`).
local Core = require("@bedrock-rs/core")
local Logger, Server = Core.Logger, Core.Server

-- ---- Top level ----------------------------------------------------------
-- Runs once, when the plugin loads: plain setup only. The API (Logger,
-- Server, Player) opens with on_load, so it can't be used here.

local BLOCKED_WORDS = { "badword" }
local TICKS_PER_SECOND = 20
local TIP_EVERY = 5 * 60 * TICKS_PER_SECOND
local TIPS = {
	"Type /hello to see what this plugin can do.",
	"Operators can make announcements with /hello admin announce.",
}

-- What the plugin keeps track of while it runs.
local stats = { joins = 0, blocksPlaced = 0, blocksBroken = 0 }
local newest: string? = nil -- the UUID of whoever joined last
local tipTask: number? = nil -- the id of the recurring tip

local function saveStats(current)
	Logger.info(`Saving stats: joins {current.joins}, placed {current.blocksPlaced}, broken {current.blocksBroken}`)
end

local plugin = {}

-- ---- on_load ------------------------------------------------------------
-- Commands are registered here, and only here: the server learns every
-- command before the plugin is enabled.
function plugin.on_load()
	Logger.info("Hello from Luau!")

	Server.registerCommand({
		name = "hello",
		description = "Friendly things to do",
		aliases = { "hi" },
		-- /hello on its own. A string returned is the reply.
		run = function(ctx)
			return "Hello! Try /hello wave, /hello to <player> or /hello kickme"
		end,
		subcommands = {
			-- /hello wave
			wave = {
				description = "Wave at whoever joined last",
				run = function(ctx)
					-- `ctx.sender` is the player, or nil when the console runs it.
					local from = if ctx.sender then ctx.sender.name else "The console"
					local target = if newest then Server.getPlayer(newest) else nil
					if not target then
						ctx.error("Whoever joined last has left")
						return
					end
					target:sendMessage(`§b{from} waves at you!`)
					ctx.reply(`You waved at {target.name}`)
				end,
			},
			-- /hello to <player> [message]
			to = {
				description = "Say hello to another player",
				args = {
					{ name = "player", type = "player" },
					{ name = "message", type = "text", optional = true },
				},
				run = function(ctx)
					local from = if ctx.sender then ctx.sender.name else "The console"
					local message = ctx.args.message or "Hello!"
					ctx.args.player:sendMessage(`§d{from} says: {message}`)
					ctx.reply(`Said hello to {ctx.args.player.name}`)
				end,
			},
			-- /hello kickme
			kickme = {
				description = "Leave with a friendly goodbye",
				run = function(ctx)
					if not ctx.sender then
						ctx.error("Only players can be kicked")
						return
					end
					ctx.sender:kick("You asked to be kicked. Come back any time!")
				end,
			},
			-- /hello admin announce <message>: only operators see and run these.
			admin = {
				permission = "operator",
				subcommands = {
					announce = {
						description = "Announce something to everyone",
						args = { { name = "message", type = "text" } },
						run = function(ctx)
							Server.broadcast(`§6[Announcement] {ctx.args.message}`)
							return "Announced"
						end,
					},
				},
			},
		},
	})
end

-- ---- on_enable ----------------------------------------------------------
-- From here on, events reach the plugin and scheduled tasks run.
function plugin.on_enable()
	-- Everyone sees vanilla's "joined the game" message; the new player also
	-- gets a hint only they can see. (`event.cancel()` here would stop
	-- vanilla's message, to broadcast one of your own instead.)
	Server.on("player_join", function(event)
		event.player:sendMessage("§7Only you can see this. Try §f/hello§7.")
		newest = event.player.uuid
		stats.joins += 1
	end)

	Server.on("player_chat", function(event)
		local message = string.lower(event.message)
		for _, word in BLOCKED_WORDS do
			if string.find(message, word, 1, true) then
				event.cancel()
				event.player:sendMessage("§cYour message was blocked.")
				return
			end
		end
	end)

	Server.on("block_place", function()
		stats.blocksPlaced += 1
	end)
	Server.on("block_break", function(event)
		stats.blocksBroken += 1
		local at = event.position
		Logger.debug(`{event.player.name} broke {event.block} at {at.x} {at.y} {at.z}`)
	end)

	-- There is no per-tick hook: the world's scheduler runs code over time.
	local world = Server.getWorld()
	local nextTip = 1
	tipTask = world:runInterval(function()
		Server.broadcast(`§e[Tip] {TIPS[nextTip]}`)
		nextTip = nextTip % #TIPS + 1
	end, TIP_EVERY)

	-- waitTicks pauses this function without holding up the server.
	world:run(function()
		world:waitTicks(5 * TICKS_PER_SECOND)
		Logger.info(`Up for 5 seconds; {#Server.getPlayers()} players online.`)
	end)
end

-- ---- on_disable ---------------------------------------------------------
-- Before the plugin is unloaded or reloaded, or the server stops: tidy up
-- and save. (Tasks are stopped after this anyway; clearing is shown here.)
function plugin.on_disable()
	if tipTask then
		Server.getWorld():clearRun(tipTask)
	end
	-- Plugins have no storage API yet; this is where you would write to one.
	saveStats(stats)
end

return plugin
