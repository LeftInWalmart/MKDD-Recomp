@echo off
rem Headless boot check: runs the recompiled game without a window for N seconds (default 90)
rem and reports whether it renders, keeps progressing, plays audio and uses the memory card.
rem The report is written to rust\preflight_report.txt. Uses a throwaway memory card.
cd /d "%~dp0rust"
set MKDD_MEMCARD=%TEMP%\mkdd_preflight_card.bin
del /q "%MKDD_MEMCARD%" 2>nul
cargo run -p mkdd-recomp --bin preflight -- %1
set MKDD_MEMCARD=
pause
