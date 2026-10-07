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

-- Welcome everyone who spawns into the world.
server.on("player_join", function(player)
	server.broadcast(`§eWelcome to BedrockRS, {player.name}!`)
	player.send_message("§7Only you can see this. Try §f!kickme§7 or §f!wave§7.")
	newest = player.uuid
end)

server.on("player_quit", function(player)
	server.broadcast(`§e{player.name} left the game`)
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
	if message == "!kickme" then
		event.cancel()
		event.player.kick("You asked to be kicked. Come back any time!")
	elseif message == "!wave" then
		-- Looks the newest player up again, long after their join event.
		event.cancel()
		local target = if newest then server.player(newest) else nil
		if target then
			target.send_message(`§b{event.player.name} waves at you!`)
			event.player.send_message(`§7You waved at {target.name}.`)
		else
			event.player.send_message("§7Whoever joined last has left.")
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
