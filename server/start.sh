#!/usr/bin/env sh
# Builds BedrockRS if needed and runs it inside this folder.
cd "$(dirname "$0")" || exit 1
exec cargo run --release --manifest-path ../Cargo.toml "$@"
