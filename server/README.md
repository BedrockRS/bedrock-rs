# server/

The folder BedrockRS runs in. Everything the server reads or writes at runtime lives here, separate from the source code in `src/`.

| Path | What it is | Tracked in git |
|---|---|---|
| `bedrockrs.toml` | Server configuration, created with defaults on first run | No |
| `plugins/` | Plugins, one folder each with a `plugin.json` | Yes |
| `worlds/` | Saved worlds; the default world is `worlds/world` | No |
| `keys/identity.pem` | The server's identity key, created on first run. Keep it private. | No |
| `start.bat` / `start.sh` | Build and start the server here | Yes |

## Starting the server

- **Windows:** double-click `start.bat`
- **Linux/macOS:** `./start.sh`

Both build a release binary on first run, which takes a few minutes, then start the server. Stop it with `Ctrl+C`; the world is saved on shutdown.

You can also run it from this folder yourself:

```bash
cargo run --release
```

The server finds its files relative to the folder it runs in, so always start it from here.
