# Project Plan: `replay.nvim` (Working Title)

**A ReplayMod-style deterministic session recorder, timeline scrubber, and offline video renderer for Neovim.**

---

## 1. Core Philosophy & Goals

- **Zero Runtime Overhead:** Recording an active coding session must never drop frames or stall Neovim's main event loop.
- **Tiny File Footprint:** Sessions are saved as compact, `zstd`-compressed binary event logs (`.nvimreplay`) rather than raw video or heavy JSON dumps.
- **Post-Production Freedom:** Once recorded, users can scrub timelines inside Neovim, adjust playback speed ramps, swap colorschemes/fonts, and render crisp 60fps+ `.mp4` or `.mkv` videos offline.

---

## 2. High-Level Architecture (Rust Sidecar + Thin Lua Client)

```text
┌─────────────────────────────────────────────────────────────────────────┐
│                             NEOVIM (Lua)                                │
│  • Event Hooks: nvim_buf_attach, CursorMoved, ModeChanged, vim.on_key   │
│  • Playback UI: Timeline scrubber float, keyframe markers, status HUD   │
│  • IPC Client: Non-blocking Msgpack-RPC over stdio / Unix Domain Socket │
└───────────────────────────────────┬─────────────────────────────────────┘
                                    │ Msgpack-RPC (Async / Fire-and-Forget)
                                    ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                          RUST SIDECAR DAEMON                            │
│  1. Recorder Engine: Lock-free MPSC queue -> Zstd frame writer          │
│  2. Replay State Machine: Keyframe index + instant state reconstruction │
│  3. Offline Renderer: Headless Nvim UI -> Software/GPU Grid Rasterizer  │
│  4. Video Encoder: Raw RGBA frame pipe -> FFmpeg (.mp4 / .mkv)          │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Binary Format Specification (`.nvimreplay`)

Designed for fast append-only writing during capture and O(1) seek/scrubbing during playback.

### File Layout

1. **Header (64 bytes, uncompressed):**
   - Magic bytes: `NVREP\0\0\1` (8 bytes)
   - Format version: `u16`
   - Initial Unix timestamp (ns): `u64`
   - Editor metadata offset & length (font, initial colorscheme, terminal dimensions)
   - Footer index offset: `u64` (written on `:ReplayStop`)
2. **Compressed Event Chunks (`zstd` frames):**
   - Each chunk spans ~2 seconds of active time or 512 events.
   - **Chunk Header (uncompressed):** `timestamp_offset_ns (u64)`, `compressed_size (u32)`, `uncompressed_size (u32)`, `is_keyframe (bool)`.
   - **Keyframe Chunks:** Contain a full snapshot of open buffers, cursor positions, and window layouts so seeking never requires replaying from `t = 0`.
   - **Delta Events (Msgpack / `bincode` encoded):**
     - `BufEnter / BufLeave` (buffer ID, file path, filetype)
     - `BufLines` (buffer ID, `firstline`, `lastline`, `new_lines` — captured via `nvim_buf_attach`)
     - `CursorMoved` (window ID, row, col)
     - `ModeChanged` (old mode, new mode)
     - `WinLayout` (splits, dimensions, scroll topline)
     - `KeyPress` (optional, captured via `vim.on_key` for keystroke HUD overlays)
3. **Seek Table Footer:**
   - Array of `(timestamp_ns, file_byte_offset)` pointing to every Keyframe Chunk for instant timeline scrubbing.

---

## 4. Component Breakdown

### A. Neovim Plugin (`lua/replay/`)

- `init.lua`: Public API (`setup()`, user commands `:ReplayStart`, `:ReplayStop`, `:ReplayOpen <file>`, `:ReplayRender`).
- `ipc.lua`: Spawns the Rust binary via `vim.uv.spawn` (communicating over a named pipe / Unix socket or stdio Msgpack-RPC) and manages lifecycle cleanup on `VimLeavePre`.
- `capture.lua`:
  - Attaches `nvim_buf_attach` to loaded buffers (`on_bytes` callback for minimal allocation overhead—avoids passing full line strings when possible).
  - Throttles high-frequency events (`CursorMoved`, `WinScrolled`) to 60Hz before dispatching to Rust.
- `scrubber.lua`:
  - Opens `.nvimreplay` in a dedicated read-only tabpage.
  - Renders a floating bottom timeline bar (`[====|---------] 01:24 / 12:05 [2.0x]`) with virtual text markers for key edits, mode changes, or user-placed bookmarks.
  - Supports Director Mode keyframes (speed multipliers, camera focus).

### B. Rust Sidecar (`src/`)

- `main.rs` & `rpc.rs`: Async runtime (`tokio`) listening for Msgpack-RPC requests/notifications from Neovim (`rmp-serde` / `rmpv`).
- `recorder.rs`:
  - Dedicated writer thread receiving events over a bounded channel (`crossbeam-channel` or `tokio::sync::mpsc`).
  - Periodically requests a full state snapshot from Lua (or maintains shadow ropes via `ropey`) to emit Keyframe Chunks.
- `player.rs`:
  - Memory-maps (`memmap2`) the `.nvimreplay` file.
  - Given target timestamp `T`, binary-searches the Seek Table for the nearest prior keyframe, decompresses only that chunk (`zstd`), and fast-forwards deltas up to `T`.
- `renderer/`:
  - **Option A (True Fidelity - Recommended):** Spawns a headless `nvim --embed` process in UI-attach mode (`nvim_ui_attach` with `ext_linegrid = true`), feeds the reconstructed buffer/cursor states into it so **real Tree-sitter highlighting, colorschemes, statuslines, and floating windows** are rendered natively by Neovim's own UI grid protocol.
  - **Rasterizer:** Reads Neovim's `grid_line` / `hl_attr_define` events and renders crisp glyphs onto an RGBA pixel buffer using `cosmic-text` + `tiny-skia` (CPU, zero GPU driver headaches) or `wgpu`. Adds smooth interpolated cursor trails (Neovide-style) and optional keycap HUDs.
- `encoder.rs`:
  - Spawns `ffmpeg` as a child process and streams raw RGBA frames into `stdin`:
    ```bash
    ffmpeg -y -f rawvideo -pix_fmt rgba -s 1920x1080 -r 60 -i - \
           -c:v libx264 -pix_fmt yuv420p -crf 18 output.mp4
    ```
  - Supports `.mp4` (`libx264` / `libx265`) and `.mkv` (`ffv1` lossless or `libx264`) presets.

---

## 5. Development Roadmap

### Phase 1: Capture & Binary Serialization (MVP Core)

- [ ] Scaffold Rust workspace + Lua plugin structure (`lazy.nvim` compatible).
- [ ] Implement Msgpack-RPC bridge between Lua and Rust sidecar.
- [ ] Hook `nvim_buf_attach` (`on_bytes`) and `CursorMoved` in Lua.
- [ ] Implement `zstd` chunk writer and Seek Table footer in Rust (`recording.nvimreplay`).
- [ ] **Goal:** Record a 10-minute session with <1% CPU overhead and verify file size.

### Phase 2: In-Editor Time-Travel Scrubber

- [ ] Implement `memmap2` + `zstd` chunk reader and state reconstructor in Rust.
- [ ] Build Lua floating timeline UI (`<Left>/<Right>` to step frames, `Space` to play/pause, `1-5` for playback speed).
- [ ] Apply reconstructed buffer states to a read-only preview buffer in Neovim.
- [ ] **Goal:** Scrub back and forth through a recorded coding session smoothly inside Neovim.

### Phase 3: Offline Grid Rasterizer & Video Export

- [ ] Integrate `nvim --embed` (`ext_linegrid`) to get exact Tree-sitter syntax colors and UI grid cells for every frame.
- [ ] Build RGBA frame rasterizer (`cosmic-text` + `tiny-skia`) supporting custom fonts, padding, and window shadows.
- [ ] Pipe frames to `ffmpeg` to output `.mp4` and `.mkv`.
- [ ] **Goal:** Run `:ReplayRender out.mp4` and get a clean 1080p 60fps video.

### Phase 4: "ReplayMod" Director Polish

- [ ] Add smooth exponential cursor interpolation (Neovide-style cursor smear) in the rasterizer.
- [ ] Add timeline speed-ramp keyframes (e.g., auto-fast-forward idle pauses, 4x boilerplate typing, 1x normal edits).
- [ ] Add optional keystroke overlay widget and audio/music track muxing flag.

---

## 6. Key Rust Crates

- **IPC & Serialization:** `rmp-serde`, `serde`, `tokio`
- **Compression & I/O:** `zstd`, `memmap2`, `ropey` (shadow buffer state tracking)
- **Text Shaping & 2D Rendering:** `cosmic-text`, `tiny-skia`, `fontdb`
- **CLI / Progress:** `indicatif` (progress reporting back to Neovim UI during video export)
