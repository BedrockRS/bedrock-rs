@echo off
title BedrockRS
cd /d "%~dp0"

cargo run --release --manifest-path ..\Cargo.toml

echo.
echo Server stopped.
pause
