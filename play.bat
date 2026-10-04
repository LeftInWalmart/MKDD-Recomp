@echo off
rem Launch the recompiled Mario Kart: Double Dash!! Bevy app (builds first if needed).
cd /d "%~dp0rust"
cargo run -p mkdd-bevy %*
if errorlevel 1 pause
