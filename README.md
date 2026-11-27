# replay.nvim

## Install with lazy.nvim

Use Neovim 0.10 or later and a Rust toolchain to build the sidecar. For video export, install FFmpeg with `libx264` for MP4 or `ffv1` for MKV, and make sure `nvim` and `ffmpeg` are on your `PATH`.

```lua
return {
  "smit4k/replay.nvim",
  main = "replay",
  cmd = { "ReplayStart", "ReplayStop", "ReplayRender" },
  build = "cargo build --release --locked --manifest-path renderer/Cargo.toml --target-dir renderer/target",
  opts = {},
}
```

For a local checkout, replace `"smit4k/replay.nvim"` with `dir = "/path/to/replay.nvim"`, set `name = "replay.nvim"`, and run `:Lazy build replay.nvim`.

## Record and export

```vim
:ReplayStart session.nvimreplay
:ReplayStop
:ReplayRender session.nvimreplay session.mp4
```

In the same Neovim session, `:ReplayRender session.mp4` exports the most recently completed recording.

Video export defaults to 3840 × 2160 at 60 fps. Use `.mp4` for H.264 or `.mkv` for lossless FFV1. Wait for the save notification before closing Neovim. Existing output files are protected.

The renderer prefers an installed monospace Nerd Font. Set `opts = { render = { font = "JetBrainsMono Nerd Font Mono" } }` to pick an installed family, or use an absolute path to a font file. Set `render.scale = 1` for 1920 × 1080 output.

New recordings from Neovim's terminal UI capture the visible screen, including Blink completion menus and inline suggestions when they appear. Live screen capture needs Neovim's local RPC socket. Earlier recordings can still be exported using their saved editor state.

New recordings show keystrokes in the bottom-right corner of the exported video. Rapid presses share one line, such as `d i w`. Arrow keys use `← ↑ ↓ →`, and macOS modifiers use `⌘`, `⌥`, and `⌃`. The box shows up to eight recent presses and clears after one second of inactivity. Keystroke capture pauses in embedded terminals and masked prompts.

## Development

From the repository root, run `cargo test --locked --manifest-path renderer/Cargo.toml` for Rust and `make test` for Lua.
