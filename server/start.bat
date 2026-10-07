@echo off
title BedrockRS
cd /d "%~dp0"

cargo run --release --manifest-path ..\Cargo.toml

rem Keep the window open only if the server failed, so the error can be read.
if errorlevel 1 pause
