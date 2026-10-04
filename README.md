# MKDD-Decomp — Mario Kart: Double Dash!! (GM4E01), Spreadsheet Method

The CSVs in `sheets/` are the **single source of truth** for the whole executable.
Everything else is a projection: `src/` (Ghidra C, for reading) and `rust/` (compilable
Rust, for running) are generated from the sheets and never edited by hand.

> **No game content in this repository.** Nintendo's code and data (the DOL sections, the
> disassembly / p-code sheet, raw byte and string sheets, the decompiled C and the generated
> recompiled crates) are not committed. They are regenerated from **your own legally dumped
> disc image** with the tools here; see [Building from a fresh clone](#building-from-a-fresh-clone).

```
your disc image (.ciso)
   │ tools/ciso_extract.py
   ▼
../MKDD/extracted/main.dol ──ghidra-cli import+analyze──► ../MKDD/ghidra_project/MKDD.gpr
   │ tools/ExportSheets.java, tools/ExportPcode.java (via `ghidra script run`)
   ▼
../MKDD/export/ ──tools/build_sheets.py──► sheets/*.csv ──► src/units/*.c   (readable C)
doldecomp/mkdd symbols ──tools/import_symbols.py──┘  │
                                                     └─tools/gen_rust.py──► rust/  (Bevy app)
```

## Sheets

| Sheet | Rows | Key | Contents |
|---|---|---|---|
| `meta.csv` | 8 | `key` | Game ID, entry point (`0x80003154`), BSS, DOL hash, toolchain |
| `sections.csv` | 26 | `name` | DOL sections + MMIO blocks |
| `functions.csv` | 13,490 | `addr` | name, size, section, **unit, status, owner**, fan-in/out, signature, C path, notes |
| `units.csv` | 868 | `unit` | Work partitions (641 named after real source files, rest `blk_<addr>` of ≤40 fns) with status roll-ups |
| `symbol_matches.csv` | 9,241 | `addr` | Names/source files ported from doldecomp/mkdd, with the debug-build address and match method |
| `hle.csv` | 91 | `name` | Functions replaced by high-level emulation in the Rust runtime (threads, alarms, DVD, PAD, DSP, memory card, ...) |
| `calls.csv` | 27,110 | `caller,callee` | Static call graph |
| `instructions.csv` | 740,738 | `addr` | Every instruction: owning function, bytes, disassembly, **raw p-code semantics** |
| `memory.csv` | 55,822 | `addr` | Every initialized byte of the DOL, 64 bytes/row hex |
| `registers.csv` | 2,220 | `name` | Gekko register file layout (offsets used by p-code `R` varnodes) |
| `strings.csv` | 3,205 | `addr` | Defined strings + referencing functions |
| `source_files.csv` | 21 | `string_addr` | Strings naming source files (JSystem `JKR*`, `RaceApp.cpp`, ...) |
| `globals.csv` | 13,093 | `addr` | Data labels, types, sizes, xref counts |
| `disc_files.csv` | 166 | `path` | Every file on the disc (assets) with offset and size |
| `perf_gaps.csv` | 26 | `id` | Gap analysis: every component that can cause stutter, hitches or freezes, with probe evidence before/after, status and fix |

`instructions.csv` + `memory.csv` + `registers.csv` + `meta.csv` fully describe the program;
that is what `gen_rust.py` consumes. P-code varnodes are `<space><hexoffset>:<size>`
with space `R` register, `U` temporary, `C` constant, `M` RAM.

### Names (`tools/import_symbols.py`)
9,309 of 13,490 functions (69%) carry real names and source files ported from the CC0
[doldecomp/mkdd](https://github.com/doldecomp/mkdd) project. That project targets the MarioClub
debug build, so its addresses differ. The importer aligns the two builds' function lists by size
sequence (7,600 matches in runs of identical sizes). It then pairs equal-length gaps between
matches positionally (1,641 more). All 30 cross-checks against `*.cpp` assert strings agree.
The remaining 4,181 `raw` functions are mostly game code that differs between the debug and
retail builds. `tools/CreateGapFunctions.java` added 2,239 functions that Ghidra's
auto-analysis skipped (code reached only through pointers).

### Status lifecycle (`functions.csv:status`)
`raw` (auto name `FUN_…`) → `named` → `typed` → `clean` → `verified`.
"What is left?" → the per-status columns in `units.csv`.

## Rust / Bevy (`rust/`)

| Crate | What |
|---|---|
| `gc-core` | Stable ABI for generated code: CPU context, p-code ops, inline MEM1 access, and the `Host` function table. The only crate the generated parts depend on |
| `gc-runtime` | The console behind `Host`: MMIO devices, scheduler, HLE, GX decoder, disc, pads, diagnostics. Editing it rebuilds only the small crates |
| `recomp/part_00..47` | **Generated.** One `pub fn f_<ADDR>(c, entry)` per function, from `instructions.csv`. Each function has one match arm per basic block, with dense block indices so the match compiles to a jump table. Straight-line code runs without dispatch. Block leaders are entries, branch/call targets, code addresses referenced from data (switch jump tables), and the instruction after an unconditional jump. Every prologue calls `c.enter()`, which delivers interrupts and routes `hle.csv` functions to `hle.rs` |
| `mkdd-recomp` | Dispatch table, memory image (`mem/*.bin` from `memory.csv`), boot (apploader-equivalent low memory, FST, BI2) |
| `mkdd-bevy` | Bevy 0.19 app: runs the game, draws its GX output, feeds keyboard/gamepad into `PADRead` |

Run with `play.bat`, or `cd rust && cargo run -p mkdd-bevy`. Run `preflight.bat` first to boot the game headlessly and get a pass/fail report (`rust/preflight_report.txt`). The disc image is read from
`MKDD_DISC` (default: the .ciso in Downloads/Emus/GAMECUBE). `.iso` and `.ciso` both work.

### Controls
Keyboard, mouse and gamepads all work at once. Gamepads go through Bevy/gilrs (XInput and HID on Windows), so Xbox, PlayStation and generic USB pads need no extra driver.

- Bindings are stored per user in `%APPDATA%\MKDD-Recomp\controls.json` (`~/.config/MKDD-Recomp` on Linux; override with `MKDD_CONTROLS`). Everyone who plays a shared copy keeps their own.
- On first run the file is created from the defaults. If a Dolphin profile exists at `%APPDATA%\Dolphin Emulator\Config\Profiles\GCPad\MKDD.ini`, its gamepad mapping is imported.
- **F2** opens the rebinding screen:
  - Up/Down selects a control.
  - Enter, then press any key, mouse button, pad button or stick direction, adds that binding.
  - Backspace clears the selected control.
  - R resets everything.
  - Every change is saved immediately.
- Source strings in the JSON: `Key:KeyX`, `Mouse:Left`, `MouseMove:X+`, `Pad:South`, `Axis:LeftStickX-`.
  - `MouseMove` turns mouse movement into a self-centring stick (tuned by `mouse_sensitivity` / `mouse_return`).
  - `Stick Modifier` scales the main stick by `modifier_scale`, like Dolphin's modifier.
- Default keys:
  - Stick: arrows. C-stick: IJKL. D-pad: TFGH.
  - A: X / left mouse button. B: Z / right mouse button. X: C. Y: V. Z: D. Start: Enter.
  - L: A. R: S / middle mouse button.
- F1 shows/hides the debug overlay (runtime statistics; hidden by default).

### Settings (P)
**P** opens the settings menu (Up/Down select, Left/Right change, P or Esc closes; the game is held neutral while it is open). Every change applies and saves immediately to `%APPDATA%\MKDD-Recomp\settings.json` (override with `MKDD_SETTINGS`), one file per user like the controls.
- Volume (0-150 %), window mode (windowed / borderless / exclusive fullscreen; **F11** or **Alt+Enter** toggle borderless any time), display gamma, VSync, anti-aliasing (internal resolution 1x-6x, default 4x) and the FPS counter (top right: the game's real speed, 60 is full speed).
- **Unlock all**: unlocks every hidden character, kart, cup and mode for the current session only (sets every secret bit of the game's save record, SystemRecord 0x803BC1C0). It is not remembered: press it again after each launch. Saves made meanwhile keep only the player's own progress (SystemFile::fetch/store are hooked in cheats.rs), so players who earn the unlocks normally are unaffected.
- The game runs at the console's steady 59.94 frames per second: its logic, physics and animation are locked to the video interrupt, as on the GameCube.

### Rendering (EFB emulation)
`mkdd-bevy/src/gxrender.rs` replays every decoded GX frame on an emulated embedded framebuffer: draws in submission order with their GX blend/Z state and scissor, all 8 texture maps and 8 texgens per material (incl. the dual-texture post transform used for environment mapping), and EFB copies with the copy format, half-size box filter, vertical copy filter, point-sampled Z copies and display gamma. Shaders: `gx_efb.wgsl` (TEV), `gx_copy.wgsl`, `gx_blit.wgsl`; `cargo test -p mkdd-bevy --bin mkdd-bevy` validates them.
- Each GX pixel state needs a GPU pipeline; known states are built on background threads at start-up and every state seen is remembered in `%APPDATA%\MKDD-Recomp\pipelines.json`, so none compiles mid-game twice.

### Hardware layer (`gc-runtime`)
| Module | What it does |
|---|---|
| `sched.rs` | Each OSThread runs on its own host thread, but only the one holding the token runs guest code. Priority scheduling matches the SDK. Interrupts are delivered at function entry when MSR.EE is set, using the game's own handlers from the OS table at `0x80003040`. VI retrace runs at 59.94 Hz, and alarms use the host clock. |
| `hle.rs` | The functions in `hle.csv`: thread API, alarms, DVD reads straight from the disc image, PADRead, display-list FIFO tracking, DSP/AI boot skips, OSReport/OSPanic logging. Hooks are installed at boot from `table.rs`, so editing `hle.csv` + `hle.rs` rebuilds only the small crates. |
| `hw.rs` | Register-level devices. PE draw-done and token interrupts, VI beam position and retrace status, PI reset switch and FIFO pointer, DSP mailboxes, ARAM DMA (with 16 MiB ARAM), DI read DMA, AI sample counter, EXI/SI transfers completing instantly, 16 KiB locked cache. |
| `gx.rs` | GX FIFO and display-list decoder. Handles vertex formats, position/normal matrices and projection, XF lighting (ambient, material, directional and spot lights), texgen matrices, and all texture formats including TLUT and CMPR, with a cache. Each batch carries its full TEV state (16 stages, 4 texture slots, 2 UV sets, registers, konst colours, swap tables and alpha compare), which `mkdd-bevy/src/gx_tev.wgsl` evaluates per pixel. Batches are published on each display copy. |
| `dsp.rs` | JAudio DSP microcode HLE. Handles the setup-table and sync-frame commands. Mixes the 64 channels with AFC 4/2-bit ADPCM and 8/16-bit PCM from ARAM, plus pitch, loops, main-bus mixers and the auto-mixer. Each sub-frame is reported back with the real `0xF355FF00` mail and a DSP interrupt. |
| `audio.rs` | AI DMA output: each buffer is handed to the host in real time at 32/48 kHz. `mkdd-bevy` plays it through `cpal`. |
| `memcard.rs` | CARD API for slot A (a 59-block card; slot B is empty), saved to `memcard/slotA.bin`, or the path in `MKDD_MEMCARD`. Covers create, open, read, write, delete, rename, status with icon/banner offsets, and format. |
| `diag.rs` | Hot-function counters, the log, and a watchdog that writes `mkdd_trace.log` every 5 s. |
| `probe.rs` | Performance probe (below). |
| `thp.rs` | Native THP video frame decoder (HLE of `THPVideoDecode`). |

### Performance probe
`MKDD_PROBE=1` appends a report to `mkdd_probe.txt` every 5 s. It includes:
- Guest frame pacing: fps, interval mean / stdev ("choppiness") / p50 / p99 / max, hitches over 1.5, 2 and 3 VI periods, and an interval histogram.
- Host render pacing (app only).
- IRQ delivery latency per line.
- Host GX cost, broken down into material, vertices and emit, with draw and vertex counts.
- The longest stall (a thread holding the CPU token without entering a guest function).
- CPU share per guest thread, and a 2 kHz sampling profile of the last-entered guest function.

Runs:
- `MKDD_PROBE=1 MKDD_NO_INPUT=1 target/debug/preflight.exe 120` runs headless and watches the logo, the opening demo and the movie without skipping them.
- `MKDD_FAKE_PAD=1100` (hex button mask) holds those buttons on port 1, toggling every 30 reads. It is a scripted input check: Start/A should skip the intro movie.
- `MKDD_PROBE_SECS=60` makes the Bevy app probe itself, append a whole-run summary and exit.

Opt-in diagnostics:
- `MKDD_HEAP_CHECK=1` walks the JKRExpHeap free list on each allocation.
- `MKDD_ARQ_TRACE=1` checks every ARAM-queue DMA against its posted request.
- `MKDD_THP_NATIVE=0` switches back to the SDK THP decoder.

`sheets/perf_gaps.csv` records what the probe found and what was fixed.

### Status
- CPU: complete. All 739,975 instructions are translated mechanically from p-code, including
  paired-single quantized load/store.
- Hardware: boots through OSInit, VI/GX/DSP/AI/CARD init, intro movie threads and the J3D
  renderer. `preflight.bat` checks this headlessly.
- Approximations: up to 4 textures and 2 texture-coordinate sets per draw. There are no indirect textures (water distortion), fog or
  EFB-copy render targets (drawn untextured). Audio has no reverb/FX buses, filters or
  oscillator voices. There is no rumble.
- If it hangs, `mkdd_trace.log` shows the hottest functions and every thread's state. Fix by
  adding the stuck SDK function to `hle.csv`, writing a handler in `hle.rs`, and rerunning
  `gen_rust.py`. It only rewrites files whose content changed, so the 48 big crates aren't rebuilt.
- Known p-code quirk handled through HLE: Ghidra's `mtmsr` keeps the old MSR.EE bit. The recompiled
  `OSDisableInterrupts` therefore never disabled anything, so those functions (and `PPCMtmsr`)
  are implemented in `hle.rs`.
- Build profile: the generated parts compile at opt-level 3 with 16 codegen units and no incremental
  compilation (`MKDD_PART_OPT` in `gen_rust.py`; 1 builds faster but runs ~2-3x slower). Runtime crates
  are at opt-level 3. Debug assertions and overflow checks are off for the whole dev profile.
- Known p-code quirk handled in the generator: GameCubeLoader's `psq_l` spec scales with
  `INT_MULT` on doubles. The generator emits `float_mult` there.

## Rules for agents (Spreadsheet Method)

1. Claim a unit: set `owner` in `units.csv` and its rows in `functions.csv`. Touch only owned rows.
2. Look up interfaces in the sheets (`calls.csv`, `functions.csv:signature`, `globals.csv`).
   Never invent a symbol that has no row.
3. Rename and retype in Ghidra (`ghidra rename`, `ghidra type …`), then update `name`/`status`.
4. Regenerate (below). Hand-edited columns `name, unit, status, owner, notes` survive re-export.
5. One unit per change. The sheet diff is the review surface, not the generated code.

## Building from a fresh clone

The repository holds the pipeline, the runtime, the renderer, the Bevy app and the hand-edited
sheets. The rest is produced from your own disc:

1. Extract `main.dol` from your disc image:
   `python tools/ciso_extract.py "path/to/your disc.ciso" ../MKDD/extracted`.
2. Import and analyze it in Ghidra (GameCubeLoader) as project `MKDD` in `../MKDD/ghidra_project`,
   then run the commands under [Regenerate everything](#regenerate-everything). This recreates
   `sheets/instructions.csv`, `memory.csv`, `strings.csv`, `src/`, `rust/crates/recomp/`,
   `rust/crates/mkdd-recomp/mem/` and `rust/crates/mkdd-recomp/src/table.rs`.
3. Tell the game where the disc is: put the image path in `rust/disc.txt` (one line), set
   `MKDD_DISC`, or place the `.ciso` / `.iso` / `.gcm` in `rust/`.
4. Run `play.bat` (the first build takes about 30 minutes).

## Regenerate everything

```bash
G=path/to/ghidra-cli/ghidra.exe   # ghidra-cli v0.2.2
"$G" script run tools/ExportSheets.java --project MKDD --program main.dol -- ../MKDD/export
"$G" script run tools/ExportPcode.java  --project MKDD --program main.dol -- ../MKDD/export
python tools/build_sheets.py
python tools/import_symbols.py   # needs ../MKDD/community/MarioClub_us/{symbols,splits}.txt
python tools/build_sheets.py     # re-project C with the imported names
python tools/gen_rust.py
```

Build profile (set by `tools/gen_rust.py`): the 48 generated crates compile at `opt-level = 3` with 16 codegen units and no incremental cache, everything else at 3; debug info, debug assertions and overflow checks are off (`rust/target` stays around 2.5 GB). The game has to run faster than real time so its low-priority threads (the THP movie decoder, for example) get CPU time, the way they do on hardware.

Toolchain: Ghidra 12.1.4 + GameCubeLoader 1.3.0, Temurin JDK 21, ghidra-cli v0.2.2
(`%APPDATA%/ghidra-cli/config.yaml`),
Rust 1.98, Bevy 0.19.1.
