# Contributing to BedrockRS

Thank you for your interest in contributing to **BedrockRS**!

BedrockRS is an open-source Minecraft: Bedrock Edition server written in Rust by **Mistvale Studios**. It is built from the ground up around the NetherNet transport and a sandboxed, hot-reloading plugin environment.

Because BedrockRS is still in early development, contributions of all kinds are welcome: fixing a bug, improving the codebase, researching the Bedrock protocol, adding a feature, improving documentation, or simply helping test the server.

## 📋 Before You Start

Before contributing, please:

1. Read the [README](README.md) and skim [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).
2. Check existing issues and pull requests to make sure your idea hasn't already been discussed.
3. For larger changes, open an issue first so the approach can be discussed before significant work begins.
4. Make sure your changes fit the project's goals and licensing.

BedrockRS is evolving rapidly, so APIs, internal systems and project structure may change without notice.

## 🛠️ Development Setup

### Requirements

* [Rust](https://rustup.rs) 1.93 or newer (with `rustfmt` and `clippy`)
* Git
* Minecraft: Bedrock Edition 26.51, for testing with a real client
* Python 3, only if you regenerate the vanilla data in `src/core/data`

### Clone and Build

```bash
git clone https://github.com/BedrockRS/bedrock-rs.git
cd bedrock-rs
cargo build
```

The optional JavaScript and Python plugin engines are behind features:

```bash
cargo build --features js,python
```

### Run

Run the server from the `server/` folder, which holds its configuration, worlds, plugins and keys:

```bash
cd server
cargo run
```

Or use `server/start.bat` / `server/start.sh`, which do the same with a release build.

## 🗂️ Where Things Go

| Path | What belongs there |
|---|---|
| `src/protocol` | Packet definitions, encoding, batching, NBT. No networking or game logic. |
| `src/net` | NetherNet signaling and WebRTC. Moves bytes; knows nothing about packets. |
| `src/plugins` | The plugin engines and the API exposed to scripts. |
| `src/core` | The game: tick loop, world, players, sessions, storage, and the binary. |
| `server/` | Runtime files only. Never put source code here. |
| `docs/` | Design notes. Update `ARCHITECTURE.md` when a design decision changes. |
| `tools/` | Scripts that generate `src/core/data`. |

`protocol`, `net` and `plugins` don't depend on each other; only `core` ties them together. Please keep it that way.

## 🧪 Testing Your Changes

Before opening a pull request, run:

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```

Depending on what you changed, also consider:

* Starting and stopping the server
* Connecting with a Bedrock Edition client
* Testing with two clients for anything other players can see
* Restarting the server to check that worlds and players persist
* Editing a plugin while the server runs to check hot reload
* Testing on Linux and Windows where applicable

For networking or protocol changes, testing with a real Bedrock client is strongly encouraged. If your change can't be tested yet because the systems it relies on don't exist, say so in your pull request.

## 🌱 Areas Where Contributions Are Welcome

* Protocol coverage and packet research
* NetherNet / WebRTC networking
* World generation and vanilla world (LevelDB) support
* Blocks, items, inventories and containers
* Entities, health, damage and survival gameplay
* The plugin API, and the JavaScript and Python engines
* Configuration and console commands
* Performance improvements
* Tests and documentation
* Bug fixes

See the milestones in the [README](README.md#️-milestones) for what's planned next. If you'd like to work on something that isn't listed, open an issue and let's discuss it.

## 🧱 Architecture and Design

BedrockRS is developed independently, not as a clone of another server.

Research and inspiration from other projects are welcome, but contributed code must be independently written and appropriately licensed. In particular:

* Do not copy implementation code from other server software.
* Mojang's `bedrock-protocol-docs` are "All rights reserved" under the Minecraft EULA. Use them as a reference only; never copy them into this repository.
* Generated data must credit its source in `src/core/data/README.md`.

## ✨ Code Style

* `cargo fmt` formatting and no `clippy` warnings.
* No `unsafe` code: the workspace forbids it.
* Keep code readable, simple, consistent with its surroundings, and focused on one responsibility.
* Prefer a simple implementation over an unnecessary abstraction.
* Errors are typed with `thiserror` in library code; `anyhow` belongs in the binary.
* Log with `tracing`. Routine activity goes at `debug`, so the console stays quiet by default.
* Write doc comments for public items, and comments that explain **why** when it isn't obvious.

## 🐛 Reporting Bugs

A good bug report includes:

* What happened, and what you expected
* Steps to reproduce
* BedrockRS version or commit
* Operating system
* Minecraft: Bedrock Edition version and platform
* Relevant console output (turn on `system_noise` in `bedrockrs.toml` for more detail)

For crashes, please include the full panic message and backtrace (`RUST_BACKTRACE=1`).

## 💡 Feature Requests

A useful feature request explains:

* What the feature would do
* Why it would be useful
* How it could work
* Whether it depends on unfinished systems

Some features may be postponed on purpose while the systems underneath are built.

## 🔀 Pull Requests

1. Keep each PR focused on one change or a closely related set of changes.
2. Explain what you changed and why.
3. Say how you tested it.
4. Link related issues.
5. Avoid unrelated formatting or refactoring.
6. Make sure formatting, clippy and tests pass.

Pull requests may be asked to change, be simplified, or be declined if they don't fit the project's current direction. This is especially likely for large architectural changes at this early stage.

## 📦 Dependencies

If your contribution needs a new dependency:

* Explain why it's needed.
* Prefer established, actively maintained crates.
* Add it to `[workspace.dependencies]` in the root `Cargo.toml` and use `.workspace = true` in the crate.
* Make sure its license is compatible with MIT.
* Avoid adding a dependency for something that can reasonably be written without one.

## 🤖 AI-Assisted Contributions

AI-assisted development is allowed, but you are responsible for the code you submit.

* Review generated code before submitting it, and make sure you understand it.
* Verify that it builds, passes tests and behaves correctly.
* Check it for copied or potentially copyrighted code.
* Don't submit large amounts of unreviewed generated code just because it compiles.

## 📜 Licensing

BedrockRS is licensed under the **MIT License**. By submitting a contribution, you agree that it may be distributed under that license. Don't submit code you don't have the right to contribute.

## 🌐 Minecraft and Mojang/Microsoft

BedrockRS is an independent project by Mistvale Studios. It is **not affiliated with, endorsed by, or sponsored by Mojang Studios or Microsoft**. Contributors should not present BedrockRS as an official Minecraft product.

## 🤝 Community

Please be respectful to other contributors. Constructive criticism and technical disagreement are a normal part of open source; personal attacks, harassment, discrimination and deliberately disruptive behaviour are not acceptable.

## 🚀 Final Notes

There's a lot left to build. If you're interested in Rust, networking, the Bedrock protocol, game servers or scripting engines, you're welcome here.

**Have fun, experiment, and help build BedrockRS!**
