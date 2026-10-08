# server/

The folder BedrockRS runs in during development, separate from the source code in `src/`.

BedrockRS keeps everything it reads and writes in the folder it is started from, and creates what is missing on first run. So only the start scripts and this README are tracked in git; the rest of this folder is yours, and git ignores it. A released BedrockRS works the same way: run it from any folder and it sets that folder up.

| Path | What it is | Tracked in git |
|---|---|---|
| `start.bat` / `start.sh` | Build and start the server here | Yes |
| `README.md` | This file | Yes |
| `server.properties` | Server configuration in vanilla's format, created with defaults on first run | No |
| `permissions.json` | Who is an operator, member or visitor, in vanilla's format but by PlayFab ID (`pfid`) rather than XUID; created empty on first run. `/op` adds an entry and `/deop` removes it; edit it by hand for members and visitors, with the server stopped. Players without an entry get `default-player-permission-level` from `server.properties`. | No |
| `plugins/` | Plugins, one folder each with a `plugin.json`; created empty on first run. See `examples/plugins` for some to copy in. | No |
| `worlds/` | Saved worlds in vanilla's layout; `level-name` picks one (default `worlds/Bedrock level`). Vanilla worlds and unzipped `.mcworld` folders can be copied in. | No |
| `keys/identity.pem` | The server's identity key, created on first run. Keep it private. | No |

## Starting the server

- **Windows:** double-click `start.bat`
- **Linux/macOS:** `./start.sh`

Both build a release binary on first run, which takes a few minutes, then start the server. Stop it with `stop` or `Ctrl+C`; the world is saved on shutdown.

Commands can be typed straight into the server window. Type `help` for a list, and `op <your name>` once you have joined to make yourself an operator.

You can also run it from this folder yourself:

```bash
cargo run --release
```

The server finds its files relative to the folder it runs in, so always start it from here.
