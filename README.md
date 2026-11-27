# replay.nvim

Record a Neovim session to a `.nvimreplay` file, scrub through it in the editor, or export it as a video.

## Install with lazy.nvim

Use Neovim 0.10 or later and a Rust toolchain to build the sidecar. For video export, install FFmpeg with `libx264` for MP4 or `ffv1` for MKV, and make sure `nvim` and `ffmpeg` are on your `PATH`.

```lua
return {
  "smit4k/replay.nvim",
  main = "replay",
  cmd = { "ReplayStart", "ReplayStop", "ReplayOpen", "ReplayRender" },
  build = "cargo build --release --locked --target-dir target",
  opts = {},
}
```

For a local checkout, replace `"smit4k/replay.nvim"` with `dir = "/path/to/replay.nvim"`, set `name = "replay.nvim"`, and run `:Lazy build replay.nvim`.

## Record and replay

```vim
:ReplayStart demo.nvimreplay
:ReplayStop
:ReplayOpen demo.nvimreplay
:ReplayRender demo.mp4
```

In the replay tab, use **Left** and **Right** to step through frames, **Space** to play or pause, and **1–5** to change speed.

Video export shows the active buffer, cursor, and scroll position at 1080p and 60 fps. Use an `.mp4` or `.mkv` filename and wait for the save notification before closing Neovim. Existing output files are protected.
