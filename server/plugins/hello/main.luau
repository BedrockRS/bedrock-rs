--!strict
-- Sample BedrockRS plugin. It runs when the server starts and again every time
-- a file in this folder is saved: edit it while the server is running to try it.
-- `plugin.json` next to it names the plugin and its entry script.

print("Hello from Luau!")
log.info(`hello loaded at {os.date("%H:%M:%S")} ({_VERSION})`)

-- Words nobody sees in chat. Messages containing one are cancelled.
local BLOCKED_WORDS = { "badword" }

-- The UUID of whoever joined most recently. `player.uuid` is the player's
-- persistent identity: use it, not the name, to remember players, and
-- `server.player(uuid)` to find them again while they are online.
local newest: string? = nil

-- Everyone sees vanilla's "joined the game" message; the new player also
-- gets a hint only they can see. (`event.cancel()` here would stop vanilla's
-- message, to broadcast one of your own instead.)
server.on("player_join", function(event)
	event.player.send_message("§7Only you can see this. Try §f/hello§7.")
	newest = event.player.uuid
end)

server.on("player_chat", function(event)
	local message = string.lower(event.message)
	for _, word in BLOCKED_WORDS do
		if string.find(message, word, 1, true) then
			event.cancel()
			event.player.send_message("§cYour message was blocked.")
			return
		end
	end
end)

server.on("block_break", function(event)
	local at = event.position
	log.info(`{event.player.name} broke {event.block} at {at.x} {at.y} {at.z}`)
end)

server.on("block_place", function(event)
	local at = event.position
	log.info(`{event.player.name} placed {event.block} at {at.x} {at.y} {at.z}`)
end)

-- A slash command with subcommands. Players see all of it in /help and get it
-- autocompleted as they type; the server checks the arguments before `run`
-- is called, so `ctx.args` always holds what each argument asks for.
server.command({
	name = "hello",
	description = "Friendly things to do",
	aliases = { "hi" },
	-- /hello on its own.
	run = function(ctx)
		return "Hello! Try /hello wave, /hello to <player> or /hello kickme."
	end,
	subcommands = {
		-- /hello wave
		wave = {
			description = "Wave at whoever joined last",
			run = function(ctx)
				-- `ctx.sender` is the player, or nil when the console runs it.
				local from = if ctx.sender then ctx.sender.name else "The console"
				local target = if newest then server.player(newest) else nil
				if not target then
					ctx.error("Whoever joined last has left.")
					return
				end
				target.send_message(`§b{from} waves at you!`)
				ctx.reply(`You waved at {target.name}.`)
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
				ctx.args.player.send_message(`§d{from} says: {message}`)
				ctx.reply(`Said hello to {ctx.args.player.name}.`)
			end,
		},
		-- /hello kickme
		kickme = {
			description = "Leave with a friendly goodbye",
			run = function(ctx)
				if not ctx.sender then
					ctx.error("Only players can be kicked.")
					return
				end
				ctx.sender.kick("You asked to be kicked. Come back any time!")
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
						server.broadcast(`§6[Announcement] {ctx.args.message}`)
						return "Announced."
					end,
				},
			},
		},
	},
})
